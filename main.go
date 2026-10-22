package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"io"
	"log"
	"os"
	"os/signal"
	"os/user"
	"path/filepath"
	"sort"
	"syscall"
)

var errUsage = errors.New(`usage:
  review-notifier run [--config PATH]
  review-notifier status
  review-notifier check [--config PATH]
  review-notifier smoke --pr OWNER/REPO#N|URL [--requester LOGIN] [--no-open] [--config PATH]`)

func main() {
	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer stop()
	if err := run(ctx, os.Args[1:], os.Stdout); err != nil {
		fmt.Fprintln(os.Stderr, "review-notifier:", err)
		os.Exit(1)
	}
}

func run(ctx context.Context, args []string, stdout io.Writer) error {
	if len(args) == 0 {
		return errUsage
	}
	env, err := currentEnv()
	if err != nil {
		return err
	}
	switch args[0] {
	case "run":
		return runWatch(ctx, args[1:], env)
	case "status":
		return runStatus(env, stdout)
	case "check":
		return runCheck(ctx, args[1:], env, execRunner{}, stdout)
	case "smoke":
		return runSmoke(ctx, args[1:], env, execRunner{}, stdout)
	default:
		return errUsage
	}
}

func currentEnv() (Env, error) {
	home, err := os.UserHomeDir()
	if err != nil {
		return Env{}, err
	}
	codexHome := os.Getenv("CODEX_HOME")
	if !filepath.IsAbs(codexHome) {
		codexHome = filepath.Join(home, ".codex")
	}
	return Env{Home: home, CodexHome: codexHome}, nil
}

func loadTools(configPath string, env Env, r Runner) (Tools, error) {
	cfg, err := loadConfig(configPath, env)
	if err != nil {
		return Tools{}, err
	}
	cmds, err := resolveCommands(cfg.Commands, env)
	if err != nil {
		return Tools{}, err
	}
	return Tools{R: r, Cmds: cmds, Cfg: cfg}, nil
}

func runWatch(ctx context.Context, args []string, env Env) error {
	fs := flag.NewFlagSet("run", flag.ContinueOnError)
	configPath := fs.String("config", defaultConfigPath(env.Home), "config file")
	if err := fs.Parse(args); err != nil {
		return err
	}
	logger := log.New(os.Stderr, "", log.LstdFlags)
	t, err := loadTools(*configPath, env, execRunner{})
	if err != nil {
		return err
	}
	dir := defaultStateDir(env.Home)
	lock, err := lockDir(dir)
	if err != nil {
		return err
	}
	// Closing releases the lock; the process is ending either way.
	defer func() { _ = lock.Close() }()
	store := Store{path: filepath.Join(dir, "state.json")}
	st, _, err := store.Load()
	if err != nil {
		return err
	}
	logger.Printf("watching every %ds with %d choices", t.Cfg.PollIntervalSeconds, len(t.Cfg.Choices))
	err = newDaemon(t, store, st, logger).Run(ctx)
	logger.Printf("stopped")
	return err
}

func runStatus(env Env, stdout io.Writer) error {
	out := &printer{w: stdout}
	store := Store{path: filepath.Join(defaultStateDir(env.Home), "state.json")}
	st, ok, err := store.Load()
	if err != nil {
		return err
	}
	if !ok {
		out.printf("no state yet: %s\n", store.path)
		return out.err
	}
	out.printf("login: %s\nlast poll: %s\n", st.Login, st.LastPoll.Local().Format("2006-01-02 15:04:05"))
	if st.LastError != "" {
		out.printf("last error: %s\n", st.LastError)
	}
	reqs := make([]*Request, 0, len(st.Requests))
	for _, r := range st.Requests {
		if r.Status != StatusBaseline {
			reqs = append(reqs, r)
		}
	}
	sort.Slice(reqs, func(i, j int) bool { return reqs[i].UpdatedAt.After(reqs[j].UpdatedAt) })
	for i, r := range reqs {
		if i == 20 {
			out.printf("... %d more\n", len(reqs)-i)
			break
		}
		line := fmt.Sprintf("%s  %-9s %s#%d %s", r.UpdatedAt.Local().Format("01-02 15:04"), r.Status, r.Repo, r.Number, oneLine(r.Title))
		if r.Choice != "" {
			line += "  [" + r.Choice + "]"
		}
		if r.Error != "" {
			line += "  error: " + r.Error
		}
		out.printf("%s\n", line)
	}
	return out.err
}

// Checks the config, the commands and that the GitHub API answers the queries the watcher uses.
func runCheck(ctx context.Context, args []string, env Env, r Runner, stdout io.Writer) error {
	out := &printer{w: stdout}
	fs := flag.NewFlagSet("check", flag.ContinueOnError)
	configPath := fs.String("config", defaultConfigPath(env.Home), "config file")
	if err := fs.Parse(args); err != nil {
		return err
	}
	t, err := loadTools(*configPath, env, r)
	if err != nil {
		return err
	}
	c := t.Cmds
	out.printf("config: %s\n", *configPath)
	out.printf("commands: gh=%s git=%s alerter=%s open=%s osascript=%s\n", c.GH, c.Git, c.Alerter, c.Open, c.Osascript)
	out.printf("pollIntervalSeconds: %d\nworktreeRoot: %s\n", t.Cfg.PollIntervalSeconds, t.Cfg.WorktreeRoot)
	repos := make([]string, 0, len(t.Cfg.Repositories))
	for repo := range t.Cfg.Repositories {
		repos = append(repos, repo)
	}
	sort.Strings(repos)
	for _, repo := range repos {
		out.printf("repository: %s -> %s\n", repo, t.Cfg.Repositories[repo])
	}
	for _, ch := range t.Cfg.Choices {
		out.printf("choice: %s %q -> %s\n", ch.ID, ch.Label, ch.skillPath)
	}
	login, err := currentLogin(ctx, r, c.GH)
	if err != nil {
		return err
	}
	out.printf("github login: %s\n", login)
	for _, q := range requestQueries(t.Cfg.owners(), t.Cfg.Teams) {
		hits, err := searchPRs(ctx, r, c.GH, q)
		if err != nil {
			return err
		}
		out.printf("pending (%s): %d\n", q, len(hits))
		if len(hits) > 0 {
			if _, err := fetchEvents(ctx, r, c.GH, hits[0].Repo, hits[0].Number); err != nil {
				return fmt.Errorf("issue events of %s#%d: %w", hits[0].Repo, hits[0].Number, err)
			}
		}
	}
	return out.err
}

// Notification, selection, worktree and deep link for one given PR, without watching GitHub or keeping state.
func runSmoke(ctx context.Context, args []string, env Env, r Runner, stdout io.Writer) error {
	out := &printer{w: stdout}
	fs := flag.NewFlagSet("smoke", flag.ContinueOnError)
	configPath := fs.String("config", defaultConfigPath(env.Home), "config file")
	ref := fs.String("pr", "", "pull request as OWNER/REPO#N or URL")
	requester := fs.String("requester", "", "login shown as the requester")
	noOpen := fs.Bool("no-open", false, "print the deep link instead of opening it")
	if err := fs.Parse(args); err != nil {
		return err
	}
	repo, number, err := parsePRRef(*ref)
	if err != nil {
		return err
	}
	t, err := loadTools(*configPath, env, r)
	if err != nil {
		return err
	}

	pr, err := fetchPR(ctx, r, t.Cmds.GH, repo, number)
	if err != nil {
		return err
	}
	out.printf("pr: %s#%d %s head=%s\n", pr.Repo, pr.Number, pr.Title, pr.HeadSHA)

	notice := Notice{Key: fmt.Sprintf("smoke:%s#%d", pr.Repo, pr.Number), PR: pr, Requester: *requester}
	sel, err := notify(ctx, r, t.Cmds.Alerter, notice, t.Cfg.Choices, t.Cfg.NotificationTimeoutSeconds)
	if err != nil {
		return err
	}
	out.printf("selection: %s\n", sel.Outcome)
	if sel.Outcome != Chosen {
		return out.err
	}
	out.printf("choice: %s (%s)\n", sel.Choice.ID, sel.Choice.Skill)

	if *noOpen {
		// The steps of launch, stopping before the App is opened.
		path, err := prepareWorktree(ctx, r, t.Cmds.Git, t.Cfg, pr)
		if err != nil {
			return err
		}
		prompt, err := sel.Choice.render(PromptData{PR: pr, Requester: *requester, Worktree: path})
		if err != nil {
			return err
		}
		out.printf("worktree: %s\nlink: %s\n", path, deepLink(prompt, path))
		return out.err
	}
	res, err := launch(ctx, t, sel.Choice, pr.Repo, pr.Number, *requester)
	if res.Worktree != "" {
		out.printf("worktree: %s\n", res.Worktree)
	}
	if err != nil {
		return err
	}
	out.printf("%s\n", "opened: send the prompt in the new Codex chat to start the review")
	return out.err
}

func resolveCommands(c Commands, env Env) (Commands, error) {
	name := os.Getenv("USER")
	if u, err := user.Current(); err == nil {
		name = u.Username
	}
	dirs := searchDirs(env.Home, name)
	out := c
	for _, item := range []struct {
		name string
		path *string
	}{{"gh", &out.GH}, {"git", &out.Git}, {"alerter", &out.Alerter}, {"open", &out.Open}, {"osascript", &out.Osascript}} {
		if *item.path != "" {
			continue
		}
		found, err := findCommand(item.name, dirs)
		if err != nil {
			return Commands{}, err
		}
		*item.path = found
	}
	return out, nil
}

// Keeps the first write error, so a command reports a failed stdout once at its end.
type printer struct {
	w   io.Writer
	err error
}

func (p *printer) printf(format string, args ...any) {
	if p.err == nil {
		_, p.err = fmt.Fprintf(p.w, format, args...)
	}
}
