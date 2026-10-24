package main

import (
	"context"
	"fmt"
	"net/url"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"testing"
)

const headSHA = "0123456789abcdef0123456789abcdef01234567"

type call struct {
	name string
	args []string
}

// fakeRunner answers each command by the first handler whose key prefixes "name arg1 arg2 ...".
type fakeRunner struct {
	mu       sync.Mutex
	handlers []handler
	calls    []call
}

type handler struct {
	prefix string
	answer func(ctx context.Context, args []string) ([]byte, error)
}

func (f *fakeRunner) on(prefix string, answer func(args []string) ([]byte, error)) {
	f.onCtx(prefix, func(_ context.Context, args []string) ([]byte, error) { return answer(args) })
}

// Handlers added later take precedence, so a test can override a fixture's answer.
func (f *fakeRunner) onCtx(prefix string, answer func(ctx context.Context, args []string) ([]byte, error)) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.handlers = append([]handler{{prefix, answer}}, f.handlers...)
}

func (f *fakeRunner) reply(prefix, out string) {
	f.on(prefix, func([]string) ([]byte, error) { return []byte(out), nil })
}

func (f *fakeRunner) Run(ctx context.Context, name string, args ...string) ([]byte, error) {
	f.mu.Lock()
	f.calls = append(f.calls, call{name, args})
	line := strings.Join(append([]string{name}, args...), " ")
	var found *handler
	for i := range f.handlers {
		if strings.HasPrefix(line, f.handlers[i].prefix) {
			found = &f.handlers[i]
			break
		}
	}
	f.mu.Unlock()
	if found == nil {
		return nil, fmt.Errorf("unexpected command: %s", line)
	}
	return found.answer(ctx, args)
}

func (f *fakeRunner) ran(prefix string) bool {
	f.mu.Lock()
	defer f.mu.Unlock()
	for _, c := range f.calls {
		if strings.HasPrefix(strings.Join(append([]string{c.name}, c.args...), " "), prefix) {
			return true
		}
	}
	return false
}

func testEnv(t *testing.T, skills ...string) Env {
	t.Helper()
	home := t.TempDir()
	codex := filepath.Join(home, ".codex")
	for _, s := range skills {
		dir := filepath.Join(codex, "skills", s)
		if err := os.MkdirAll(dir, 0o755); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(filepath.Join(dir, "SKILL.md"), []byte("# "+s), 0o644); err != nil {
			t.Fatal(err)
		}
	}
	return Env{Home: home, CodexHome: codex}
}

func choiceJSON(id, label, skill string) string {
	return fmt.Sprintf(`{"id":%q,"label":%q,"skill":%q,"prompt":"[${{.Skill}}]({{.SkillPath}}) {{.PR.URL}} {{.PR.HeadSHA}} {{.Worktree}}"}`, id, label, skill)
}

func configJSON(choices ...string) string {
	return `{"repositories":{"acme/app":"~/src/app"},"choices":[` + strings.Join(choices, ",") + `]}`
}

func mustConfig(t *testing.T, env Env, data string) *Config {
	t.Helper()
	cfg, err := parseConfig([]byte(data), env)
	if err != nil {
		t.Fatal(err)
	}
	return cfg
}

func TestConfigRejectsSettingsThatWouldMisbehave(t *testing.T) {
	env := testEnv(t, "adr-review", "code-review")
	adr := choiceJSON("adr", "ADRレビュー", "adr-review")
	cases := map[string]string{
		// Silently ignoring it would pretend a setting this version cannot apply.
		"unsupported key": `{"choices":[{"id":"adr","label":"ADR","skill":"adr-review","prompt":"x","model":"gpt"}]}`,
		// alerter splits actions on commas, and equal labels cannot be told apart.
		"comma in label":  configJSON(choiceJSON("adr", "ADR,レビュー", "adr-review")),
		"duplicate label": configJSON(adr, choiceJSON("code", "ADRレビュー", "code-review")),
		// Both would otherwise fail only after the user picked the choice.
		"missing skill":           configJSON(choiceJSON("adr", "ADR", "no-such-skill")),
		"unknown prompt variable": configJSON(`{"id":"adr","label":"ADR","skill":"adr-review","prompt":"{{.PR.Nope}}"}`),
	}
	for name, data := range cases {
		t.Run(name, func(t *testing.T) {
			if _, err := parseConfig([]byte(data), env); err == nil {
				t.Fatal("expected an error")
			}
		})
	}
}

func TestExampleConfigLoads(t *testing.T) {
	env := testEnv(t, "adr-review", "code-review")
	if _, err := loadConfig("config.example.json", env); err != nil {
		t.Fatal(err)
	}
}

func TestNotificationShowsChoicesInConfigOrderAndTheRequester(t *testing.T) {
	env := testEnv(t, "adr-review", "code-review", "ai-code-review")
	cfg := mustConfig(t, env, configJSON(
		choiceJSON("code", "コードレビュー", "code-review"),
		choiceJSON("ai", "AIレビュー", "ai-code-review"),
		choiceJSON("adr", "ADRレビュー", "adr-review"),
	))
	pr := PullRequest{Repo: "acme/app", Number: 7}
	args := alerterArgs(Notice{PR: pr, Requester: "tanaka"}, cfg.Choices, 60)
	if got := argValue(t, args, "--actions"); got != "コードレビュー,AIレビュー,ADRレビュー" {
		t.Errorf("--actions = %q", got)
	}
	if got := argValue(t, args, "--message"); got != "依頼者: @tanaka\nacme/app #7" {
		t.Errorf("--message = %q", got)
	}
	args = alerterArgs(Notice{PR: pr}, cfg.Choices, 60)
	if got := argValue(t, args, "--message"); !strings.HasPrefix(got, "依頼者不明\n") {
		t.Errorf("unknown requester: --message = %q", got)
	}
}

func argValue(t *testing.T, args []string, flag string) string {
	t.Helper()
	for i := 0; i+1 < len(args); i++ {
		if args[i] == flag {
			return args[i+1]
		}
	}
	t.Fatalf("%s missing in %q", flag, args)
	return ""
}

func TestParseSelection(t *testing.T) {
	choices := []Choice{{ID: "adr", Label: "ADRレビュー"}, {ID: "code", Label: "コードレビュー"}}
	cases := []struct {
		name    string
		out     string
		outcome Outcome
		id      string
		wantErr bool
	}{
		{name: "dropdown", out: `{"activationType":"actionClicked","activationValue":"コードレビュー","activationValueIndex":"1"}`, outcome: Chosen, id: "code"},
		{name: "single action", out: `{"activationType":"actionClicked","activationValue":"ADRレビュー"}`, outcome: Chosen, id: "adr"},
		{name: "decline", out: `{"activationType":"closed","activationValue":"見送る"}`, outcome: Declined},
		{name: "timeout", out: `{"activationType":"timeout"}`, outcome: TimedOut},
		{name: "body click", out: `{"activationType":"contentsClicked"}`, outcome: Dismissed},
		// An action that is not configured must never start some other review.
		{name: "unknown action", out: `{"activationType":"actionClicked","activationValue":"x"}`, wantErr: true},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			sel, err := parseSelection([]byte(c.out), choices)
			if c.wantErr {
				if err == nil {
					t.Fatalf("expected an error, got %s %+v", sel.Outcome, sel.Choice)
				}
				return
			}
			if err != nil {
				t.Fatal(err)
			}
			if sel.Outcome != c.outcome || (c.id == "") != (sel.Choice == nil) || (sel.Choice != nil && sel.Choice.ID != c.id) {
				t.Errorf("selection = %s %+v", sel.Outcome, sel.Choice)
			}
		})
	}
}

func TestDeepLinkEncodesPromptAndPath(t *testing.T) {
	prompt := "[$code-review](/x/SKILL.md)\nPR: https://github.com/a/b/pull/1?x=1&y=2#frag 100% +ok"
	path := "/Users/me/review worktrees/a&b"
	link := deepLink(prompt, path)
	if strings.ContainsAny(link[len("codex://new?"):], " \n#+") {
		t.Errorf("unescaped characters in %q", link)
	}
	u, err := url.Parse(link)
	if err != nil {
		t.Fatal(err)
	}
	if q := u.Query(); q.Get("prompt") != prompt || q.Get("path") != path {
		t.Errorf("round trip: prompt=%q path=%q", q.Get("prompt"), q.Get("path"))
	}
}

func TestRemoteRepo(t *testing.T) {
	cases := []struct {
		remote string
		want   string
		ok     bool
	}{
		{"git@github.com:acme/app.git\n", "acme/app", true},
		{"https://github.com/acme/app", "acme/app", true},
		{"ssh://git@github.com/acme/app.git", "acme/app", true},
		{"https://gitlab.com/acme/app.git", "", false},
	}
	for _, c := range cases {
		if got, ok := remoteRepo(c.remote); ok != c.ok || got != c.want {
			t.Errorf("%q: %q %v", c.remote, got, ok)
		}
	}
}

// gitRepo is a checkout of acme/app whose origin is served from a local bare repository,
// with the PR head reachable only through refs/pull/12/head as on GitHub.
type gitRepo struct {
	cfg      *Config
	checkout string
	pr       PullRequest
}

func git(t *testing.T, dir string, args ...string) string {
	t.Helper()
	cmd := exec.Command("git", append([]string{"-C", dir}, args...)...)
	out, err := cmd.CombinedOutput()
	if err != nil {
		t.Fatalf("git %v: %v\n%s", args, err, out)
	}
	return strings.TrimSpace(string(out))
}

func newGitRepo(t *testing.T) *gitRepo {
	if _, err := exec.LookPath("git"); err != nil {
		t.Skip("git not installed")
	}
	tmp := t.TempDir()
	t.Setenv("GIT_CONFIG_GLOBAL", filepath.Join(tmp, "gitconfig"))
	t.Setenv("GIT_CONFIG_NOSYSTEM", "1")
	t.Setenv("GIT_AUTHOR_NAME", "t")
	t.Setenv("GIT_AUTHOR_EMAIL", "t@example.com")
	t.Setenv("GIT_COMMITTER_NAME", "t")
	t.Setenv("GIT_COMMITTER_EMAIL", "t@example.com")

	bare := filepath.Join(tmp, "origin.git")
	git(t, tmp, "init", "-q", "--bare", "-b", "main", bare)
	seed := filepath.Join(tmp, "seed")
	git(t, tmp, "init", "-q", "-b", "main", seed)
	writeFile(t, filepath.Join(seed, "file.txt"), "base\n")
	git(t, seed, "add", ".")
	git(t, seed, "commit", "-q", "-m", "base")
	git(t, seed, "push", "-q", bare, "main")
	writeFile(t, filepath.Join(seed, "file.txt"), "change\n")
	git(t, seed, "commit", "-q", "-am", "change")
	git(t, seed, "push", "-q", bare, "HEAD:refs/pull/12/head")
	head := git(t, seed, "rev-parse", "HEAD")

	env := testEnv(t, "code-review")
	env.Home = tmp
	cfg := mustConfig(t, env, configJSON(choiceJSON("code", "コードレビュー", "code-review")))
	checkout := cfg.Repositories["acme/app"]
	git(t, tmp, "clone", "-q", bare, checkout)
	git(t, checkout, "remote", "set-url", "origin", "git@github.com:acme/app.git")
	git(t, checkout, "config", "url.file://"+bare+".insteadOf", "git@github.com:acme/app.git")
	return &gitRepo{cfg: cfg, checkout: checkout, pr: PullRequest{Repo: "acme/app", Number: 12, HeadSHA: head}}
}

func TestPrepareWorktreePinsThePRHeadAndLeavesTheCheckout(t *testing.T) {
	g := newGitRepo(t)
	before := git(t, g.checkout, "rev-parse", "HEAD")
	path, err := prepareWorktree(context.Background(), execRunner{}, "git", g.cfg, g.pr)
	if err != nil {
		t.Fatal(err)
	}
	if got := git(t, path, "rev-parse", "HEAD"); got != g.pr.HeadSHA {
		t.Errorf("worktree HEAD = %s", got)
	}
	if err := exec.Command("git", "-C", path, "symbolic-ref", "-q", "HEAD").Run(); err == nil {
		t.Error("the worktree must be detached")
	}
	if git(t, g.checkout, "rev-parse", "HEAD") != before || git(t, g.checkout, "branch", "--show-current") != "main" || git(t, g.checkout, "status", "--porcelain") != "" {
		t.Error("the checkout changed")
	}
}

func TestPrepareWorktreeNeverReusesOneWithChanges(t *testing.T) {
	g := newGitRepo(t)
	ctx := context.Background()
	first, err := prepareWorktree(ctx, execRunner{}, "git", g.cfg, g.pr)
	if err != nil {
		t.Fatal(err)
	}
	if again, err := prepareWorktree(ctx, execRunner{}, "git", g.cfg, g.pr); err != nil || again != first {
		t.Fatalf("a clean worktree must be reused: %q %v", again, err)
	}
	edited := filepath.Join(first, "file.txt")
	writeFile(t, edited, "left over\n")
	second, err := prepareWorktree(ctx, execRunner{}, "git", g.cfg, g.pr)
	if err != nil {
		t.Fatal(err)
	}
	if second == first || git(t, second, "status", "--porcelain") != "" {
		t.Errorf("got %q for a changed %q", second, first)
	}
	if data, _ := os.ReadFile(edited); string(data) != "left over\n" {
		t.Error("changes in the earlier worktree must be kept")
	}
}

func TestPrepareWorktreeRejectsAnotherRepository(t *testing.T) {
	g := newGitRepo(t)
	git(t, g.checkout, "remote", "set-url", "origin", "git@github.com:acme/other.git")
	if _, err := prepareWorktree(context.Background(), execRunner{}, "git", g.cfg, g.pr); err == nil {
		t.Fatal("expected an error")
	}
	if _, err := os.Stat(g.cfg.WorktreeRoot); !os.IsNotExist(err) {
		t.Error("no worktree may be created")
	}
}

func writeFile(t *testing.T, path, text string) {
	t.Helper()
	if err := os.WriteFile(path, []byte(text), 0o644); err != nil {
		t.Fatal(err)
	}
}
