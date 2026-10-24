package launchagent

import (
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
)

func TestRenderWritesAValidPlist(t *testing.T) {
	a := Agent{
		Label:       "io.example.agent",
		Home:        "/Users/me",
		Program:     []string{"/Users/me/bin/tool", "run", "a&b"},
		Log:         "/Users/me/Library/Logs/tool.log",
		ProcessType: "Interactive",
	}
	plist := string(Render(a, "me"))
	for _, want := range []string{
		"<string>a&amp;b</string>",
		"<string>/Users/me/.nix-profile/bin:/etc/profiles/per-user/me/bin:",
		"<key>LimitLoadToSessionType</key>\n\t<string>Aqua</string>",
		"<key>ProcessType</key>\n\t<string>Interactive</string>",
	} {
		if !strings.Contains(plist, want) {
			t.Errorf("plist has no %q:\n%s", want, plist)
		}
	}

	plutil, err := exec.LookPath("plutil")
	if err != nil {
		t.Skip("plutil not found")
	}
	path := filepath.Join(t.TempDir(), "agent.plist")
	if err := os.WriteFile(path, []byte(plist), 0o644); err != nil {
		t.Fatal(err)
	}
	if out, err := exec.Command(plutil, "-lint", path).CombinedOutput(); err != nil {
		t.Fatalf("plutil -lint: %v: %s", err, out)
	}
}
