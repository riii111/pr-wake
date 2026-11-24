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
