package main

import (
	"context"
	"encoding/json"
	"fmt"
	"net/url"
	"regexp"
	"strconv"
	"strings"
)

type PullRequest struct {
	Repo    string
	Number  int
	Title   string
	URL     string
	HeadSHA string
	BaseRef string
}

var commitSHA = regexp.MustCompile(`^[0-9a-f]{40}$`)

// Accepts owner/repo#123 or a pull request URL on github.com.
func parsePRRef(ref string) (string, int, error) {
	if u, err := url.Parse(ref); err == nil && u.Scheme == "https" && strings.EqualFold(u.Host, "github.com") {
		parts := strings.Split(strings.Trim(u.Path, "/"), "/")
		if len(parts) >= 4 && parts[2] == "pull" {
			return checkedRef(parts[0]+"/"+parts[1], parts[3])
		}
		return "", 0, fmt.Errorf("not a pull request URL: %s", ref)
	}
	repo, num, ok := strings.Cut(ref, "#")
	if !ok {
		return "", 0, fmt.Errorf("expected owner/repo#number or a pull request URL: %s", ref)
	}
	return checkedRef(repo, num)
}

func checkedRef(repo, num string) (string, int, error) {
	if !repoName.MatchString(repo) {
		return "", 0, fmt.Errorf("not owner/repo: %q", repo)
	}
	n, err := strconv.Atoi(num)
	if err != nil || n <= 0 {
		return "", 0, fmt.Errorf("not a pull request number: %q", num)
	}
	return repo, n, nil
}

func repoPath(repo string) string {
	owner, name, _ := strings.Cut(repo, "/")
	return url.PathEscape(owner) + "/" + url.PathEscape(name)
}

func fetchPR(ctx context.Context, r Runner, gh, repo string, number int) (PullRequest, error) {
	out, err := r.Run(ctx, gh, "api", fmt.Sprintf("repos/%s/pulls/%d", repoPath(repo), number))
	if err != nil {
		return PullRequest{}, err
	}
	return parsePR(out, repo, number)
}

func parsePR(data []byte, repo string, number int) (PullRequest, error) {
	var raw struct {
		Number  int    `json:"number"`
		Title   string `json:"title"`
		HTMLURL string `json:"html_url"`
		Head    struct {
			SHA string `json:"sha"`
		} `json:"head"`
		Base struct {
			Ref  string `json:"ref"`
			Repo struct {
				FullName string `json:"full_name"`
			} `json:"repo"`
		} `json:"base"`
	}
	if err := json.Unmarshal(data, &raw); err != nil {
		return PullRequest{}, fmt.Errorf("pull request %s#%d: %w", repo, number, err)
	}
	if raw.Number != number || !strings.EqualFold(raw.Base.Repo.FullName, repo) {
		return PullRequest{}, fmt.Errorf("pull request %s#%d: answer names %s#%d", repo, number, raw.Base.Repo.FullName, raw.Number)
	}
	if !commitSHA.MatchString(raw.Head.SHA) {
		return PullRequest{}, fmt.Errorf("pull request %s#%d: invalid head sha %q", repo, number, raw.Head.SHA)
	}
	return PullRequest{
		Repo:    raw.Base.Repo.FullName,
		Number:  number,
		Title:   raw.Title,
		URL:     raw.HTMLURL,
		HeadSHA: raw.Head.SHA,
		BaseRef: raw.Base.Ref,
	}, nil
}
