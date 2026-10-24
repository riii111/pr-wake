package main

import (
	"bytes"
	"context"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"syscall"
	"time"
)

// Runner starts external commands with an argument list and never through a shell.
type Runner interface {
	Run(ctx context.Context, name string, args ...string) ([]byte, error)
}

type execRunner struct{}

func (execRunner) Run(ctx context.Context, name string, args ...string) ([]byte, error) {
	cmd := exec.CommandContext(ctx, name, args...)
	// alerter removes its notification on SIGTERM; a kill would leave it on screen.
	cmd.Cancel = func() error { return cmd.Process.Signal(syscall.SIGTERM) }
	cmd.WaitDelay = 5 * time.Second
	var stderr bytes.Buffer
	cmd.Stderr = &stderr
	out, err := cmd.Output()
	if err != nil {
		msg := strings.TrimSpace(stderr.String())
		if msg == "" {
			return out, fmt.Errorf("%s: %w", filepath.Base(name), err)
		}
		return out, fmt.Errorf("%s: %w: %s", filepath.Base(name), err, msg)
	}
	return out, nil
}

// launchd does not give the agent the login shell's PATH, so commands are looked up in fixed directories.
func searchDirs(home, user string) []string {
	return []string{
		filepath.Join(home, ".nix-profile", "bin"),
		filepath.Join("/etc/profiles/per-user", user, "bin"),
		"/run/current-system/sw/bin",
		"/nix/var/nix/profiles/default/bin",
		"/opt/homebrew/bin",
		"/usr/local/bin",
		"/usr/bin",
	}
}

func findCommand(name string, dirs []string) (string, error) {
	for _, dir := range dirs {
		path := filepath.Join(dir, name)
		if info, err := os.Stat(path); err == nil && !info.IsDir() && info.Mode()&0o111 != 0 {
			return path, nil
		}
	}
	return "", fmt.Errorf("%s not found in %s; set commands.%s in the config", name, strings.Join(dirs, ", "), name)
}
