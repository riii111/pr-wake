package watch

import (
	"reflect"
	"strings"
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

func TestStatusLinesReadTheStatusHarnexusTaskPrints(t *testing.T) {
	output := `{"prs": [
		{"pr": "https://github.com/org/pay/pull/7", "status": "watching", "reason": null, "pending": 2, "awaitingAnswer": true},
		{"pr": "https://github.com/org/pay/pull/8", "status": "stopped", "reason": "worker thread is gone", "pending": 0, "awaitingAnswer": false}
	]}`
	got := statusLines([]byte(output))
	want := []string{
		"https://github.com/org/pay/pull/7 watching pending=2 awaiting answer",
		"https://github.com/org/pay/pull/8 stopped (worker thread is gone) pending=0",
	}
	if !reflect.DeepEqual(got[:len(got)-1], want) {
		t.Fatalf("lines = %q, want %q", got, want)
	}
}

func TestStatusLinesShowOtherOutputAsItIs(t *testing.T) {
	if got := statusLines([]byte("not json\n")); !reflect.DeepEqual(got, []string{"not json"}) {
		t.Fatalf("lines = %q", got)
	}
}

func TestSenderLineNamesTheSenderAnUnreadableRecordOrHowToRegisterOne(t *testing.T) {
	if got := statusLines([]byte(`{"sender": "chat-1", "prs": []}`)); !reflect.DeepEqual(got, []string{"sender: chat-1"}) {
		t.Fatalf("lines = %q", got)
	}
	if got := senderLine(shown{SenderError: "bad JSON"}); got != "sender: unreadable (bad JSON)" {
		t.Fatalf("line = %q", got)
	}
	if got := senderLine(shown{}); !strings.Contains(got, "harnexus-task watch --sender") {
		t.Fatalf("line = %q", got)
	}
}
