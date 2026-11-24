use std::{
    fs, iter,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use rstest::rstest;
use serde_json::{Value, json};

const PR_WAKE: &str = env!("CARGO_BIN_EXE_pr-wake");

const PR_9: &str = "https://github.com/org/pay/pull/9";

struct Home {
    root: tempfile::TempDir,
}

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

#[test]
fn help_shows_both_features_and_succeeds() {
    let output = run(&["--help"]);

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 help");
    assert!(stdout.contains("Usage: pr-wake <COMMAND>"), "{stdout}");
    assert!(stdout.contains("review"), "{stdout}");
    assert!(stdout.contains("watch"), "{stdout}");
    assert_eq!(output.stderr, *b"");
}

#[test]
fn wrong_command_line_is_a_usage_error_on_stderr() {
    let output = run(&[]);

    assert_eq!(output.status.code(), Some(2));
    assert_eq!(output.stdout, *b"");
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 error");
    assert!(stderr.contains("Usage: pr-wake"), "{stderr}");
}

#[test]
fn calling_chat_is_recorded_as_the_sender_and_a_chat_none_can_send_to_is_warned() {
    let home = Home::new();

    let unsent = home.watch(&["link", PR_9], &[]);
    let registered = home.watch(&["sender"], &[("CODEX_THREAD_ID", "sender-chat")]);
    let linked = home.watch(&["link", PR_9], &[]);
    let not_pr = home.watch(&["link", "9"], &[]);
    let shown = home.watch(&["status", "--json"], &[]);

    assert_eq!(
        unsent.json(),
        json!({
            "threadId": "caller",
            "watched": PR_9,
            "warning": "no chat is registered to send from: run pr-wake watch sender in a chat kept for it",
        })
    );
    assert_eq!(registered.json(), json!({ "sender": "sender-chat" }));
    assert_eq!(
        linked.json(),
        json!({ "threadId": "caller", "watched": PR_9 })
    );
    assert_eq!(not_pr.code, Some(1));
    assert!(
        not_pr
            .stderr
            .contains("pr-wake: 9 is not watched: the PR watch needs the URL"),
        "{}",
        not_pr.stderr
    );
    let shown = shown.json();
    assert_eq!(shown["sender"], "sender-chat");
    assert_eq!(shown["prs"][0]["worker"], "caller");
    assert_eq!(shown["prs"].as_array().map(Vec::len), Some(1));
}

#[test]
fn result_is_one_json_line_on_stdout() {
    let home = Home::new();

    let linked = home.watch(&["link", PR_9], &[("CODEX_THREAD_ID", "caller")]);

    assert_eq!(linked.code, Some(0));
    assert!(linked.stdout.ends_with("}\n"), "{:?}", linked.stdout);
    assert_eq!(linked.stdout.lines().count(), 1);
    assert!(linked.stderr.is_empty(), "{}", linked.stderr);
}

#[test]
fn t3_code_thread_is_linked_as_a_t3_code_worker_which_needs_no_sender() {
    let home = Home::new();
    let t3 = [("T3CODE_THREAD_ID", "mcp:w1")];

    let linked = home.watch(&["link", PR_9], &t3);
    let sender = home.watch(&["sender"], &t3);
    let shown = home.watch(&["status", "--json"], &t3);

    assert_eq!(
        linked.json(),
        json!({ "threadId": "t3:mcp:w1", "watched": PR_9 })
    );
    assert_eq!(sender.code, Some(1));
    assert!(
        sender.stderr.contains("without a sender"),
        "{}",
        sender.stderr
    );
    assert_eq!(shown.json()["prs"][0]["worker"], "t3:mcp:w1");
}

#[test]
fn cloud_session_is_refused() {
    let refused = Home::new().watch(
        &["status", "--json"],
        &[("HARNEXUS_EXECUTION_ENV", "cloud")],
    );

    assert_eq!(refused.code, Some(1));
    assert!(
        refused.stderr.contains("unavailable in Cloud"),
        "{}",
        refused.stderr
    );
}

#[rstest]
#[case::watch_run(None)]
#[case::engine_link(Some("pr-wake-engine"))]
fn watch_ends_before_it_can_send_when_harnexus_cannot_be_asked(#[case] link: Option<&str>) {
    let home = Home::new();
    home.watch(&["sender"], &[("CODEX_THREAD_ID", "sender-chat")]);
    home.watch(&["link", PR_9], &[]);
    let socket = home.root.path().join("missing.sock");
    let program = link.map_or_else(
        || PathBuf::from(PR_WAKE),
        |name| {
            let linked = home.root.path().join(name);
            symlink(PR_WAKE, &linked).expect("link the engine name");
            linked
        },
    );
    let arguments = if link.is_some() {
        ["watch"].as_slice()
    } else {
        ["watch", "run"].as_slice()
    };

    let refused = home.run(
        &program,
        arguments,
        &[(
            "HARNEXUS_CALL_SOCKET",
            socket.to_str().expect("a UTF-8 path"),
        )],
    );

    assert_eq!(refused.code, Some(1));
    assert!(
        refused
            .stderr
            .contains("cannot use the harnexus call socket"),
        "{}",
        refused.stderr
    );
    assert!(
        !refused.stdout.contains("\"started\""),
        "{}",
        refused.stdout
    );
    assert!(!home.state().join("watch.lock").exists());
}

#[test]
fn status_shows_an_answer_awaited_for_the_recorded_delivery() {
    let home = Home::new();
    home.watch(&["sender"], &[("CODEX_THREAD_ID", "sender-chat")]);
    home.watch(&["link", PR_9], &[]);
    home.write_pr_state(&pr_state(&[(
        "delivered",
        json!({
            "at": "2026-10-23T12:00:00.000Z",
            "sender": "sender-chat",
            "prompt": format!("$task-worker\n{PR_9}#discussion_r1"),
            "findings": [],
            "followUp": false,
            "events": [],
            "lost": false,
        }),
    )]));

    let shown = home.watch(&["status", "--json"], &[]);

    assert_eq!(shown.code, Some(0), "{}", shown.stderr);
    // Other programs parse this line, so its keys and their order are compared as written.
    assert_eq!(
        shown.stdout,
        concat!(
            r#"{"sender":"sender-chat","prs":[{"pr":"https://github.com/org/pay/pull/9","worker":"caller","status":"watching","reason":null,"pending":0,"followUp":0,"awaitingAnswer":true,"handingOver":false,"checkedAt":"2026-10-23T12:00:00.000Z"}]}"#,
            "\n"
        )
    );
}

#[test]
fn halted_pr_is_released_with_one_json_line() {
    let home = Home::new();
    home.watch(&["link", PR_9], &[]);
    home.write_pr_state(&pr_state(&[
        ("status", json!("halted")),
        ("reason", json!("the delivery could not be confirmed")),
        (
            "pending",
            json!([{ "type": "conflict", "head": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" }]),
        ),
        ("halted", json!({ "prompt": "$task-worker" })),
    ]));

    let released = home.watch(&["release", PR_9], &[]);

    assert_eq!(released.code, Some(0), "{}", released.stderr);
    assert_eq!(
        released.stdout,
        concat!(
            r#"{"released":"https://github.com/org/pay/pull/9","worker":"caller"}"#,
            "\n"
        )
    );
    let state = home.pr_state();
    assert_eq!(
        [&state["status"], &state["halted"], &state["pending"]],
        [&json!("watching"), &Value::Null, &json!([])]
    );
}

impl Home {
    fn new() -> Self {
        let home = Self {
            root: tempfile::tempdir().expect("create a directory"),
        };
        home.write_config(&json!({ "repositories": { "org/pay": { "account": "owner" } } }));
        home
    }

    fn write_config(&self, config: &Value) {
        let path = self.root.path().join("config/harnexus/pr-watch.json");
        fs::create_dir_all(path.parent().expect("a parent")).expect("create the directory");
        fs::write(path, config.to_string()).expect("write the config");
    }

    fn state(&self) -> PathBuf {
        self.root.path().join("state/harnexus-crew/watch")
    }

    fn write_pr_state(&self, state: &Value) {
        fs::write(self.pr_directory().join("state.json"), state.to_string())
            .expect("write the state");
    }

    fn pr_state(&self) -> Value {
        let state =
            fs::read_to_string(self.pr_directory().join("state.json")).expect("read the state");
        serde_json::from_str(&state).expect("parse the state")
    }

    // Named by a hash the binary keeps to itself, so the one directory `watch link` made is found rather than computed.
    fn pr_directory(&self) -> PathBuf {
        let mut directories = fs::read_dir(self.state())
            .expect("list the records")
            .map(|entry| entry.expect("read an entry").path())
            .filter(|path| path.is_dir());
        let directory = directories.next().expect("a linked PR");
        assert!(directories.next().is_none(), "more than one linked PR");
        directory
    }

    fn watch(&self, arguments: &[&str], env: &[(&str, &str)]) -> Run {
        let arguments = iter::once("watch")
            .chain(arguments.iter().copied())
            .collect::<Vec<_>>();
        self.run(Path::new(PR_WAKE), &arguments, env)
    }

    // Pinned to a local run from the chat `caller`, so the ambient environment cannot change the result.
    fn run(&self, program: &Path, arguments: &[&str], env: &[(&str, &str)]) -> Run {
        let mut command = Command::new(program);
        command
            .args(arguments)
            .env_clear()
            .env("HOME", self.root.path())
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_STATE_HOME", self.root.path().join("state"))
            .env("HARNEXUS_EXECUTION_ENV", "local")
            .env("CODEX_THREAD_ID", "caller")
            .envs(env.iter().copied());
        let output = command.output().expect("run pr-wake");
        Run {
            code: output.status.code(),
            stdout: String::from_utf8(output.stdout).expect("UTF-8 output"),
            stderr: String::from_utf8(output.stderr).expect("UTF-8 error"),
        }
    }
}

impl Run {
    fn json(&self) -> Value {
        assert_eq!(self.code, Some(0), "{}", self.stderr);
        serde_json::from_str(&self.stdout).expect("one JSON line")
    }
}

fn run(arguments: &[&str]) -> Output {
    Command::new(PR_WAKE)
        .args(arguments)
        .output()
        .expect("run pr-wake")
}

// Every field the watch writes is present, as a record missing one is refused rather than read as empty.
fn pr_state(fields: &[(&str, Value)]) -> Value {
    let mut state = json!({
        "status": "watching",
        "reason": null,
        "summary": null,
        "checkedAt": "2026-10-23T12:00:00.000Z",
        "changedAt": null,
        "seen": [],
        "pending": [],
        "followUp": [],
        "conflictHead": null,
        "delivered": null,
        "worker": "caller",
        "halted": null,
        "lostSince": null,
    });
    for (name, value) in fields {
        state[*name] = value.clone();
    }
    state
}
