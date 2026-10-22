package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"sort"
	"strings"
)

// A review request is one review_requested event, so a re-request of the same PR is a new request
// while comments and pushes keep the key unchanged.
type Found struct {
	Key       string `json:"key"`
	Repo      string `json:"repo"`
	Number    int    `json:"number"`
	Title     string `json:"title"`
	Requester string `json:"requester"`
	Team      string `json:"team,omitempty"`
}

type cachedPR struct {
	UpdatedAt string `json:"updatedAt"`
	Found     *Found `json:"found,omitempty"`
}

// Lists the pending requests. Events are read again only for PRs updated since the last poll.
func pendingRequests(ctx context.Context, r Runner, gh, login string, owners, teams []string, cache map[string]cachedPR) ([]Found, map[string]cachedPR, error) {
	hits := map[string]searchHit{}
	for _, q := range requestQueries(owners, teams) {
		found, err := searchPRs(ctx, r, gh, q)
		if err != nil {
			return nil, nil, err
		}
		for _, h := range found {
			hits[prKey(h.Repo, h.Number)] = h
		}
	}
	keys := make([]string, 0, len(hits))
	for k := range hits {
		keys = append(keys, k)
	}
	sort.Strings(keys)

	next := map[string]cachedPR{}
	var result []Found
	for _, k := range keys {
		hit := hits[k]
		if c, ok := cache[k]; ok && c.UpdatedAt == hit.UpdatedAt {
			next[k] = c
		} else {
			events, err := fetchEvents(ctx, r, gh, hit.Repo, hit.Number)
			if err != nil {
				return nil, nil, err
			}
			c := cachedPR{UpdatedAt: hit.UpdatedAt}
			if f, ok := latestRequest(events, hit, login, teams); ok {
				c.Found = &f
			} else {
				// Still pending per search, but no event names it: kept under a PR-level key and an unknown requester.
				c.Found = &Found{Key: k + "@unknown", Repo: hit.Repo, Number: hit.Number, Title: hit.Title}
			}
			next[k] = c
		}
		f := *next[k].Found
		f.Title = hit.Title
		result = append(result, f)
	}
	return result, next, nil
}

// Search lists what is pending now, independent of whether a notification was read on another device.
// A search across all of GitHub always times out into incomplete results, so each query is limited to
// the owners of the configured repositories (user: matches organizations too).
func requestQueries(owners, teams []string) []string {
	var queries []string
	for _, owner := range owners {
		queries = append(queries, "is:pr is:open archived:false user-review-requested:@me user:"+owner)
	}
	for _, team := range teams {
		queries = append(queries, "is:pr is:open archived:false team-review-requested:"+team)
	}
	return queries
}

type searchHit struct {
	Repo      string
	Number    int
	Title     string
	UpdatedAt string
}

// Search answers at most 1000 results; past that the oldest updated requests are not seen.
func searchPRs(ctx context.Context, r Runner, gh, query string) ([]searchHit, error) {
	out, err := r.Run(ctx, gh, "api", "--paginate", "-X", "GET", "search/issues", "-f", "q="+query, "-f", "per_page=100", "-f", "sort=updated")
	if err != nil {
		return nil, err
	}
	type page struct {
		Incomplete bool `json:"incomplete_results"`
		Items      []struct {
			Number        int    `json:"number"`
			Title         string `json:"title"`
			UpdatedAt     string `json:"updated_at"`
			RepositoryURL string `json:"repository_url"`
		} `json:"items"`
	}
	pages, err := decodeAll[page](out)
	if err != nil {
		return nil, fmt.Errorf("search %q: %w", query, err)
	}
	var hits []searchHit
	for _, p := range pages {
		// A partial answer would make missing requests look withdrawn.
		if p.Incomplete {
			return nil, fmt.Errorf("search %q: incomplete results", query)
		}
		for _, it := range p.Items {
			repo := strings.TrimPrefix(it.RepositoryURL, "https://api.github.com/repos/")
			if !repoName.MatchString(repo) || it.Number <= 0 {
				return nil, fmt.Errorf("search %q: unexpected item %s#%d", query, it.RepositoryURL, it.Number)
			}
			hits = append(hits, searchHit{Repo: repo, Number: it.Number, Title: it.Title, UpdatedAt: it.UpdatedAt})
		}
	}
	return hits, nil
}

type issueEvent struct {
	ID              int64  `json:"id"`
	Event           string `json:"event"`
	ReviewRequester *struct {
		Login string `json:"login"`
	} `json:"review_requester"`
	RequestedReviewer *struct {
		Login string `json:"login"`
	} `json:"requested_reviewer"`
	RequestedTeam *struct {
		Slug string `json:"slug"`
	} `json:"requested_team"`
}

func fetchEvents(ctx context.Context, r Runner, gh, repo string, number int) ([]issueEvent, error) {
	out, err := r.Run(ctx, gh, "api", "--paginate", fmt.Sprintf("repos/%s/issues/%d/events?per_page=100", repoPath(repo), number))
	if err != nil {
		return nil, err
	}
	return decodePages(out)
}

func currentLogin(ctx context.Context, r Runner, gh string) (string, error) {
	out, err := r.Run(ctx, gh, "api", "user")
	if err != nil {
		return "", err
	}
	var user struct {
		Login string `json:"login"`
	}
	if err := json.Unmarshal(out, &user); err != nil || user.Login == "" {
		return "", fmt.Errorf("gh api user: no login in %q", out)
	}
	return user.Login, nil
}

// The latest review_requested event addressed to the login or to one of the configured teams.
// The requester is the person who asked for the review, never the PR author as a stand-in.
func latestRequest(events []issueEvent, hit searchHit, login string, teams []string) (Found, bool) {
	owner, _, _ := strings.Cut(hit.Repo, "/")
	var best *issueEvent
	team := ""
	for i := range events {
		ev := &events[i]
		if ev.Event != "review_requested" {
			continue
		}
		matched := ""
		switch {
		case ev.RequestedReviewer != nil && strings.EqualFold(ev.RequestedReviewer.Login, login):
		case ev.RequestedTeam != nil && configuredTeam(teams, owner, ev.RequestedTeam.Slug) != "":
			matched = configuredTeam(teams, owner, ev.RequestedTeam.Slug)
		default:
			continue
		}
		if best == nil || ev.ID > best.ID {
			best, team = ev, matched
		}
	}
	if best == nil {
		return Found{}, false
	}
	requester := ""
	if best.ReviewRequester != nil {
		requester = best.ReviewRequester.Login
	}
	return Found{
		Key:       fmt.Sprintf("%s#%d@%d", hit.Repo, hit.Number, best.ID),
		Repo:      hit.Repo,
		Number:    hit.Number,
		Title:     hit.Title,
		Requester: requester,
		Team:      team,
	}, true
}

func configuredTeam(teams []string, owner, slug string) string {
	for _, t := range teams {
		org, s, _ := strings.Cut(t, "/")
		if strings.EqualFold(org, owner) && strings.EqualFold(s, slug) {
			return t
		}
	}
	return ""
}

// gh api --paginate writes one JSON value per page back to back.
func decodeAll[T any](data []byte) ([]T, error) {
	dec := json.NewDecoder(bytes.NewReader(data))
	var all []T
	for {
		var v T
		err := dec.Decode(&v)
		if errors.Is(err, io.EOF) {
			return all, nil
		}
		if err != nil {
			return nil, err
		}
		all = append(all, v)
	}
}

func decodePages(data []byte) ([]issueEvent, error) {
	pages, err := decodeAll[[]issueEvent](data)
	if err != nil {
		return nil, fmt.Errorf("issue events: %w", err)
	}
	var all []issueEvent
	for _, p := range pages {
		all = append(all, p...)
	}
	return all, nil
}

func prKey(repo string, number int) string {
	return fmt.Sprintf("%s#%d", strings.ToLower(repo), number)
}
