// Package watch runs the bundled PR watch engine as a per-user LaunchAgent and passes its commands through.
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
  pr-wake watch install|uninstall|start|stop
  pr-wake watch status [--json]
  pr-wake watch logs [-f]
  pr-wake watch sender
  pr-wake watch link PR [--take-over]
  pr-wake watch release PR
  pr-wake watch dry-run [--pr PR]...`)

// ExitCode carries the engine's exit status, whose own message is already on stderr.
type ExitCode struct{ Code int }

func (e *ExitCode) Error() string { return fmt.Sprintf("engine exited with %d", e.Code) }

func Run(args []string, stdout io.Writer) error {
	if len(args) == 0 {
		return ErrUsage
	}
	home, err := os.UserHomeDir()
	if err != nil {
		return err
	}
	engine, err := enginePath()
	if err != nil {
		return err
	}
	a := agent(home, engine)
	switch args[0] {
	case "install":
		return install(home, engine, stdout)
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
		flags := flag.NewFlagSet("status", flag.ContinueOnError)
		raw := flags.Bool("json", false, "print the engine's JSON, for a program reading the sender and the links")
		if err := flags.Parse(args[1:]); err != nil {
			return err
		}
		if *raw {
			return runEngine(engine, []string{"watch", "--status"}, stdout)
		}
		return status(a, engine, stdout)
	case "logs":
		flags := flag.NewFlagSet("logs", flag.ContinueOnError)
		follow := flags.Bool("f", false, "follow the log")
		if err := flags.Parse(args[1:]); err != nil {
			return err
		}
		return launchagent.Logs(a, *follow, stdout)
	}
	engineArgs, err := passthrough(args)
	if err != nil {
		return err
	}
	return runEngine(engine, engineArgs, stdout)
}

// The engine's own flags for each passed-through subcommand, so its JSON reaches the caller unchanged.
func passthrough(args []string) ([]string, error) {
	switch args[0] {
	case "sender":
		if len(args) != 1 {
			return nil, ErrUsage
		}
		return []string{"watch", "--sender"}, nil
	case "link":
		flags := flag.NewFlagSet("link", flag.ContinueOnError)
		takeOver := flags.Bool("take-over", false, "take over a PR another chat is watched for")
		pr, err := onePR(flags, args[1:])
		if err != nil {
			return nil, err
		}
		out := []string{"watch", "--link", pr}
		if *takeOver {
			out = append(out, "--take-over")
		}
		return out, nil
	case "release":
		pr, err := onePR(flag.NewFlagSet("release", flag.ContinueOnError), args[1:])
		if err != nil {
			return nil, err
		}
		return []string{"watch", "--release", pr}, nil
	case "dry-run":
		flags := flag.NewFlagSet("dry-run", flag.ContinueOnError)
		var prs prList
		flags.Var(&prs, "pr", "a PR to check; the configured repositories' open PRs when none")
		if err := flags.Parse(args[1:]); err != nil {
			return nil, err
		}
		if flags.NArg() != 0 {
			return nil, ErrUsage
		}
		out := []string{"watch", "--dry-run"}
		for _, pr := range prs {
			out = append(out, "--pr", pr)
		}
		return out, nil
	default:
		return nil, ErrUsage
	}
}

// The PR may come before or after the flags, as `link PR --take-over` reads naturally.
func onePR(flags *flag.FlagSet, args []string) (string, error) {
	var positional []string
	for len(args) > 0 {
		if err := flags.Parse(args); err != nil {
			return "", err
		}
		args = flags.Args()
		if len(args) > 0 {
			positional = append(positional, args[0])
			args = args[1:]
		}
	}
	if len(positional) != 1 {
		return "", ErrUsage
	}
	return positional[0], nil
}

type prList []string

func (p *prList) String() string     { return strings.Join(*p, ",") }
func (p *prList) Set(v string) error { *p = append(*p, v); return nil }

func runEngine(engine string, args []string, stdout io.Writer) error {
	if !fileExists(engine) {
		return missingEngine(engine)
	}
	cmd := exec.Command(engine, args...)
	cmd.Stdout, cmd.Stderr = stdout, os.Stderr
	if err := cmd.Run(); err != nil {
		var exit *exec.ExitError
		if errors.As(err, &exit) {
			return &ExitCode{Code: exit.ExitCode()}
		}
		return err
	}
	return nil
}

func agent(home, engine string) launchagent.Agent {
	return launchagent.Agent{
		Label:       "io.github.riii111.pr-wake.watch",
		Command:     "watch",
		Home:        home,
		Program:     []string{engine, "watch"},
		Log:         filepath.Join(home, "Library", "Logs", "pr-wake", "watch.log"),
		ProcessType: "Background",
	}
}

func legacyAgent(home string) launchagent.Agent {
	return launchagent.Agent{Label: "io.github.riii111.pr-watch", Home: home}
}

func install(home, engine string, stdout io.Writer) error {
	if _, err := os.Stat(configPath(home)); errors.Is(err, fs.ErrNotExist) {
		return fmt.Errorf("%s not found; write it as the README shows", configPath(home))
	}
	if !fileExists(engine) {
		return missingEngine(engine)
	}
	// Validate before replacing the running agent so a failed check leaves it running.
	if err := launchagent.RunQuiet(stdout, engine, "watch", "--dry-run"); err != nil {
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
	return launchagent.Install(agent(home, engine), u.Username, stdout)
}

func status(a launchagent.Agent, engine string, stdout io.Writer) error {
	if err := launchagent.PrintLoaded(a, stdout); err != nil {
		return err
	}
	if !fileExists(engine) {
		return missingEngine(engine)
	}
	// The engine owns the layout of its records, so they are read through it.
	var out, stderr bytes.Buffer
	cmd := exec.Command(engine, "watch", "--status")
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
		return "sender: none; run `pr-wake watch sender` in a chat kept for it"
	}
}

// The engine is installed beside pr-wake, so a copy installed elsewhere for a trial runs its own engine.
func enginePath() (string, error) {
	self, err := os.Executable()
	if err != nil {
		return "", err
	}
	if resolved, err := filepath.EvalSymlinks(self); err == nil {
		self = resolved
	}
	return filepath.Join(filepath.Dir(self), engineName), nil
}

const engineName = "pr-wake-engine"

func missingEngine(path string) error {
	return fmt.Errorf("%s not found; install pr-wake with scripts/install.sh", path)
}

func configPath(home string) string {
	return filepath.Join(home, ".config", "harnexus", "pr-watch.json")
}

func fileExists(path string) bool {
	_, err := os.Stat(path)
	return err == nil
}
