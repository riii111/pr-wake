package watch

import (
	"bytes"
	"errors"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
)

func TestAgentRunsTheBundledEngine(t *testing.T) {
	a := agent("/Users/me", "/Users/me/.local/bin/pr-wake-engine")
	if want := []string{"/Users/me/.local/bin/pr-wake-engine", "watch"}; !reflect.DeepEqual(a.Program, want) {
		t.Fatalf("program = %v, want %v", a.Program, want)
	}
	if want := "/Users/me/Library/LaunchAgents/io.github.riii111.pr-wake.watch.plist"; a.Plist() != want {
		t.Fatalf("plist = %s, want %s", a.Plist(), want)
	}
	if a.ProcessType != "Background" {
		t.Fatalf("process type = %s", a.ProcessType)
	}
}

func TestStatusLines(t *testing.T) {
	register := "sender: none; run `pr-wake watch sender` in a chat kept for it"
	for _, tc := range []struct {
		name   string
		output string
		want   []string
	}{
		{
			name: "watched PRs",
			output: `{"prs": [
				{"pr": "https://github.com/org/pay/pull/7", "status": "watching", "reason": null, "pending": 2, "awaitingAnswer": true},
				{"pr": "https://github.com/org/pay/pull/8", "status": "stopped", "reason": "worker thread is gone", "pending": 0, "awaitingAnswer": false}
			]}`,
			want: []string{
				"https://github.com/org/pay/pull/7 watching pending=2 awaiting answer",
				"https://github.com/org/pay/pull/8 stopped (worker thread is gone) pending=0",
				register,
			},
		},
		{name: "output that is not JSON", output: "not json\n", want: []string{"not json"}},
		{name: "sender", output: `{"sender": "chat-1", "prs": []}`, want: []string{"sender: chat-1"}},
		{name: "unreadable sender", output: `{"senderError": "bad JSON", "prs": []}`, want: []string{"sender: unreadable (bad JSON)"}},
		{name: "no sender", output: `{"prs": []}`, want: []string{register}},
	} {
		t.Run(tc.name, func(t *testing.T) {
			if got := statusLines([]byte(tc.output)); !reflect.DeepEqual(got, tc.want) {
				t.Fatalf("lines = %q, want %q", got, tc.want)
			}
		})
	}
}

func TestPassthroughGivesTheEngineItsFlags(t *testing.T) {
	pr := "https://github.com/org/pay/pull/7"
	for _, tc := range []struct {
		args []string
		want []string
	}{
		{[]string{"sender"}, []string{"watch", "--sender"}},
		{[]string{"link", pr}, []string{"watch", "--link", pr}},
		{[]string{"link", pr, "--take-over"}, []string{"watch", "--link", pr, "--take-over"}},
		{[]string{"link", "--take-over", pr}, []string{"watch", "--link", pr, "--take-over"}},
		{[]string{"release", pr}, []string{"watch", "--release", pr}},
		{[]string{"dry-run"}, []string{"watch", "--dry-run"}},
		{[]string{"dry-run", "--pr", pr, "--pr", "x"}, []string{"watch", "--dry-run", "--pr", pr, "--pr", "x"}},
	} {
		got, err := passthrough(tc.args)
		if err != nil || !reflect.DeepEqual(got, tc.want) {
			t.Errorf("passthrough(%q) = %q, %v; want %q", tc.args, got, err, tc.want)
		}
	}
	for _, args := range [][]string{
		{"sender", "extra"},
		{"link"},
		{"link", pr, pr},
		{"release"},
		{"dry-run", pr},
		{"unknown"},
	} {
		if _, err := passthrough(args); err == nil {
			t.Errorf("passthrough(%q) accepted", args)
		}
	}
}

func TestRunEnginePassesItsOutputAndExitCodeThrough(t *testing.T) {
	engine := filepath.Join(t.TempDir(), "pr-wake-engine")
	script := "#!/bin/sh\necho \"$*\"\necho refused >&2\nexit 2\n"
	if err := os.WriteFile(engine, []byte(script), 0o755); err != nil {
		t.Fatal(err)
	}
	var out bytes.Buffer
	err := runEngine(engine, []string{"watch", "--link", "x"}, &out)
	var exit *ExitCode
	if !errors.As(err, &exit) || exit.Code != 2 {
		t.Fatalf("err = %v, want exit code 2", err)
	}
	if got := out.String(); got != "watch --link x\n" {
		t.Fatalf("stdout = %q", got)
	}
}

func TestRunEngineNamesTheInstallerWhenTheEngineIsMissing(t *testing.T) {
	err := runEngine(filepath.Join(t.TempDir(), "pr-wake-engine"), []string{"watch"}, &bytes.Buffer{})
	if err == nil || !strings.Contains(err.Error(), "scripts/install.sh") {
		t.Fatalf("err = %v", err)
	}
}
