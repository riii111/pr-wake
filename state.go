package main

import (
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"sort"
	"syscall"
	"time"
)

type Status string

const (
	// Pending when the state was first created; never notified.
	StatusBaseline  Status = "baseline"
	StatusQueued    Status = "queued"
	StatusNotifying Status = "notifying"
	StatusDeclined  Status = "declined"
	StatusTimeout   Status = "timeout"
	StatusDismissed Status = "dismissed"
	// Closed unanswered to make room for a newer request; treated like a timeout.
	StatusReplaced Status = "replaced"
	// The request disappeared from GitHub before it was shown.
	StatusWithdrawn Status = "withdrawn"
	StatusLaunching Status = "launching"
	StatusLaunched  Status = "launched"
	StatusFailed    Status = "failed"
)

type Request struct {
	Found
	Status    Status    `json:"status"`
	FirstSeen time.Time `json:"firstSeen"`
	UpdatedAt time.Time `json:"updatedAt"`
	Choice    string    `json:"choice,omitempty"`
	HeadSHA   string    `json:"headSha,omitempty"`
	Worktree  string    `json:"worktree,omitempty"`
	Error     string    `json:"error,omitempty"`
}

type State struct {
	Version   int                 `json:"version"`
	Login     string              `json:"login"`
	Requests  map[string]*Request `json:"requests"`
	PRs       map[string]cachedPR `json:"prs"`
	LastPoll  time.Time           `json:"lastPoll"`
	LastError string              `json:"lastError,omitempty"`
}

const (
	stateVersion = 1
	// Finished requests are kept this long after they stop being pending, so a late search result cannot notify them again.
	retention = 90 * 24 * time.Hour
)

func newState() *State {
	return &State{Version: stateVersion, Requests: map[string]*Request{}, PRs: map[string]cachedPR{}}
}

func defaultStateDir(home string) string {
	if xdg := os.Getenv("XDG_STATE_HOME"); filepath.IsAbs(xdg) {
		return filepath.Join(xdg, "review-notifier")
	}
	return filepath.Join(home, ".local", "state", "review-notifier")
}

type Store struct {
	path string
}

func (s Store) Load() (*State, bool, error) {
	data, err := os.ReadFile(s.path)
	if errors.Is(err, fs.ErrNotExist) {
		return newState(), false, nil
	}
	if err != nil {
		return nil, false, err
	}
	st := newState()
	if err := json.Unmarshal(data, st); err != nil {
		return nil, false, fmt.Errorf("%s: %w", s.path, err)
	}
	if st.Version != stateVersion {
		return nil, false, fmt.Errorf("%s: unsupported version %d", s.path, st.Version)
	}
	if st.Requests == nil {
		st.Requests = map[string]*Request{}
	}
	if st.PRs == nil {
		st.PRs = map[string]cachedPR{}
	}
	return st, true, nil
}

// Written to a temporary file and renamed, so a crash leaves either the old or the new state.
func (s Store) Save(st *State) error {
	data, err := json.MarshalIndent(st, "", "  ")
	if err != nil {
		return err
	}
	if err := os.MkdirAll(filepath.Dir(s.path), 0o700); err != nil {
		return err
	}
	tmp, err := os.CreateTemp(filepath.Dir(s.path), ".state-*.json")
	if err != nil {
		return err
	}
	// Fails harmlessly once the rename has moved the file.
	defer func() { _ = os.Remove(tmp.Name()) }()
	if _, err := tmp.Write(append(data, '\n')); err != nil {
		return errors.Join(err, tmp.Close())
	}
	if err := tmp.Sync(); err != nil {
		return errors.Join(err, tmp.Close())
	}
	if err := tmp.Close(); err != nil {
		return err
	}
	return os.Rename(tmp.Name(), s.path)
}

// Held for the life of the watcher, so a second instance cannot notify or launch the same request.
func lockDir(dir string) (*os.File, error) {
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return nil, err
	}
	f, err := os.OpenFile(filepath.Join(dir, "lock"), os.O_CREATE|os.O_RDWR, 0o600)
	if err != nil {
		return nil, err
	}
	if err := syscall.Flock(int(f.Fd()), syscall.LOCK_EX|syscall.LOCK_NB); err != nil {
		return nil, errors.Join(fmt.Errorf("another review-notifier is running (%s): %w", f.Name(), err), f.Close())
	}
	return f, nil
}

// Records one poll. The first poll of a new state only records a baseline.
func (st *State) applyPoll(found []Found, now time.Time, baseline bool) []string {
	active := map[string]bool{}
	activePR := map[string]bool{}
	var added []string
	for _, f := range found {
		active[f.Key] = true
		activePR[prKey(f.Repo, f.Number)] = true
		if req, ok := st.Requests[f.Key]; ok {
			req.Title = f.Title
			// Search can briefly miss a PR; a request it lists again is still owed its notification.
			if req.Status == StatusWithdrawn {
				req.Status, req.UpdatedAt = StatusQueued, now
			}
			continue
		}
		status := StatusQueued
		if baseline {
			status = StatusBaseline
		}
		st.Requests[f.Key] = &Request{Found: f, Status: status, FirstSeen: now, UpdatedAt: now}
		if !baseline {
			added = append(added, f.Key)
		}
	}
	for key, req := range st.Requests {
		switch {
		case active[key]:
		case req.Status == StatusQueued:
			req.Status, req.UpdatedAt = StatusWithdrawn, now
		case !activePR[prKey(req.Repo, req.Number)] && now.Sub(req.UpdatedAt) > retention:
			delete(st.Requests, key)
		}
	}
	sort.Strings(added)
	return added
}

func (st *State) nextQueued() *Request {
	var next *Request
	for _, req := range st.Requests {
		if req.Status != StatusQueued {
			continue
		}
		if next == nil || req.FirstSeen.Before(next.FirstSeen) || (req.FirstSeen.Equal(next.FirstSeen) && req.Key < next.Key) {
			next = req
		}
	}
	return next
}

// A notification or launch cut short by a crash or restart. A shown notification is shown again;
// a launch is never repeated because the App may already have opened the chat.
func (st *State) recover(now time.Time) {
	for _, req := range st.Requests {
		switch req.Status {
		case StatusNotifying:
			req.Status, req.UpdatedAt = StatusQueued, now
		case StatusLaunching:
			req.Status, req.UpdatedAt = StatusFailed, now
			req.Error = "interrupted while launching; check the Codex App before starting it again"
		}
	}
}
