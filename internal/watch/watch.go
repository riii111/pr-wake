// Package watch runs `harnexus-task watch` as a per-user LaunchAgent.
//
// The watch tells each task worker what happened on its PR. It is kept apart from
// the review notifier, which notifies the owner, so either can be stopped alone.
package watch

import (
	"bytes"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"io/fs"
	"os"
	"os/exec"
	"os/user"
	"path/filepath"
	"strings"

	"github.com/riii111/pr-wake/internal/launchagent"
)

var ErrUsage = errors.New(`usage:
  pr-wake watch install|uninstall|start|stop|status
  pr-wake watch logs [-f]`)

func Run(args []string, stdout io.Writer) error {
	if len(args) == 0 {
		return ErrUsage
	}
	home, err := os.UserHomeDir()
	if err != nil {
		return err
	}
	a := agent(home)
	switch args[0] {
	case "install":
		return install(home, stdout)
	case "uninstall":
		if err := launchagent.Uninstall(a, stdout); err != nil {
			return err
		}
		_, err := fmt.Fprintln(stdout, "config, state and logs are kept")
		return err
	case "start":
		return launchagent.Start(a, stdout)
	case "stop":
		return launchagent.Stop(a, stdout)
	case "status":
		return status(home, stdout)
	case "logs":
		flags := flag.NewFlagSet("logs", flag.ContinueOnError)
		follow := flags.Bool("f", false, "follow the log")
		if err := flags.Parse(args[1:]); err != nil {
			return err
		}
		return launchagent.Logs(a, *follow, stdout)
	default:
		return ErrUsage
	}
}

func agent(home string) launchagent.Agent {
	return launchagent.Agent{
		Label:       "io.github.riii111.pr-wake.watch",
		Command:     "watch",
		Home:        home,
		Program:     []string{binaryPath(home), "watch"},
		Log:         filepath.Join(home, "Library", "Logs", "pr-wake", "watch.log"),
		ProcessType: "Background",
	}
}

// The agent dotfiles installed, replaced on install so two watches never send the same message.
func legacyAgent(home string) launchagent.Agent {
	return launchagent.Agent{Label: "io.github.riii111.pr-watch", Home: home}
}

func install(home string, stdout io.Writer) error {
	for _, item := range []struct{ path, hint string }{
		{configPath(home), "write it as the harnexus README shows"},
		{binaryPath(home), "run `bun run install:task` in the harnexus checkout"},
	} {
		if _, err := os.Stat(item.path); errors.Is(err, fs.ErrNotExist) {
			return fmt.Errorf("%s not found; %s", item.path, item.hint)
		}
	}
	// Fails before the old agent is stopped or the new one is loaded if the config, a gh account
	// or the GitHub API is not usable.
	// The dry run lists every comment of every open PR, so only its failure is shown.
	if err := launchagent.RunQuiet(stdout, binaryPath(home), "watch", "--dry-run"); err != nil {
		return err
	}
	if old := legacyAgent(home); fileExists(old.Plist()) {
		if err := launchagent.Uninstall(old, stdout); err != nil {
			return err
		}
	}
	u, err := user.Current()
	if err != nil {
		return err
	}
	return launchagent.Install(agent(home), u.Username, stdout)
}

func status(home string, stdout io.Writer) error {
	if err := launchagent.PrintLoaded(agent(home), stdout); err != nil {
		return err
	}
	binary := binaryPath(home)
	if !fileExists(binary) {
		return fmt.Errorf("%s not found", binary)
	}
	// harnexus-task owns the layout of its records, so they are read through it.
	var out, stderr bytes.Buffer
	cmd := exec.Command(binary, "watch", "--status")
	cmd.Stdout, cmd.Stderr = &out, &stderr
	if err := cmd.Run(); err != nil {
		_, _ = os.Stderr.Write(stderr.Bytes())
		return err
	}
	_, err := fmt.Fprintln(stdout, strings.Join(statusLines(out.Bytes()), "\n"))
	return err
}

type shownPR struct {
	PR             string `json:"pr"`
	Status         string `json:"status"`
	Reason         string `json:"reason"`
	Pending        int    `json:"pending"`
	AwaitingAnswer bool   `json:"awaitingAnswer"`
}

type shown struct {
	PRs         []shownPR `json:"prs"`
	Sender      string    `json:"sender"`
	SenderError string    `json:"senderError"`
}

// Output that is not the expected object is shown as it is.
func statusLines(output []byte) []string {
	var s shown
	if err := json.Unmarshal(output, &s); err != nil {
		return []string{strings.TrimRight(string(output), "\n")}
	}
	lines := make([]string, 0, len(s.PRs)+1)
	for _, pr := range s.PRs {
		detail := ""
		if pr.Reason != "" {
			detail = " (" + pr.Reason + ")"
		}
		waiting := ""
		if pr.AwaitingAnswer {
			waiting = " awaiting answer"
		}
		lines = append(lines, fmt.Sprintf("%s %s%s pending=%d%s", pr.PR, pr.Status, detail, pr.Pending, waiting))
	}
	return append(lines, senderLine(s))
}

func senderLine(s shown) string {
	switch {
	case s.SenderError != "":
		return fmt.Sprintf("sender: unreadable (%s)", s.SenderError)
	case s.Sender != "":
		return "sender: " + s.Sender
	default:
		return "sender: none; run `harnexus-task watch --sender` in a chat kept for it"
	}
}

func binaryPath(home string) string {
	return filepath.Join(home, ".local", "bin", "harnexus-task")
}

func configPath(home string) string {
	return filepath.Join(home, ".config", "harnexus", "pr-watch.json")
}

func fileExists(path string) bool {
	_, err := os.Stat(path)
	return err == nil
}
