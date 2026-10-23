package main

import (
	"context"
	"fmt"
)

type Tools struct {
	R    Runner
	Cmds Commands
	Cfg  *Config
}

type Launched struct {
	PR       PullRequest
	Worktree string
}

// Reads the PR as it is when the choice was made, pins a worktree to that head and opens the prefilled chat.
func launch(ctx context.Context, t Tools, ch *Choice, repo string, number int, requester string) (Launched, error) {
	pr, err := fetchPR(ctx, t.R, t.Cmds.GH, repo, number)
	if err != nil {
		return Launched{}, err
	}
	path, err := prepareWorktree(ctx, t.R, t.Cmds.Git, t.Cfg, pr)
	if err != nil {
		return Launched{PR: pr}, err
	}
	prompt, err := ch.render(PromptData{PR: pr, Requester: requester, Worktree: path})
	if err != nil {
		return Launched{PR: pr, Worktree: path}, err
	}
	if err := openDeepLink(ctx, t.R, t.Cmds.Open, deepLink(prompt, t.Cfg.ChatHome)); err != nil {
		return Launched{PR: pr, Worktree: path}, fmt.Errorf("open deep link: %w", err)
	}
	return Launched{PR: pr, Worktree: path}, nil
}

// A plain notification for a failure after the user chose a review; it has no actions and does not wait.
func notifyFailure(ctx context.Context, r Runner, osascript, subtitle, message string) error {
	_, err := r.Run(ctx, osascript,
		"-e", "on run argv",
		"-e", "display notification (item 1 of argv) with title (item 2 of argv) subtitle (item 3 of argv)",
		"-e", "end run",
		message, "review-notifier", subtitle)
	return err
}
