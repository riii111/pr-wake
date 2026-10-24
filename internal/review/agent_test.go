package review

import (
	"io"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/riii111/pr-wake/internal/launchagent"
)

// The old agent used the paths under home, while the installing shell may point XDG elsewhere.
func useOtherXDGHomes(t *testing.T, home string) {
	t.Helper()
	t.Setenv("XDG_CONFIG_HOME", filepath.Join(home, "xdg-config"))
	t.Setenv("XDG_STATE_HOME", filepath.Join(home, "xdg-state"))
}

func TestLegacyConfigAndStateAreCarriedOverOnce(t *testing.T) {
	home := t.TempDir()
	useOtherXDGHomes(t, home)
	env := Env{Home: home}
	write := func(path, data string) {
		t.Helper()
		if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(path, []byte(data), 0o600); err != nil {
			t.Fatal(err)
		}
	}
	write(filepath.Join(home, ".config", "review-notifier", "config.json"), "old config")
	write(filepath.Join(home, ".local", "state", "review-notifier", "state.json"), "old state")
	write(legacyBinary(env), "binary")

	if err := copyIfMissing(legacyConfigPath(home), defaultConfigPath(home), io.Discard); err != nil {
		t.Fatal(err)
	}
	if err := replaceLegacy(env, io.Discard); err != nil {
		t.Fatal(err)
	}
	if data, _ := os.ReadFile(filepath.Join(home, "xdg-config", "pr-wake", "review.json")); string(data) != "old config" {
		t.Fatalf("config = %q", data)
	}
	if data, _ := os.ReadFile(filepath.Join(home, "xdg-state", "pr-wake", "review", "state.json")); string(data) != "old state" {
		t.Fatalf("state = %q", data)
	}
	if _, err := os.Stat(legacyBinary(env)); !os.IsNotExist(err) {
		t.Fatalf("legacy binary is still there: %v", err)
	}

	write(defaultConfigPath(home), "new config")
	if err := copyIfMissing(legacyConfigPath(home), defaultConfigPath(home), io.Discard); err != nil {
		t.Fatal(err)
	}
	if data, _ := os.ReadFile(defaultConfigPath(home)); string(data) != "new config" {
		t.Fatalf("config = %q", data)
	}
}

func TestLegacyStateMovesOnlyAfterTheOldWatcherReleasesItsLock(t *testing.T) {
	home := t.TempDir()
	useOtherXDGHomes(t, home)
	env := Env{Home: home}
	old := filepath.Join(home, ".local", "state", "review-notifier")
	held, err := lockDir(old)
	if err != nil {
		t.Fatal(err)
	}
	// The old watcher saves once more while it exits, then releases the lock.
	go func() {
		time.Sleep(300 * time.Millisecond)
		_ = os.WriteFile(filepath.Join(old, "state.json"), []byte("final state"), 0o600)
		_ = held.Close()
	}()

	if err := moveLegacyState(env, io.Discard); err != nil {
		t.Fatal(err)
	}
	if data, _ := os.ReadFile(filepath.Join(defaultStateDir(home), "state.json")); string(data) != "final state" {
		t.Fatalf("state = %q", data)
	}
	lock, err := lockDir(defaultStateDir(home))
	if err != nil {
		t.Fatalf("the new watcher cannot lock its state: %v", err)
	}
	_ = lock.Close()
}

func TestInstalledAgentRunsWithTheXDGHomesInstallUsed(t *testing.T) {
	t.Setenv("XDG_CONFIG_HOME", "/Users/me/conf")
	t.Setenv("XDG_STATE_HOME", "/Users/me/st")
	plist := string(launchagent.Render(installedAgent(Env{Home: "/Users/me"}, "/Users/me/.local/bin/pr-wake"), "me"))
	for _, want := range []string{
		"<string>/Users/me/.local/bin/pr-wake</string>\n\t\t<string>review</string>\n\t\t<string>run</string>",
		"<key>XDG_CONFIG_HOME</key>\n\t\t<string>/Users/me/conf</string>",
		"<key>XDG_STATE_HOME</key>\n\t\t<string>/Users/me/st</string>",
	} {
		if !strings.Contains(plist, want) {
			t.Errorf("plist has no %q:\n%s", want, plist)
		}
	}

	t.Setenv("XDG_CONFIG_HOME", "relative")
	t.Setenv("XDG_STATE_HOME", "")
	if env := installedAgent(Env{Home: "/Users/me"}, "pr-wake").Env; len(env) != 0 {
		t.Fatalf("env = %v; a home the code ignores must not be passed on", env)
	}
}
