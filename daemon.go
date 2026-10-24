package main

import (
	"context"
	"errors"
	"fmt"
	"log"
	"maps"
	"strconv"
	"sync"
	"time"
)

// Polling and notifying run separately, so a notification waiting for the user never delays the next poll.
type Daemon struct {
	tools Tools
	store Store
	log   *log.Logger
	now   func() time.Time
	sleep func(context.Context, time.Duration) error

	mu   sync.Mutex
	st   *State
	wake chan struct{}

	launchMu sync.Mutex
}

func newDaemon(t Tools, store Store, st *State, logger *log.Logger) *Daemon {
	return &Daemon{
		tools: t,
		store: store,
		st:    st,
		log:   logger,
		now:   time.Now,
		sleep: sleepCtx,
		wake:  make(chan struct{}, 1),
	}
}

func (d *Daemon) Run(ctx context.Context) error {
	d.mu.Lock()
	d.st.recover(d.now())
	err := d.store.Save(d.st)
	d.mu.Unlock()
	if err != nil {
		return err
	}

	var wg sync.WaitGroup
	wg.Add(1)
	go func() {
		defer wg.Done()
		d.notifyLoop(ctx)
	}()
	d.pollLoop(ctx)
	wg.Wait()
	return nil
}

func (d *Daemon) pollLoop(ctx context.Context) {
	interval := time.Duration(d.tools.Cfg.PollIntervalSeconds) * time.Second
	failures := 0
	for {
		delay := interval
		if err := d.pollOnce(ctx); err != nil {
			if ctx.Err() != nil {
				return
			}
			failures++
			delay = backoff(interval, failures)
			d.log.Printf("poll failed (%d in a row, next in %s): %v", failures, delay, err)
		} else {
			failures = 0
		}
		if d.sleep(ctx, delay) != nil {
			return
		}
	}
}

func (d *Daemon) pollOnce(ctx context.Context) error {
	d.mu.Lock()
	login := d.st.Login
	cache := maps.Clone(d.st.PRs)
	d.mu.Unlock()

	t := d.tools
	if login == "" {
		var err error
		if login, err = currentLogin(ctx, t.R, t.Cmds.GH); err != nil {
			return d.recordError(err)
		}
	}
	found, next, err := pendingRequests(ctx, t.R, t.Cmds.GH, login, t.Cfg.owners(), t.Cfg.Teams, cache)
	if err != nil {
		return d.recordError(err)
	}

	d.mu.Lock()
	baseline := d.st.LastPoll.IsZero() && len(d.st.Requests) == 0
	added := d.st.applyPoll(found, d.now(), baseline)
	d.st.Login, d.st.PRs, d.st.LastPoll, d.st.LastError = login, next, d.now(), ""
	err = d.store.Save(d.st)
	d.mu.Unlock()
	if err != nil {
		return err
	}
	if baseline {
		d.log.Printf("recorded %d pending requests as the baseline; only later requests are notified", len(found))
	}
	for _, key := range added {
		d.log.Printf("new request %s", key)
	}
	d.signal()
	return nil
}

var errReplaced = errors.New("replaced by a newer request")

type shownNotice struct {
	seq       int
	cancel    context.CancelCauseFunc
	replacing bool
}

// Up to maxNotifications notifications wait for an answer at once. When they are all on screen and
// another request is queued, the oldest one is closed as if it timed out, so a new request is never
// held back by notifications left unanswered and the number of alerter processes stays bounded.
func (d *Daemon) notifyLoop(ctx context.Context) {
	var wg sync.WaitGroup
	defer wg.Wait()
	var mu sync.Mutex
	shown := map[string]*shownNotice{}
	seq := 0
	for ctx.Err() == nil {
		mu.Lock()
		full := len(shown) >= d.tools.Cfg.MaxNotifications
		// One notification is closed per waiting request; a closing one already makes room.
		if full && replacing(shown) < d.queuedCount() {
			d.replaceOldest(shown)
		}
		mu.Unlock()

		if !full {
			if req := d.claim(); req != nil {
				nctx, cancel := context.WithCancelCause(ctx)
				mu.Lock()
				seq++
				shown[req.Key] = &shownNotice{seq: seq, cancel: cancel}
				mu.Unlock()
				wg.Add(1)
				go func() {
					defer wg.Done()
					ch := d.ask(ctx, nctx, *req)
					mu.Lock()
					delete(shown, req.Key)
					mu.Unlock()
					cancel(nil)
					// The freed place may let a queued request through.
					d.signal()
					if ch != nil {
						d.launch(ctx, *req, ch)
					}
				}()
				continue
			}
		}
		select {
		case <-ctx.Done():
			return
		case <-d.wake:
		}
	}
}

// Shows one request and records the answer. Returns the choice when the user picked one.
func (d *Daemon) ask(ctx, nctx context.Context, req Request) *Choice {
	t := d.tools
	notice := Notice{Key: req.Key, PR: PullRequest{Repo: req.Repo, Number: req.Number, Title: req.Title}, Requester: req.Requester, Team: req.Team}
	sel, err := notify(nctx, t.R, t.Cmds.Alerter, notice, t.Cfg.Choices, t.Cfg.NotificationTimeoutSeconds)
	if ctx.Err() != nil {
		// Stopping: the request stays notifying and is shown again after the restart.
		return nil
	}
	// A choice made just before the notification was closed still counts.
	if context.Cause(nctx) == errReplaced && (err != nil || sel.Outcome != Chosen) {
		d.logSave(d.update(req.Key, func(r *Request) { r.Status = StatusReplaced }))
		d.log.Printf("%s: %v", req.Key, errReplaced)
		return nil
	}
	if err != nil {
		d.log.Printf("notify %s: %v", req.Key, err)
		// Queued again after a poll interval rather than retried in a tight loop.
		if d.sleep(ctx, time.Duration(t.Cfg.PollIntervalSeconds)*time.Second) != nil {
			return nil
		}
		d.logSave(d.update(req.Key, func(r *Request) { r.Status, r.Error = StatusQueued, err.Error() }))
		return nil
	}

	switch sel.Outcome {
	case Chosen:
		return sel.Choice
	case Declined:
		d.logSave(d.update(req.Key, func(r *Request) { r.Status = StatusDeclined }))
	case TimedOut:
		d.logSave(d.update(req.Key, func(r *Request) { r.Status = StatusTimeout }))
	case Dismissed:
		d.logSave(d.update(req.Key, func(r *Request) { r.Status = StatusDismissed }))
	}
	d.log.Printf("%s: %s", req.Key, sel.Outcome)
	return nil
}

func (d *Daemon) launch(ctx context.Context, req Request, ch *Choice) {
	// Recorded before anything is opened, so a restart never opens the same request twice.
	// Without that record nothing is opened.
	if err := d.update(req.Key, func(r *Request) { r.Status, r.Choice, r.Error = StatusLaunching, ch.ID, "" }); err != nil {
		d.log.Printf("%s: not launched: %v", req.Key, err)
		d.reportFailure(ctx, req, err)
		return
	}
	d.log.Printf("%s: chose %s", req.Key, ch.ID)
	// Worktrees of one checkout are added one at a time.
	d.launchMu.Lock()
	res, err := launch(ctx, d.tools, ch, req.Repo, req.Number, req.Requester)
	d.launchMu.Unlock()
	d.logSave(d.update(req.Key, func(r *Request) {
		r.HeadSHA, r.Worktree = res.PR.HeadSHA, res.Worktree
		if err != nil {
			r.Status, r.Error = StatusFailed, err.Error()
		} else {
			r.Status = StatusLaunched
		}
	}))
	if err != nil {
		d.log.Printf("%s: launch failed: %v", req.Key, err)
		d.reportFailure(ctx, req, err)
		return
	}
	d.log.Printf("%s: opened %s at %s", req.Key, res.Worktree, res.PR.HeadSHA)
}

func (d *Daemon) queuedCount() int {
	d.mu.Lock()
	defer d.mu.Unlock()
	n := 0
	for _, req := range d.st.Requests {
		if req.Status == StatusQueued {
			n++
		}
	}
	return n
}

func replacing(shown map[string]*shownNotice) int {
	n := 0
	for _, s := range shown {
		if s.replacing {
			n++
		}
	}
	return n
}

// Closes the longest-shown notification that is not already being closed.
func (d *Daemon) replaceOldest(shown map[string]*shownNotice) {
	var oldest *shownNotice
	key := ""
	for k, s := range shown {
		if !s.replacing && (oldest == nil || s.seq < oldest.seq) {
			oldest, key = s, k
		}
	}
	if oldest == nil {
		return
	}
	oldest.replacing = true
	d.log.Printf("%s: closing to show a newer request", key)
	oldest.cancel(errReplaced)
}

// Marks the oldest queued request as being notified and returns a copy, or nil when none is queued
// or the mark could not be saved.
func (d *Daemon) claim() *Request {
	d.mu.Lock()
	defer d.mu.Unlock()
	req := d.st.nextQueued()
	if req == nil {
		return nil
	}
	prev := *req
	req.Status, req.UpdatedAt = StatusNotifying, d.now()
	if err := d.store.Save(d.st); err != nil {
		// Left queued, so the next wake-up tries again.
		*req = prev
		d.log.Printf("save state: %v", err)
		return nil
	}
	snapshot := *req
	return &snapshot
}

var errUnknownRequest = errors.New("request is not in the state")

// Applies the change and saves it. On a failed save the change is undone in memory too,
// so memory never claims a step the state file does not hold.
func (d *Daemon) update(key string, change func(*Request)) error {
	d.mu.Lock()
	defer d.mu.Unlock()
	req, ok := d.st.Requests[key]
	if !ok {
		return errUnknownRequest
	}
	prev := *req
	change(req)
	req.UpdatedAt = d.now()
	if err := d.store.Save(d.st); err != nil {
		*req = prev
		return fmt.Errorf("save state: %w", err)
	}
	return nil
}

func (d *Daemon) recordError(err error) error {
	d.mu.Lock()
	d.st.LastError = err.Error()
	if saveErr := d.store.Save(d.st); saveErr != nil {
		err = errors.Join(err, saveErr)
	}
	d.mu.Unlock()
	return err
}

func (d *Daemon) reportFailure(ctx context.Context, req Request, err error) {
	subtitle := req.Repo + " #" + strconv.Itoa(req.Number)
	if nerr := notifyFailure(ctx, d.tools.R, d.tools.Cmds.Osascript, subtitle, "レビューを開始できませんでした: "+err.Error()); nerr != nil {
		d.log.Printf("%s: failure notification: %v", req.Key, nerr)
	}
}

func (d *Daemon) logSave(err error) {
	if err != nil {
		d.log.Print(err)
	}
}

func (d *Daemon) signal() {
	select {
	case d.wake <- struct{}{}:
	default:
	}
}

const maxBackoff = 15 * time.Minute

// Rate limits and outages are waited out with a growing delay instead of stopping the watcher.
func backoff(interval time.Duration, failures int) time.Duration {
	delay := interval
	for i := 0; i < failures && delay < maxBackoff; i++ {
		delay *= 2
	}
	return min(delay, maxBackoff)
}

func sleepCtx(ctx context.Context, d time.Duration) error {
	timer := time.NewTimer(d)
	defer timer.Stop()
	select {
	case <-ctx.Done():
		return ctx.Err()
	case <-timer.C:
		return nil
	}
}
