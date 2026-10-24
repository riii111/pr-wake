package main

import (
	"context"
	"encoding/json"
	"fmt"
	"strconv"
	"strings"
)

type Notice struct {
	Key       string
	PR        PullRequest
	Requester string
	Team      string
}

type Outcome string

const (
	Chosen    Outcome = "chosen"
	Declined  Outcome = "declined"
	TimedOut  Outcome = "timeout"
	Dismissed Outcome = "dismissed"
)

type Selection struct {
	Outcome Outcome
	Choice  *Choice
}

func notify(ctx context.Context, r Runner, alerter string, n Notice, choices []Choice, timeoutSeconds int) (Selection, error) {
	out, err := r.Run(ctx, alerter, alerterArgs(n, choices, timeoutSeconds)...)
	if err != nil {
		return Selection{}, err
	}
	return parseSelection(out, choices)
}

// Anything but a click on one of the configured actions starts nothing.
func parseSelection(data []byte, choices []Choice) (Selection, error) {
	var raw struct {
		ActivationType       string  `json:"activationType"`
		ActivationValue      *string `json:"activationValue"`
		ActivationValueIndex *string `json:"activationValueIndex"`
	}
	if err := json.Unmarshal(data, &raw); err != nil {
		return Selection{}, fmt.Errorf("alerter output: %w: %q", err, data)
	}
	switch raw.ActivationType {
	case "closed":
		return Selection{Outcome: Declined}, nil
	case "timeout":
		return Selection{Outcome: TimedOut}, nil
	case "actionClicked":
	default:
		return Selection{Outcome: Dismissed}, nil
	}
	value := ""
	if raw.ActivationValue != nil {
		value = *raw.ActivationValue
	}
	if raw.ActivationValueIndex != nil {
		i, err := strconv.Atoi(*raw.ActivationValueIndex)
		if err != nil || i < 0 || i >= len(choices) || choices[i].Label != value {
			return Selection{}, fmt.Errorf("alerter output names no configured action: index %q, value %q", *raw.ActivationValueIndex, value)
		}
		return Selection{Outcome: Chosen, Choice: &choices[i]}, nil
	}
	for i := range choices {
		if choices[i].Label == value {
			return Selection{Outcome: Chosen, Choice: &choices[i]}, nil
		}
	}
	return Selection{}, fmt.Errorf("alerter output names no configured action: %q", value)
}

const (
	noticeTitle   = "GitHub レビュー依頼"
	dropdownLabel = "レビュー方法を選択"
	declineLabel  = "見送る"
)

func alerterArgs(n Notice, choices []Choice, timeoutSeconds int) []string {
	labels := make([]string, len(choices))
	for i, ch := range choices {
		labels[i] = ch.Label
	}
	requester := "依頼者不明"
	if n.Requester != "" {
		requester = "依頼者: @" + n.Requester
	}
	if n.Team != "" {
		requester += "（" + n.Team + " 宛て）"
	}
	args := []string{
		"--title", noticeTitle,
		"--subtitle", oneLine(n.PR.Title),
		"--message", fmt.Sprintf("%s\n%s #%d", requester, n.PR.Repo, n.PR.Number),
		"--actions", strings.Join(labels, ","),
		"--close-label", declineLabel,
		"--group", "review-notifier:" + n.Key,
		"--timeout", strconv.Itoa(timeoutSeconds),
		"--json",
	}
	if len(choices) > 1 {
		args = append(args, "--dropdown-label", dropdownLabel)
	}
	return args
}

func oneLine(s string) string {
	return strings.Join(strings.Fields(s), " ")
}
