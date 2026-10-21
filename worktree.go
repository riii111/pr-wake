package main

import (
	"context"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"regexp"
	"strings"
)

// Creates a detached worktree at the PR head, or returns a clean one an earlier selection created.
// Only objects are fetched into the checkout; its branches, HEAD and working tree are left alone.
func prepareWorktree(ctx context.Context, r Runner, git string, cfg *Config, pr PullRequest) (string, error) {
	checkout, ok := lookupRepo(cfg.Repositories, pr.Repo)
	if !ok {
		return "", fmt.Errorf("%s has no local checkout in repositories of the config", pr.Repo)
	}
	// The configured URL, before any url.<base>.insteadOf rewriting, names the repository.
	out, err := r.Run(ctx, git, "-C", checkout, "config", "--get", "remote.origin.url")
	if err != nil {
		return "", err
	}
	if got, ok := remoteRepo(string(out)); !ok || !strings.EqualFold(got, pr.Repo) {
		return "", fmt.Errorf("%s: origin %q is not %s", checkout, strings.TrimSpace(string(out)), pr.Repo)
	}

	path, reuse, err := pickWorktree(ctx, r, git, worktreePath(cfg.WorktreeRoot, pr), pr.HeadSHA)
	if err != nil || reuse {
		return path, err
	}

	commit := pr.HeadSHA + "^{commit}"
	if _, err := r.Run(ctx, git, "-C", checkout, "cat-file", "-e", commit); err != nil {
		// The pull ref also serves heads from forks; the SHA itself covers a PR that moved on since it was read.
		fetched := false
		for _, ref := range []string{fmt.Sprintf("refs/pull/%d/head", pr.Number), pr.HeadSHA} {
			if _, err := r.Run(ctx, git, "-C", checkout, "fetch", "--no-tags", "--no-write-fetch-head", "origin", ref); err != nil {
				continue
			}
			if _, err := r.Run(ctx, git, "-C", checkout, "cat-file", "-e", commit); err == nil {
				fetched = true
				break
			}
		}
		if !fetched {
			return "", fmt.Errorf("%s: cannot fetch %s", checkout, pr.HeadSHA)
		}
	}
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		return "", err
	}
	if _, err := r.Run(ctx, git, "-C", checkout, "worktree", "add", "--detach", path, pr.HeadSHA); err != nil {
		return "", err
	}
	return path, nil
}

// Earlier worktrees of the same head are kept with whatever was done in them, so one is reused
// only while it is clean; otherwise the next numbered path is used.
const maxWorktreeCopies = 20

func pickWorktree(ctx context.Context, r Runner, git, base, sha string) (string, bool, error) {
	for i := 1; i <= maxWorktreeCopies; i++ {
		path := base
		if i > 1 {
			path = fmt.Sprintf("%s-%d", base, i)
		}
		if _, err := os.Stat(path); errors.Is(err, fs.ErrNotExist) {
			return path, false, nil
		} else if err != nil {
			return "", false, err
		}
		if cleanAt(ctx, r, git, path, sha) {
			return path, true, nil
		}
	}
	return "", false, fmt.Errorf("%s: %d worktrees of this head exist and none is clean; remove those no longer needed", base, maxWorktreeCopies)
}

func cleanAt(ctx context.Context, r Runner, git, path, sha string) bool {
	head, err := r.Run(ctx, git, "-C", path, "rev-parse", "HEAD")
	if err != nil || strings.TrimSpace(string(head)) != sha {
		return false
	}
	status, err := r.Run(ctx, git, "-C", path, "status", "--porcelain", "--untracked-files=all")
	return err == nil && len(strings.TrimSpace(string(status))) == 0
}

func worktreePath(root string, pr PullRequest) string {
	owner, name, _ := strings.Cut(pr.Repo, "/")
	return filepath.Join(root, owner, name, fmt.Sprintf("pr-%d-%s", pr.Number, pr.HeadSHA[:12]))
}

func lookupRepo(repos map[string]string, repo string) (string, bool) {
	for name, path := range repos {
		if strings.EqualFold(name, repo) {
			return path, true
		}
	}
	return "", false
}

var githubRemote = regexp.MustCompile(`^(?:https://github\.com/|ssh://git@github\.com/|git@github\.com:)([A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+?)(?:\.git)?/?$`)

func remoteRepo(remote string) (string, bool) {
	m := githubRemote.FindStringSubmatch(strings.TrimSpace(remote))
	if m == nil {
		return "", false
	}
	return m[1], true
}
