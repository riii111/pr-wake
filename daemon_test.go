package main

import (
	"context"
	"fmt"
	"io"
	"log"
	"net/url"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"testing"
	"time"
)

func searchJSON(items ...string) string {
	return `{"incomplete_results":false,"items":[` + strings.Join(items, ",") + `]}`
}

func searchItem(repo string, number int, updated string) string {
	return fmt.Sprintf(`{"number":%d,"title":"PR %d","updated_at":%q,"repository_url":"https://api.github.com/repos/%s"}`, number, number, updated, repo)
}

func requestedEvent(id int64, requester, reviewer string) string {
	return fmt.Sprintf(`{"id":%d,"event":"review_requested","review_requester":{"login":%q},"requested_reviewer":{"login":%q}}`, id, requester, reviewer)
}

func prJSON(repo string, number int) string {
	return fmt.Sprintf(`{"number":%d,"title":"T","html_url":"https://github.com/%s/pull/%d","head":{"sha":%q},"base":{"ref":"main","repo":{"full_name":%q}}}`, number, repo, number, headSHA, repo)
}

func TestLatestRequest(t *testing.T) {
	teamEvents := `{"id":1,"event":"review_requested","review_requester":{"login":"a"},"requested_team":{"slug":"backend"}},` +
		`{"id":2,"event":"review_requested","review_requester":{"login":"b"},"requested_team":{"slug":"frontend"}}`
	cases := []struct {
		name          string
		events        string
		teams         []string
		wantKey       string // empty when no request matches
		wantRequester string
	}{
		{
			// The requester is the person who asked, and the latest request to the login wins.
			name:          "direct request",
			events:        requestedEvent(10, "tanaka", "me") + "," + requestedEvent(11, "sato", "someone-else") + "," + requestedEvent(12, "suzuki", "ME"),
			wantKey:       "acme/app#3@12",
			wantRequester: "suzuki",
		},
		{
			name:    "unknown requester",
			events:  `{"id":20,"event":"review_requested","requested_reviewer":{"login":"me"}}`,
			wantKey: "acme/app#3@20",
		},
		{name: "configured team", events: teamEvents, teams: []string{"acme/backend"}, wantKey: "acme/app#3@1", wantRequester: "a"},
		{name: "team of another organization", events: teamEvents, teams: []string{"other/backend"}},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			events, err := decodePages([]byte("[" + c.events + "]"))
			if err != nil {
				t.Fatal(err)
			}
			f, ok := latestRequest(events, searchHit{Repo: "acme/app", Number: 3}, "me", c.teams)
			if ok != (c.wantKey != "") || f.Key != c.wantKey || f.Requester != c.wantRequester {
				t.Errorf("found = %+v %v", f, ok)
			}
		})
	}
}

// A search across all of GitHub always answers incomplete results, so every direct-request query is scoped.
func TestRequestQueriesAreScopedToConfiguredOwners(t *testing.T) {
	cfg := &Config{Repositories: map[string]string{"zeta/one": "/a", "acme/app": "/b", "acme/lib": "/c"}}
	owners := cfg.owners()
	if strings.Join(owners, ",") != "acme,zeta" {
		t.Fatalf("owners = %v", owners)
	}
	want := []string{
		"is:pr is:open archived:false user-review-requested:@me user:acme",
		"is:pr is:open archived:false user-review-requested:@me user:zeta",
		"is:pr is:open archived:false team-review-requested:acme/backend",
	}
	got := requestQueries(owners, []string{"acme/backend"})
	if strings.Join(got, "\n") != strings.Join(want, "\n") {
		t.Fatalf("queries = %q", got)
	}
}

// A re-request bumps the PR's updated_at, so the cache must not hide it; unchanged PRs cost no event reads.
func TestPendingRequestsRereadsEventsOnlyForUpdatedPRs(t *testing.T) {
	r := &fakeRunner{}
	updated := "2026-10-20T10:00:00Z"
	r.on("gh api --paginate -X GET search/issues", func([]string) ([]byte, error) {
		return []byte(searchJSON(searchItem("acme/app", 3, updated))), nil
	})
	events := `[` + requestedEvent(7, "tanaka", "me") + `]`
	r.on("gh api --paginate repos/acme/app/issues/3/events", func([]string) ([]byte, error) { return []byte(events), nil })

	found, cache, err := pendingRequests(context.Background(), r, "gh", "me", []string{"acme"}, nil, nil)
	if err != nil || len(found) != 1 || found[0].Key != "acme/app#3@7" {
		t.Fatalf("found = %+v, %v", found, err)
	}
	r.calls = nil
	if _, _, err := pendingRequests(context.Background(), r, "gh", "me", []string{"acme"}, nil, cache); err != nil || r.ran("gh api --paginate repos/") {
		t.Fatalf("an unchanged PR must come from the cache: %v", err)
	}
	// The second page of the events, as gh writes it.
	updated, events = "2026-10-20T11:00:00Z", events+"\n["+requestedEvent(8, "sato", "me")+"]"
	found, _, err = pendingRequests(context.Background(), r, "gh", "me", []string{"acme"}, nil, cache)
	if err != nil || found[0].Key != "acme/app#3@8" || found[0].Requester != "sato" {
		t.Fatalf("found = %+v, %v", found, err)
	}
}

func found(repo string, number int, id int64) Found {
	return Found{Key: fmt.Sprintf("%s#%d@%d", repo, number, id), Repo: repo, Number: number, Title: "T"}
}

// Search can briefly miss a PR; the request must still be notified when it shows up again.
func TestApplyPollRequeuesARequestSearchMissedOnce(t *testing.T) {
	st := newState()
	st.applyPoll([]Found{found("acme/app", 1, 10)}, time.Now(), false)
	st.applyPoll(nil, time.Now(), false)
	st.applyPoll([]Found{found("acme/app", 1, 10)}, time.Now(), false)
	if got := st.Requests["acme/app#1@10"].Status; got != StatusQueued {
		t.Fatalf("status = %s", got)
	}
}

func TestRecoverNeverRepeatsALaunch(t *testing.T) {
	st := newState()
	st.applyPoll([]Found{found("acme/app", 1, 10), found("acme/app", 2, 20)}, time.Now(), false)
	st.Requests["acme/app#1@10"].Status = StatusNotifying
	st.Requests["acme/app#2@20"].Status = StatusLaunching
	st.recover(time.Now())
	if got := st.Requests["acme/app#1@10"].Status; got != StatusQueued {
		t.Errorf("notifying became %s", got)
	}
	if got := st.Requests["acme/app#2@20"].Status; got != StatusFailed {
		t.Errorf("launching became %s", got)
	}
}

func TestLockAllowsOneWatcher(t *testing.T) {
	dir := t.TempDir()
	lock, err := lockDir(dir)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := lockDir(dir); err == nil {
		t.Error("a second instance must not get the lock")
	}
	if err := lock.Close(); err != nil {
		t.Fatal(err)
	}
}

// daemonFixture wires a daemon to a fake GitHub whose pending requests the test changes between polls.
type daemonFixture struct {
	d      *Daemon
	r      *fakeRunner
	env    Env
	mu     sync.Mutex
	events map[string]string // "repo#n" -> events JSON
	// Answers alerter by the request key in its --group argument.
	answer func(key string) string
	opened []string
	store  Store
}

func newDaemonFixture(t *testing.T) *daemonFixture {
	env := testEnv(t, "code-review")
	cfg := mustConfig(t, env, configJSON(choiceJSON("code", "コードレビュー", "code-review")))
	f := &daemonFixture{r: &fakeRunner{}, env: env, events: map[string]string{}}
	f.r.reply("gh api user", `{"login":"me"}`)
	f.r.on("gh api --paginate -X GET search/issues", func([]string) ([]byte, error) {
		f.mu.Lock()
		defer f.mu.Unlock()
		var items []string
		for k, ev := range f.events {
			repo, num, _ := strings.Cut(k, "#")
			n, _ := strconv.Atoi(num)
			items = append(items, searchItem(repo, n, fmt.Sprint(len(ev))))
		}
		return []byte(searchJSON(items...)), nil
	})
	f.r.on("gh api --paginate repos/", func(args []string) ([]byte, error) {
		f.mu.Lock()
		defer f.mu.Unlock()
		for k, ev := range f.events {
			repo, num, _ := strings.Cut(k, "#")
			if strings.HasPrefix(args[2], "repos/"+repo+"/issues/"+num+"/") {
				return []byte(ev), nil
			}
		}
		return nil, fmt.Errorf("no events for %v", args)
	})
	f.r.on("gh api repos/", func(args []string) ([]byte, error) {
		parts := strings.Split(args[1], "/")
		n, _ := strconv.Atoi(parts[4])
		return []byte(prJSON(parts[1]+"/"+parts[2], n)), nil
	})
	f.r.on("alerter", func(args []string) ([]byte, error) {
		return []byte(f.answer(strings.TrimPrefix(argValue(t, args, "--group"), "review-notifier:"))), nil
	})
	checkout := cfg.Repositories["acme/app"]
	f.r.reply("git -C "+checkout+" config --get remote.origin.url", "git@github.com:acme/app.git\n")
	f.r.reply("git -C "+checkout+" cat-file", "")
	f.r.reply("git -C "+checkout+" worktree add", "")
	f.r.on("open", func(args []string) ([]byte, error) {
		f.mu.Lock()
		defer f.mu.Unlock()
		f.opened = append(f.opened, args[0])
		return nil, nil
	})
	f.r.reply("osascript", "")

	f.store = Store{path: filepath.Join(t.TempDir(), "state.json")}
	st, _, _ := f.store.Load()
	tools := Tools{R: f.r, Cmds: Commands{GH: "gh", Git: "git", Alerter: "alerter", Open: "open", Osascript: "osascript"}, Cfg: cfg}
	f.d = newDaemon(tools, f.store, st, log.New(io.Discard, "", 0))
	f.d.sleep = func(context.Context, time.Duration) error { return nil }
	return f
}

func (f *daemonFixture) request(repo string, number int, events ...string) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.events[fmt.Sprintf("%s#%d", repo, number)] = "[" + strings.Join(events, ",") + "]"
}

func (f *daemonFixture) poll(t *testing.T) {
	t.Helper()
	if err := f.d.pollOnce(context.Background()); err != nil {
		t.Fatal(err)
	}
}

func (f *daemonFixture) status(t *testing.T, key string) Status {
	t.Helper()
	st, _, err := f.store.Load()
	if err != nil {
		t.Fatal(err)
	}
	req, ok := st.Requests[key]
	if !ok {
		t.Fatalf("%s not in state", key)
	}
	return req.Status
}

// Claims and handles one request synchronously, as one notification slot of notifyLoop does.
func (d *Daemon) handleNext(ctx context.Context) bool {
	req := d.claim()
	if req == nil {
		return false
	}
	if ch := d.ask(ctx, ctx, *req); ch != nil {
		d.launch(ctx, *req, ch)
	}
	return true
}

func TestDaemonNotifiesEachRequestOnceAndOpensTheChoice(t *testing.T) {
	f := newDaemonFixture(t)
	ctx := context.Background()
	f.answer = func(string) string {
		return `{"activationType":"actionClicked","activationValue":"コードレビュー"}`
	}
	f.request("acme/app", 1, requestedEvent(10, "tanaka", "me"))
	f.poll(t)
	if f.d.handleNext(ctx) {
		t.Fatal("the first poll is only a baseline")
	}

	f.request("acme/app", 12, requestedEvent(30, "sato", "me"))
	f.poll(t)
	if !f.d.handleNext(ctx) || f.status(t, "acme/app#12@30") != StatusLaunched || len(f.opened) != 1 {
		t.Fatalf("the new request must be launched; opened = %v", f.opened)
	}
	u, _ := url.Parse(f.opened[0])
	worktree := worktreePath(f.d.tools.Cfg.WorktreeRoot, PullRequest{Repo: "acme/app", Number: 12, HeadSHA: headSHA})
	skill := filepath.Join(f.env.CodexHome, "skills", "code-review", "SKILL.md")
	want := "[$code-review](" + skill + ") https://github.com/acme/app/pull/12 " + headSHA + " " + worktree
	if q := u.Query(); q.Get("prompt") != want || q.Get("path") != worktree {
		t.Errorf("link = %s", f.opened[0])
	}

	// After a restart, nothing already handled comes back; a re-request of the same PR does.
	st, _, _ := f.store.Load()
	f.d.st = st
	f.poll(t)
	if f.d.handleNext(ctx) {
		t.Error("a launched request must not be notified again")
	}
	f.request("acme/app", 12, requestedEvent(30, "sato", "me"), requestedEvent(31, "sato", "me"))
	f.poll(t)
	if !f.d.handleNext(ctx) || f.status(t, "acme/app#12@31") != StatusLaunched {
		t.Error("a re-request must be notified")
	}
}

func TestDaemonAnswersOtherThanAChoiceStartNothing(t *testing.T) {
	cases := []struct {
		name   string
		answer string
		want   Status
	}{
		{"decline", `{"activationType":"closed","activationValue":"見送る"}`, StatusDeclined},
		{"timeout", `{"activationType":"timeout"}`, StatusTimeout},
		{"body click", `{"activationType":"contentsClicked"}`, StatusDismissed},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			f := newDaemonFixture(t)
			ctx := context.Background()
			f.poll(t)
			f.request("acme/app", 1, requestedEvent(10, "tanaka", "me"))
			f.poll(t)
			f.answer = func(string) string { return c.answer }
			f.d.handleNext(ctx)
			if got := f.status(t, "acme/app#1@10"); got != c.want {
				t.Errorf("status = %s", got)
			}
			f.poll(t)
			if f.d.handleNext(ctx) || f.r.ran("git") || len(f.opened) != 0 {
				t.Error("the request must start nothing and not come back")
			}
		})
	}
}

func TestDaemonReportsALaunchFailure(t *testing.T) {
	f := newDaemonFixture(t)
	f.answer = func(string) string {
		return `{"activationType":"actionClicked","activationValue":"コードレビュー"}`
	}
	f.poll(t)
	f.request("acme/unknown", 5, requestedEvent(50, "tanaka", "me"))
	f.poll(t)
	f.d.handleNext(context.Background())
	if f.status(t, "acme/unknown#5@50") != StatusFailed || !f.r.ran("osascript") || len(f.opened) != 0 {
		t.Error("the failure must be notified without opening anything")
	}
}

func TestDaemonKeepsPollingAfterAnAPIFailure(t *testing.T) {
	f := newDaemonFixture(t)
	f.r.on("gh api --paginate -X GET search/issues", func([]string) ([]byte, error) {
		return nil, fmt.Errorf("HTTP 403: API rate limit exceeded")
	})
	ctx, cancel := context.WithCancel(context.Background())
	var delays []time.Duration
	f.d.sleep = func(_ context.Context, d time.Duration) error {
		delays = append(delays, d)
		if len(delays) == 3 {
			cancel()
			return context.Canceled
		}
		return nil
	}
	f.d.pollLoop(ctx)
	if len(delays) != 3 || delays[1] <= delays[0] {
		t.Errorf("delays = %v", delays)
	}
	if st, _, _ := f.store.Load(); !strings.Contains(st.LastError, "rate limit") {
		t.Errorf("last error = %q", st.LastError)
	}
}

// With every place taken by unanswered notifications, a new request closes exactly the oldest one.
func TestDaemonClosesTheOldestNotificationForANewRequest(t *testing.T) {
	cases := []struct {
		name string
		// The fourth request arrives while three are on screen, instead of being queued from the start.
		late bool
		// alerter takes a while to close, and wake-ups arrive meanwhile; they must not close another one.
		slowClose bool
	}{
		{name: "new request while three are shown", late: true},
		{name: "four queued and a wake-up while closing", slowClose: true},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			f := newDaemonFixture(t)
			f.poll(t)
			queued := 4
			if c.late {
				queued = 3
			}
			for n := 1; n <= queued; n++ {
				f.request("acme/app", n, requestedEvent(int64(n*10), "tanaka", "me"))
			}
			f.poll(t)

			var mu sync.Mutex
			var closed []string
			f.r.onCtx("alerter", func(ctx context.Context, args []string) ([]byte, error) {
				key := strings.TrimPrefix(argValue(t, args, "--group"), "review-notifier:")
				if strings.HasPrefix(key, "acme/app#4@") {
					return []byte(`{"activationType":"closed"}`), nil
				}
				<-ctx.Done()
				mu.Lock()
				closed = append(closed, key)
				mu.Unlock()
				if c.slowClose {
					for range 5 {
						f.d.signal()
						time.Sleep(10 * time.Millisecond)
					}
				}
				return nil, ctx.Err()
			})
			ctx, cancel := context.WithCancel(context.Background())
			done := make(chan struct{})
			go func() { f.d.notifyLoop(ctx); close(done) }()

			if c.late {
				waitFor(t, func() bool { return f.status(t, "acme/app#3@30") == StatusNotifying })
				f.request("acme/app", 4, requestedEvent(40, "tanaka", "me"))
				f.poll(t)
			}
			// The fourth is shown only after the closed one has exited, so any extra close happened before this.
			waitFor(t, func() bool { return f.status(t, "acme/app#4@40") == StatusDeclined })
			mu.Lock()
			got := append([]string(nil), closed...)
			mu.Unlock()
			cancel()
			<-done

			if len(got) != 1 || got[0] != "acme/app#1@10" {
				t.Errorf("closed = %v, want only the oldest", got)
			}
			if s := f.status(t, "acme/app#1@10"); s != StatusReplaced {
				t.Errorf("oldest = %s", s)
			}
		})
	}
}

func waitFor(t *testing.T, cond func() bool) {
	t.Helper()
	deadline := time.Now().Add(5 * time.Second)
	for !cond() {
		if time.Now().After(deadline) {
			t.Fatal("condition not reached")
		}
		time.Sleep(5 * time.Millisecond)
	}
}
