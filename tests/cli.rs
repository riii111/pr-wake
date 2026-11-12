use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use rstest::rstest;
use serde_json::{Value, json};

const ENGINE: &str = env!("CARGO_BIN_EXE_pr-wake-engine");

const PR_9: &str = "https://github.com/org/pay/pull/9";

// A home of its own for each run, with the environment the engine reads and nothing else.
struct Home {
    root: tempfile::TempDir,
}

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

#[test]
fn help_shows_the_watch_usage_and_succeeds() {
    let output = run(&["--help"]);

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 help");
    assert!(stdout.contains("Usage: pr-wake-engine"), "{stdout}");
    assert!(
        stdout.contains("pr-wake-engine watch [--dry-run"),
        "{stdout}"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn version_names_the_engine() {
    let output = run(&["--version"]);

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 version");
    assert_eq!(
        stdout,
        format!("pr-wake-engine {}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[rstest]
#[case::no_arguments(&[])]
#[case::another_subcommand(&["review"])]
#[case::stray_argument(&["watch", "status"])]
fn command_line_without_a_watch_command_is_a_usage_error(#[case] arguments: &[&str]) {
    let output = run(arguments);

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 error");
    assert!(stderr.contains("Usage: pr-wake-engine"), "{stderr}");
}

#[test]
fn wrong_watch_flags_are_a_usage_error_naming_what_is_wrong() {
    for (name, arguments, says) in [
        (
            "pr_without_dry_run",
            &["--pr", "https://github.com/org/pay/pull/1"][..],
            "--pr is only for --dry-run",
        ),
        (
            "release_with_dry_run",
            &[
                "--dry-run",
                "--release",
                "https://github.com/org/pay/pull/1",
            ],
            "use only one of --dry-run, --release, --status, --sender and --link",
        ),
        (
            "status_with_release",
            &["--status", "--release", "https://github.com/org/pay/pull/1"],
            "use only one of --dry-run, --release, --status, --sender and --link",
        ),
        (
            "take_over_without_link",
            &["--take-over"],
            "--take-over is only for --link",
        ),
        ("unknown_option", &["--nope"], "--nope"),
    ] {
        let refused = Home::new().watch(arguments, &[]);

        assert_eq!(refused.code, Some(2), "case: {name}");
        assert!(refused.stdout.is_empty(), "case: {name}");
        assert!(
            refused.stderr.contains(says),
            "case: {name}: {}",
            refused.stderr
        );
        assert!(
            refused.stderr.contains("Usage: pr-wake-engine watch"),
            "case: {name}: {}",
            refused.stderr
        );
    }
}

#[test]
fn calling_chat_is_recorded_as_the_sender_and_a_chat_none_can_send_to_is_warned() {
    let home = Home::new();

    let unsent = home.watch(&["--link", PR_9], &[]);
    let registered = home.watch(&["--sender"], &[("CODEX_THREAD_ID", "sender-chat")]);
    let linked = home.watch(&["--link", PR_9], &[]);
    let not_pr = home.watch(&["--link", "9"], &[]);
    let shown = home.watch(&["--status"], &[]);

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
            .contains("pr-wake-engine: 9 is not watched: the PR watch needs the URL"),
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

    let linked = home.watch(&["--link", PR_9], &[("CODEX_THREAD_ID", "caller")]);

    assert_eq!(linked.code, Some(0));
    assert!(linked.stdout.ends_with("}\n"), "{:?}", linked.stdout);
    assert_eq!(linked.stdout.lines().count(), 1);
    assert!(linked.stderr.is_empty(), "{}", linked.stderr);
}

#[rstest]
#[case::no_chat_id("")]
#[case::chat_id_with_a_space("a b")]
#[case::unconfirmed_chat_id("client-new-thread:1")]
fn link_from_a_chat_without_a_confirmed_id_is_refused(#[case] id: &str) {
    let refused = Home::new().watch(&["--link", PR_9], &[("CODEX_THREAD_ID", id)]);

    assert_eq!(refused.code, Some(1));
    assert!(refused.stdout.is_empty());
    assert!(
        refused.stderr.contains("CODEX_THREAD_ID"),
        "{}",
        refused.stderr
    );
}

#[test]
fn t3_code_thread_is_linked_as_a_t3_code_worker_which_needs_no_sender() {
    let home = Home::new();
    // T3 Code runs Codex with a CODEX_THREAD_ID of Codex's own.
    let t3 = [("T3CODE_THREAD_ID", "mcp:w1")];

    let linked = home.watch(&["--link", PR_9], &t3);
    let sender = home.watch(&["--sender"], &t3);
    let shown = home.watch(&["--status"], &t3);

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

#[rstest]
#[case::explicit_cloud(&[("HARNEXUS_EXECUTION_ENV", "cloud")])]
#[case::claude_code_remote(&[("HARNEXUS_EXECUTION_ENV", ""), ("CLAUDE_CODE_REMOTE", "true")])]
#[case::codex_environment(&[("HARNEXUS_EXECUTION_ENV", ""), ("CODEX_ENVIRONMENT_ID", "env")])]
fn cloud_session_is_refused(#[case] cloud: &[(&str, &str)]) {
    let refused = Home::new().watch(&["--status"], cloud);

    assert_eq!(refused.code, Some(1));
    assert!(
        refused.stderr.contains("unavailable in Cloud"),
        "{}",
        refused.stderr
    );
}

#[test]
fn watch_ends_before_it_can_send_when_harnexus_cannot_be_asked() {
    let home = Home::new();
    home.watch(&["--sender"], &[("CODEX_THREAD_ID", "sender-chat")]);
    home.watch(&["--link", PR_9], &[]);
    let socket = home.root.path().join("missing.sock");

    let refused = home.watch(
        &[],
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

// The records harnexus-task watch left on a Mac, with a field a later version might add to each.
mod legacy_records {
    use super::*;

    #[test]
    fn status_shows_the_sender_queues_awaited_answers_and_a_pending_hand_over() {
        let home = Home::legacy();

        let shown = home.watch(&["--status"], &[]);

        let shown = shown.json();
        assert_eq!(shown["sender"], "sender-chat");
        let prs = shown["prs"]
            .as_array()
            .expect("the PRs")
            .iter()
            .map(|pr| {
                json!([
                    pr["pr"],
                    pr["worker"],
                    pr["status"],
                    pr["pending"],
                    pr["awaitingAnswer"],
                    pr["handingOver"]
                ])
            })
            .collect::<Vec<_>>();
        assert_eq!(
            prs,
            [
                json!([url(7), "next-chat", "watching", 0, true, true]),
                json!([url(8), "w8", "watching", 0, false, false]),
                json!([url(9), "w9", "watching", 1, false, false]),
                json!([url(10), "w10", "halted", 0, false, false]),
            ]
        );
    }

    #[test]
    fn halted_pr_is_released_and_its_message_taken_as_delivered() {
        let home = Home::legacy();

        let released = home.watch(&["--release", &url(10)], &[]);

        assert_eq!(
            released.json(),
            json!({ "released": url(10), "worker": "w10" })
        );
        let state = home.record("e4fa9c7ae05e70a7", "state");
        assert_eq!(
            [&state["status"], &state["halted"], &state["pending"]],
            [&json!("watching"), &Value::Null, &json!([])]
        );
    }

    #[test]
    fn same_chat_keeps_its_link_time_and_another_chat_must_take_over() {
        let home = Home::legacy();

        let again = home.watch(&["--link", &url(9)], &[("CODEX_THREAD_ID", "w9")]);
        let refused = home.watch(&["--link", &url(9)], &[("CODEX_THREAD_ID", "other")]);
        let kept = home.record("6db070b27f9194fb", "link");
        let taken = home.watch(
            &["--link", &url(9), "--take-over"],
            &[("CODEX_THREAD_ID", "other")],
        );
        let moved = home.record("6db070b27f9194fb", "link");

        assert_eq!(again.json(), json!({ "threadId": "w9", "watched": url(9) }));
        assert_eq!(
            [&kept["worker"], &kept["linkedAt"]],
            [&json!("w9"), &json!("2026-10-20T00:00:00.000Z")]
        );
        assert_eq!(refused.code, Some(1));
        assert!(
            refused.stderr.contains("already watched for chat w9"),
            "{}",
            refused.stderr
        );
        assert_eq!(
            taken.json(),
            json!({ "threadId": "other", "watched": url(9) })
        );
        assert_eq!(moved["worker"], "other");
        assert_ne!(moved["linkedAt"], "2026-10-20T00:00:00.000Z");
    }

    fn url(number: u64) -> String {
        format!("https://github.com/org/pay/pull/{number}")
    }
}

impl Home {
    fn new() -> Self {
        let home = Self {
            root: tempfile::tempdir().expect("create a directory"),
        };
        home.write_config(&json!({ "repositories": { "org/pay": { "account": "owner" } } }));
        home
    }

    fn legacy() -> Self {
        let home = Self::new();
        home.write_config(&json!({
            "repositories": { "org/pay": { "account": "owner" } },
            "quietHours": null,
            "instructions": "$task-worker",
        }));
        copy_with_unknown_fields(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/legacy-watch"),
            &home.state(),
        );
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

    fn record(&self, key: &str, file: &str) -> Value {
        let path = self.state().join(key).join(format!("{file}.json"));
        serde_json::from_str(&fs::read_to_string(path).expect("read the record"))
            .expect("parse the record")
    }

    // Pinned to a local run from the chat `caller`, so the ambient environment cannot change the
    // result.
    fn watch(&self, arguments: &[&str], env: &[(&str, &str)]) -> Run {
        let mut command = Command::new(ENGINE);
        command
            .arg("watch")
            .args(arguments)
            .env_clear()
            .env("HOME", self.root.path())
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_STATE_HOME", self.root.path().join("state"))
            .env("HARNEXUS_EXECUTION_ENV", "local")
            .env("CODEX_THREAD_ID", "caller")
            .envs(env.iter().copied());
        let output = command.output().expect("run pr-wake-engine");
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
    Command::new(ENGINE)
        .args(arguments)
        .output()
        .expect("run pr-wake-engine")
}

fn copy_with_unknown_fields(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("create the directory");
    for entry in fs::read_dir(from).expect("list the fixture") {
        let entry = entry.expect("read an entry");
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_with_unknown_fields(&entry.path(), &target);
            continue;
        }
        let mut record: Value =
            serde_json::from_str(&fs::read_to_string(entry.path()).expect("read the fixture"))
                .expect("parse the fixture");
        record["addedLater"] = json!({ "by": "a later version" });
        fs::write(target, record.to_string()).expect("write the record");
    }
}
