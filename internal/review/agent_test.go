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

func TestLegacyConfigAndStateAreCarriedOverOnce(t *testing.T) {
	home := t.TempDir()
	t.Setenv("XDG_CONFIG_HOME", "")
	t.Setenv("XDG_STATE_HOME", "")
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
	write(legacyConfigPath(home), "old config")
	write(filepath.Join(legacyStateDir(home), "state.json"), "old state")
	write(legacyBinary(env), "binary")

	if err := copyIfMissing(legacyConfigPath(home), defaultConfigPath(home), io.Discard); err != nil {
		t.Fatal(err)
	}
	if err := replaceLegacy(env, io.Discard); err != nil {
		t.Fatal(err)
	}
	if data, _ := os.ReadFile(defaultConfigPath(home)); string(data) != "old config" {
		t.Fatalf("config = %q", data)
	}
	if data, _ := os.ReadFile(filepath.Join(defaultStateDir(home), "state.json")); string(data) != "old state" {
		t.Fatalf("state = %q", data)
	}
	if _, err := os.Stat(legacyBinary(env)); !os.IsNotExist(err) {
		t.Fatalf("legacy binary is still there: %v", err)
	}

	// A config edited after the move is never overwritten by the old one.
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
	t.Setenv("XDG_STATE_HOME", "")
	env := Env{Home: home}
	old := legacyStateDir(home)
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

func TestAgentReadsTheXDGHomesInstallUsed(t *testing.T) {
	t.Setenv("XDG_CONFIG_HOME", "/Users/me/conf")
	t.Setenv("XDG_STATE_HOME", "/Users/me/st")
	plist := string(launchagent.Render(agent(Env{Home: "/Users/me"}, "/Users/me/.local/bin/pr-wake"), "me"))
	for _, want := range []string{
		"<key>XDG_CONFIG_HOME</key>\n\t\t<string>/Users/me/conf</string>",
		"<key>XDG_STATE_HOME</key>\n\t\t<string>/Users/me/st</string>",
	} {
		if !strings.Contains(plist, want) {
			t.Errorf("plist has no %q:\n%s", want, plist)
		}
	}

	t.Setenv("XDG_CONFIG_HOME", "relative")
	t.Setenv("XDG_STATE_HOME", "")
	if env := agent(Env{Home: "/Users/me"}, "pr-wake").Env; len(env) != 0 {
		t.Fatalf("env = %v; a home the code ignores must not be passed on", env)
	}
}
