package main

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"sort"
	"strings"
	"text/template"
)

type Config struct {
	// "owner/repo" to the absolute path of the checkout that worktrees are created from.
	Repositories map[string]string `json:"repositories"`
	// "org/team-slug" whose review requests are notified too; direct requests are always notified.
	Teams                      []string `json:"teams"`
	PollIntervalSeconds        int      `json:"pollIntervalSeconds"`
	WorktreeRoot               string   `json:"worktreeRoot"`
	NotificationTimeoutSeconds int      `json:"notificationTimeoutSeconds"`
	// Notifications waiting for an answer at the same time; more requests wait in the queue.
	MaxNotifications int      `json:"maxNotifications"`
	Commands         Commands `json:"commands"`
	Choices          []Choice `json:"choices"`
}

// Sorted owners of the configured repositories.
func (c *Config) owners() []string {
	seen := map[string]bool{}
	var owners []string
	for repo := range c.Repositories {
		owner, _, _ := strings.Cut(repo, "/")
		if !seen[owner] {
			seen[owner] = true
			owners = append(owners, owner)
		}
	}
	sort.Strings(owners)
	return owners
}

// Absolute paths that replace the fixed-directory lookup.
type Commands struct {
	GH        string `json:"gh"`
	Git       string `json:"git"`
	Alerter   string `json:"alerter"`
	Open      string `json:"open"`
	Osascript string `json:"osascript"`
}

type Choice struct {
	ID     string `json:"id"`
	Label  string `json:"label"`
	Skill  string `json:"skill"`
	Prompt string `json:"prompt"`

	skillPath string
	prompt    *template.Template
}

type Env struct {
	Home      string
	CodexHome string
}

var (
	choiceID  = regexp.MustCompile(`^[a-z0-9][a-z0-9-]*$`)
	skillName = regexp.MustCompile(`^[A-Za-z0-9][A-Za-z0-9_-]*$`)
	repoName  = regexp.MustCompile(`^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$`)
)

const (
	defaultTimeoutSeconds = 1800
	defaultPollSeconds    = 60
	minPollSeconds        = 30
	defaultNotifications  = 3
	maxNotifications      = 10
)

func defaultConfigPath(home string) string {
	if xdg := os.Getenv("XDG_CONFIG_HOME"); filepath.IsAbs(xdg) {
		return filepath.Join(xdg, "review-notifier", "config.json")
	}
	return filepath.Join(home, ".config", "review-notifier", "config.json")
}

func loadConfig(path string, env Env) (*Config, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	cfg, err := parseConfig(data, env)
	if err != nil {
		return nil, fmt.Errorf("%s: %w", path, err)
	}
	return cfg, nil
}

// Unknown keys are rejected so that a setting this version does not support is never silently ignored.
func parseConfig(data []byte, env Env) (*Config, error) {
	dec := json.NewDecoder(bytes.NewReader(data))
	dec.DisallowUnknownFields()
	var cfg Config
	if err := dec.Decode(&cfg); err != nil {
		return nil, err
	}
	if dec.More() {
		return nil, errors.New("trailing data after the config object")
	}
	if err := cfg.resolve(env); err != nil {
		return nil, err
	}
	return &cfg, nil
}

func (c *Config) resolve(env Env) error {
	if c.NotificationTimeoutSeconds < 0 {
		return errors.New("notificationTimeoutSeconds must not be negative")
	}
	if c.NotificationTimeoutSeconds == 0 {
		c.NotificationTimeoutSeconds = defaultTimeoutSeconds
	}
	if c.PollIntervalSeconds == 0 {
		c.PollIntervalSeconds = defaultPollSeconds
	}
	if c.PollIntervalSeconds < minPollSeconds {
		return fmt.Errorf("pollIntervalSeconds must be at least %d", minPollSeconds)
	}
	if c.MaxNotifications == 0 {
		c.MaxNotifications = defaultNotifications
	}
	if c.MaxNotifications < 1 || c.MaxNotifications > maxNotifications {
		return fmt.Errorf("maxNotifications must be between 1 and %d", maxNotifications)
	}
	for i, team := range c.Teams {
		if !repoName.MatchString(team) {
			return fmt.Errorf("teams[%d] is not org/team-slug: %q", i, team)
		}
	}

	root := c.WorktreeRoot
	if root == "" {
		root = filepath.Join(env.Home, ".local", "share", "review-notifier", "worktrees")
	}
	root = expandHome(root, env.Home)
	if !filepath.IsAbs(root) {
		return fmt.Errorf("worktreeRoot must be absolute: %q", c.WorktreeRoot)
	}
	c.WorktreeRoot = filepath.Clean(root)

	for repo, path := range c.Repositories {
		if !repoName.MatchString(repo) {
			return fmt.Errorf("repositories: %q is not owner/repo", repo)
		}
		expanded := expandHome(path, env.Home)
		if !filepath.IsAbs(expanded) {
			return fmt.Errorf("repositories.%s must be an absolute path: %q", repo, path)
		}
		c.Repositories[repo] = filepath.Clean(expanded)
	}

	for name, path := range map[string]string{"gh": c.Commands.GH, "git": c.Commands.Git, "alerter": c.Commands.Alerter, "open": c.Commands.Open, "osascript": c.Commands.Osascript} {
		if path != "" && !filepath.IsAbs(path) {
			return fmt.Errorf("commands.%s must be an absolute path: %q", name, path)
		}
	}

	if len(c.Choices) == 0 {
		return errors.New("choices must have at least one entry")
	}
	ids := map[string]bool{}
	labels := map[string]bool{}
	for i := range c.Choices {
		ch := &c.Choices[i]
		if !choiceID.MatchString(ch.ID) {
			return fmt.Errorf("choices[%d].id must match %s: %q", i, choiceID, ch.ID)
		}
		if ids[ch.ID] {
			return fmt.Errorf("choices: duplicate id %q", ch.ID)
		}
		ids[ch.ID] = true
		label := strings.TrimSpace(ch.Label)
		// alerter splits --actions on commas.
		if label == "" || strings.ContainsAny(label, ",\n") {
			return fmt.Errorf("choices[%d].label must be one line without commas: %q", i, ch.Label)
		}
		if labels[label] {
			return fmt.Errorf("choices: duplicate label %q", label)
		}
		labels[label] = true
		ch.Label = label
		if !skillName.MatchString(ch.Skill) {
			return fmt.Errorf("choices[%d].skill is not a skill name: %q", i, ch.Skill)
		}
		ch.skillPath = filepath.Join(env.CodexHome, "skills", ch.Skill, "SKILL.md")
		if _, err := os.Stat(ch.skillPath); err != nil {
			return fmt.Errorf("choices[%d].skill %q: %w", i, ch.Skill, err)
		}
		tmpl, err := template.New(ch.ID).Option("missingkey=error").Parse(ch.Prompt)
		if err != nil {
			return fmt.Errorf("choices[%d].prompt: %w", i, err)
		}
		if strings.TrimSpace(ch.Prompt) == "" {
			return fmt.Errorf("choices[%d].prompt is empty", i)
		}
		ch.prompt = tmpl
		// Render once with sample values so that a wrong field name fails at load, not after the user picked it.
		if _, err := ch.render(PromptData{PR: PullRequest{Repo: "o/r", Number: 1}}); err != nil {
			return fmt.Errorf("choices[%d].prompt: %w", i, err)
		}
	}
	return nil
}

func expandHome(path, home string) string {
	if path == "~" {
		return home
	}
	if strings.HasPrefix(path, "~/") {
		return filepath.Join(home, path[2:])
	}
	return path
}

type PromptData struct {
	PR        PullRequest
	Requester string
	Skill     string
	SkillPath string
	Worktree  string
}

func (ch *Choice) render(data PromptData) (string, error) {
	data.Skill = ch.Skill
	data.SkillPath = ch.skillPath
	var buf bytes.Buffer
	if err := ch.prompt.Execute(&buf, data); err != nil {
		return "", err
	}
	return buf.String(), nil
}
