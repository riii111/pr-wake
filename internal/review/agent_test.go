package review

import (
	"io"
	"os"
	"path/filepath"
	"testing"
)

func TestReplaceLegacyCarriesConfigAndStateOverOnce(t *testing.T) {
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
	if err := replaceLegacy(env, io.Discard); err != nil {
		t.Fatal(err)
	}
	if data, _ := os.ReadFile(defaultConfigPath(home)); string(data) != "new config" {
		t.Fatalf("config = %q", data)
	}
}
