package watch

import (
	"reflect"
	"testing"
)

func TestAgentRunsHarnexusTaskWatch(t *testing.T) {
	a := agent("/Users/me")
	if want := []string{"/Users/me/.local/bin/harnexus-task", "watch"}; !reflect.DeepEqual(a.Program, want) {
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
	register := "sender: none; run `harnexus-task watch --sender` in a chat kept for it"
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
