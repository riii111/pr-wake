// Package launchagent manages a resident command as a per-user LaunchAgent.
package launchagent

import (
	"bytes"
	"encoding/xml"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"maps"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"strings"
)

type Agent struct {
	Label string
	// The pr-wake subcommand, named in hints.
	Command string
	Home    string
	Program []string
	// Set in the agent's environment besides HOME, PATH and LANG.
	Env map[string]string
	Log string
	// Interactive for an agent the owner answers, as notifications are; Background otherwise.
	ProcessType string
}

func (a Agent) Plist() string {
	return filepath.Join(a.Home, "Library", "LaunchAgents", a.Label+".plist")
}

func (a Agent) service() string {
	return fmt.Sprintf("%s/%s", domain(), a.Label)
}

func Install(a Agent, user string, out io.Writer) error {
	if err := Stop(a, out); err != nil {
		return err
	}
	for _, dir := range []string{filepath.Dir(a.Plist()), filepath.Dir(a.Log)} {
		if err := os.MkdirAll(dir, 0o755); err != nil {
			return err
		}
	}
	if err := os.WriteFile(a.Plist(), Render(a, user), 0o644); err != nil {
		return err
	}
	return Start(a, out)
}

func Uninstall(a Agent, out io.Writer) error {
	if err := Stop(a, out); err != nil {
		return err
	}
	if err := os.Remove(a.Plist()); err != nil && !errors.Is(err, fs.ErrNotExist) {
		return err
	}
	return nil
}

func Start(a Agent, out io.Writer) error {
	if _, err := os.Stat(a.Plist()); errors.Is(err, fs.ErrNotExist) {
		return fmt.Errorf("%s not found; run `pr-wake %s install`", a.Plist(), a.Command)
	}
	if Loaded(a) {
		return Run(out, "launchctl", "kickstart", "-k", a.service())
	}
	return Run(out, "launchctl", "bootstrap", domain(), a.Plist())
}

func Stop(a Agent, out io.Writer) error {
	if !Loaded(a) {
		_, err := fmt.Fprintf(out, "%s is not running\n", a.Label)
		return err
	}
	return Run(out, "launchctl", "bootout", a.service())
}

func PrintLoaded(a Agent, out io.Writer) error {
	state := "not loaded"
	if Loaded(a) {
		state = "loaded"
	}
	_, err := fmt.Fprintf(out, "%s: %s\n", a.Label, state)
	return err
}

func Logs(a Agent, follow bool, out io.Writer) error {
	if _, err := os.Stat(a.Log); errors.Is(err, fs.ErrNotExist) {
		return fmt.Errorf("%s not found", a.Log)
	}
	args := []string{"-n", "200"}
	if follow {
		args = append(args, "-F")
	}
	return Run(out, "tail", append(args, a.Log)...)
}

func Loaded(a Agent) bool {
	return exec.Command("launchctl", "print", a.service()).Run() == nil
}

func Render(a Agent, user string) []byte {
	// launchd does not inherit the login shell's PATH.
	path := strings.Join([]string{
		filepath.Join(a.Home, ".nix-profile", "bin"),
		filepath.Join("/etc/profiles/per-user", user, "bin"),
		"/run/current-system/sw/bin",
		"/nix/var/nix/profiles/default/bin",
		"/opt/homebrew/bin",
		"/usr/bin",
		"/bin",
		"/usr/sbin",
		"/sbin",
	}, ":")

	var b bytes.Buffer
	b.WriteString(xml.Header)
	b.WriteString(`<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">` + "\n")
	b.WriteString("<plist version=\"1.0\">\n<dict>\n")
	key := func(k string) { fmt.Fprintf(&b, "\t<key>%s</key>\n", k) }
	str := func(indent, s string) {
		b.WriteString(indent + "<string>")
		_ = xml.EscapeText(&b, []byte(s))
		b.WriteString("</string>\n")
	}

	key("Label")
	str("\t", a.Label)
	key("ProgramArguments")
	b.WriteString("\t<array>\n")
	for _, arg := range a.Program {
		str("\t\t", arg)
	}
	b.WriteString("\t</array>\n")
	key("EnvironmentVariables")
	b.WriteString("\t<dict>\n")
	env := [][2]string{{"HOME", a.Home}, {"PATH", path}, {"LANG", "en_US.UTF-8"}}
	for _, name := range slices.Sorted(maps.Keys(a.Env)) {
		env = append(env, [2]string{name, a.Env[name]})
	}
	for _, kv := range env {
		fmt.Fprintf(&b, "\t\t<key>%s</key>\n", kv[0])
		str("\t\t", kv[1])
	}
	b.WriteString("\t</dict>\n")
	key("RunAtLoad")
	b.WriteString("\t<true/>\n")
	// Restarted after a crash or a config error, but not after a clean stop.
	key("KeepAlive")
	b.WriteString("\t<dict>\n\t\t<key>SuccessfulExit</key>\n\t\t<false/>\n\t</dict>\n")
	key("ThrottleInterval")
	b.WriteString("\t<integer>60</integer>\n")
	// Notifications, the app's call socket and the gh keychain belong to the logged-in GUI session.
	key("LimitLoadToSessionType")
	str("\t", "Aqua")
	key("ProcessType")
	str("\t", a.ProcessType)
	key("StandardOutPath")
	str("\t", a.Log)
	key("StandardErrorPath")
	str("\t", a.Log)
	b.WriteString("</dict>\n</plist>\n")
	return b.Bytes()
}

func domain() string {
	return fmt.Sprintf("gui/%d", os.Getuid())
}

// Run prints and runs a command, passing its output through.
func Run(out io.Writer, name string, args ...string) error {
	if err := printCommand(out, name, args); err != nil {
		return err
	}
	cmd := exec.Command(name, args...)
	cmd.Stdout, cmd.Stderr = out, os.Stderr
	return cmd.Run()
}

// RunQuiet is for a check whose full output is long, so only its failure is shown.
func RunQuiet(out io.Writer, name string, args ...string) error {
	if err := printCommand(out, name, args); err != nil {
		return err
	}
	var stderr bytes.Buffer
	cmd := exec.Command(name, args...)
	cmd.Stderr = &stderr
	if err := cmd.Run(); err != nil {
		_, _ = os.Stderr.Write(stderr.Bytes())
		return err
	}
	return nil
}

func printCommand(out io.Writer, name string, args []string) error {
	_, err := fmt.Fprintln(out, "$", strings.Join(append([]string{name}, args...), " "))
	return err
}
