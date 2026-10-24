package review

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"io"
	"io/fs"
	"os"
	"os/user"
	"path/filepath"

	"github.com/riii111/pr-wake/internal/launchagent"
)

func agent(env Env, exe string) launchagent.Agent {
	return launchagent.Agent{
		Label:       "io.github.riii111.pr-wake.review",
		Command:     "review",
		Home:        env.Home,
		Program:     []string{exe, "review", "run"},
		Log:         filepath.Join(env.Home, "Library", "Logs", "pr-wake", "review.log"),
		ProcessType: "Interactive",
	}
}

// review-notifier from dotfiles, replaced on install so two watchers never notify the same request.
func legacyAgent(env Env) launchagent.Agent {
	return launchagent.Agent{Label: "io.github.riii111.review-notifier", Home: env.Home}
}

func legacyBinary(env Env) string {
	return filepath.Join(env.Home, ".local", "bin", "review-notifier")
}

func install(ctx context.Context, env Env, stdout io.Writer) error {
	exe, err := executable()
	if err != nil {
		return err
	}
	if err := replaceLegacy(env, stdout); err != nil {
		return err
	}
	config := defaultConfigPath(env.Home)
	if _, err := os.Stat(config); errors.Is(err, fs.ErrNotExist) {
		return fmt.Errorf("%s not found; copy config.example.json from the pr-wake repository and edit it", config)
	}
	// Fails before the agent is loaded if the config, a command or the GitHub API is not usable.
	if err := runCheck(ctx, nil, env, execRunner{}, stdout); err != nil {
		return err
	}
	u, err := user.Current()
	if err != nil {
		return err
	}
	return launchagent.Install(agent(env, exe), u.Username, stdout)
}

// Stops review-notifier and carries its config and state over; worktrees it made stay where they are.
func replaceLegacy(env Env, stdout io.Writer) error {
	old := legacyAgent(env)
	if _, err := os.Stat(old.Plist()); err == nil {
		if err := launchagent.Uninstall(old, stdout); err != nil {
			return err
		}
	}
	if err := os.Remove(legacyBinary(env)); err == nil {
		fmt.Fprintf(stdout, "removed %s\n", legacyBinary(env))
	}
	if err := copyIfMissing(legacyConfigPath(env.Home), defaultConfigPath(env.Home), stdout); err != nil {
		return err
	}
	return moveIfMissing(legacyStateDir(env.Home), defaultStateDir(env.Home), stdout)
}

func copyIfMissing(from, to string, stdout io.Writer) error {
	if _, err := os.Stat(to); err == nil {
		return nil
	}
	data, err := os.ReadFile(from)
	if errors.Is(err, fs.ErrNotExist) {
		return nil
	} else if err != nil {
		return err
	}
	if err := os.MkdirAll(filepath.Dir(to), 0o755); err != nil {
		return err
	}
	if err := os.WriteFile(to, data, 0o644); err != nil {
		return err
	}
	fmt.Fprintf(stdout, "copied %s to %s\n", from, to)
	return nil
}

func moveIfMissing(from, to string, stdout io.Writer) error {
	if _, err := os.Stat(to); err == nil {
		return nil
	}
	if _, err := os.Stat(from); errors.Is(err, fs.ErrNotExist) {
		return nil
	}
	if err := os.MkdirAll(filepath.Dir(to), 0o700); err != nil {
		return err
	}
	if err := os.Rename(from, to); err != nil {
		return err
	}
	fmt.Fprintf(stdout, "moved %s to %s\n", from, to)
	return nil
}

func uninstall(env Env, stdout io.Writer) error {
	exe, err := executable()
	if err != nil {
		return err
	}
	if err := launchagent.Uninstall(agent(env, exe), stdout); err != nil {
		return err
	}
	fmt.Fprintln(stdout, "config, state, logs and review worktrees are kept")
	return nil
}

func start(env Env, stdout io.Writer) error {
	exe, err := executable()
	if err != nil {
		return err
	}
	return launchagent.Start(agent(env, exe), stdout)
}

func stop(env Env, stdout io.Writer) error {
	exe, err := executable()
	if err != nil {
		return err
	}
	return launchagent.Stop(agent(env, exe), stdout)
}

func runAgentStatus(env Env, stdout io.Writer) error {
	exe, err := executable()
	if err != nil {
		return err
	}
	launchagent.PrintLoaded(agent(env, exe), stdout)
	return runStatus(env, stdout)
}

func logs(args []string, env Env, stdout io.Writer) error {
	flags := flag.NewFlagSet("logs", flag.ContinueOnError)
	follow := flags.Bool("f", false, "follow the log")
	if err := flags.Parse(args); err != nil {
		return err
	}
	exe, err := executable()
	if err != nil {
		return err
	}
	return launchagent.Logs(agent(env, exe), *follow, stdout)
}

// The agent runs the binary that installed it, so a rebuild at the same path takes effect on restart.
func executable() (string, error) {
	return os.Executable()
}
