use std::{
    ffi::OsStr,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Duration,
};

use jiff::Timestamp;
use serde_json::{Map, Value};

use crate::app::watch::{
    Answer, AnswerStatus, SendOutcome, WorkerView,
    state::Delivered,
    worker::{Sent, T3_SENDER, T3_WORKER, Workers},
};

// `started` tells a command that may have acted from one that never ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum T3Ran {
    Ran {
        code: i32,
        stdout: String,
        stderr: String,
    },
    NotRan {
        started: bool,
        message: String,
    },
}

// Runs t3 with these arguments and this standard input within the timeout.
pub(crate) type RunT3<'run> = Box<dyn FnMut(&[&str], &str, Duration) -> T3Ran + 'run>;

pub(crate) struct T3Workers<'run> {
    run: RunT3<'run>,
}

struct View {
    thread: Thread,
    messages: Vec<Message>,
}

struct Thread {
    status: String,
    active_run_id: Option<String>,
    latest_run_id: Option<String>,
    pending_request_count: f64,
}

struct Message {
    role: String,
    run_id: Option<String>,
    text: String,
}

// Enough of the latest messages to still hold the watch's own after the worker's answer to it.
const MESSAGES: &str = "50";
const SHOW_TIMEOUT: Duration = Duration::from_secs(30);
const SEND_TIMEOUT: Duration = Duration::from_secs(60);

// What the t3 CLI answers for a thread that was deleted or never existed.
const GONE: &str = "is no longer available";

// Failures the t3 CLI reports before it reaches the server.
// Any other failure may come after the message was queued, so its delivery is unknown.
const BEFORE_SENDING: [&str; 4] = [
    "No running T3 Code server",
    "is not a thread id",
    "Failed to reserve loopback port",
    "listen EPERM",
];

const BRIEF_CHARS: usize = 500;

impl<'run> T3Workers<'run> {
    #[must_use]
    pub(crate) fn new(run: RunT3<'run>) -> Self {
        Self { run }
    }
}

impl Workers for T3Workers<'_> {
    fn sender_of(&mut self, _worker: &str) -> Result<String, String> {
        Ok(T3_SENDER.to_owned())
    }

    fn look(
        &mut self,
        _sender: &str,
        worker: &str,
        awaited: Option<&Delivered>,
        _now: Timestamp,
    ) -> Option<WorkerView> {
        let shown = (self.run)(
            &[
                "thread",
                "show",
                thread_of(worker),
                "--messages",
                MESSAGES,
                "--json",
            ],
            "",
            SHOW_TIMEOUT,
        );

        let T3Ran::Ran {
            code,
            stdout,
            stderr,
        } = shown
        else {
            return None;
        };
        if code != 0 {
            return stderr.contains(GONE).then_some(WorkerView::Gone);
        }
        let view = view_of(&printed(&stdout)?)?;
        Some(WorkerView::Present {
            busy: is_busy(&view.thread),
            answer: awaited.and_then(|sent| answer_to(&view, &sent.prompt)),
        })
    }

    fn send(&mut self, _sender: &str, worker: &str, prompt: &str) -> Sent {
        let sent = (self.run)(
            &["thread", "send", thread_of(worker), "-", "--json"],
            prompt,
            SEND_TIMEOUT,
        );

        let (outcome, detail) = match sent {
            T3Ran::NotRan { started, message } => (
                if started {
                    Outcome::Unknown
                } else {
                    Outcome::NotSent
                },
                error_detail(message),
            ),
            T3Ran::Ran {
                code: 0, stdout, ..
            } => (Outcome::Done, printed(&stdout).unwrap_or_default()),
            T3Ran::Ran { code, stderr, .. } => {
                let brief = brief(&stderr);
                let error = if brief.is_empty() {
                    format!("t3 exited with {code}")
                } else {
                    brief
                };
                let before = BEFORE_SENDING.iter().any(|said| stderr.contains(said));
                (
                    if before {
                        Outcome::NotSent
                    } else {
                        Outcome::Unknown
                    },
                    error_detail(error),
                )
            }
        };

        // The CLI's own word for the outcome when it gives one, as the log has kept.
        let reported = detail
            .get("outcome")
            .and_then(Value::as_str)
            .unwrap_or_else(|| outcome.name())
            .to_owned();
        let outcome = match outcome {
            Outcome::Done => SendOutcome::Done,
            Outcome::NotSent => SendOutcome::NotSent,
            Outcome::Unknown => SendOutcome::Unknown(detail),
        };
        Sent { outcome, reported }
    }
}

#[derive(Debug, Clone, Copy)]
enum Outcome {
    Done,
    NotSent,
    Unknown,
}

impl Outcome {
    const fn name(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::NotSent => "not_sent",
            Self::Unknown => "unknown",
        }
    }
}

// Looks for HARNEXUS_T3_BIN, then t3 on PATH, then the launcher the desktop app writes.
// `None` for a HARNEXUS_T3_BIN that is not an absolute path.
pub(crate) fn t3_binary(
    t3_bin: Option<&OsStr>,
    t3_home: Option<&OsStr>,
    home: &Path,
    which: impl FnOnce(&str) -> Option<PathBuf>,
) -> Option<PathBuf> {
    if let Some(set) = t3_bin.filter(|set| !set.is_empty()).map(Path::new) {
        return set.is_absolute().then(|| set.to_owned());
    }
    let t3_home = t3_home
        .map(Path::new)
        .filter(|path| path.is_absolute())
        .map_or_else(|| home.join(".t3"), Path::to_owned);
    Some(which("t3").unwrap_or_else(|| t3_home.join("bin/t3")))
}

pub(crate) fn find_on_path(name: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    std::env::split_paths(path?)
        .map(|directory| directory.join(name))
        .find(|candidate| {
            candidate
                .metadata()
                .is_ok_and(|found| found.is_file() && found.permissions().mode() & 0o111 != 0)
        })
}

fn thread_of(worker: &str) -> &str {
    worker.strip_prefix(T3_WORKER).unwrap_or(worker)
}

fn is_busy(thread: &Thread) -> bool {
    thread.active_run_id.is_some()
        || thread.status == "running"
        // The worker is waiting for an answer from the owner.
        || thread.pending_request_count > 0.0
}

// The t3 CLI shows no times, so the latest message with our text is taken as ours.
fn answer_to(view: &View, prompt: &str) -> Option<Answer> {
    let asked = view.messages.iter().rposition(|message| {
        message.role == "user" && message.text.trim_end() == prompt.trim_end()
    })?;
    let run = view.messages[asked].run_id.as_deref()?;
    let text = view.messages[asked + 1..]
        .iter()
        .rfind(|message| message.role == "assistant" && message.run_id.as_deref() == Some(run))
        .map_or("", |reply| reply.text.as_str());
    Some(Answer {
        status: run_status(&view.thread, run, text),
        text: text.to_owned(),
    })
}

fn run_status(thread: &Thread, run: &str, text: &str) -> AnswerStatus {
    if thread.active_run_id.as_deref() == Some(run) {
        return AnswerStatus::InProgress;
    }
    if thread.latest_run_id.as_deref() == Some(run) {
        return match thread.status.as_str() {
            "completed" => AnswerStatus::Completed,
            "running" => AnswerStatus::InProgress,
            _ => AnswerStatus::Other,
        };
    }
    // A run a later one followed has ended, and only its reply tells whether it was answered.
    if text.is_empty() {
        AnswerStatus::Other
    } else {
        AnswerStatus::Completed
    }
}

fn view_of(shown: &Map<String, Value>) -> Option<View> {
    let thread = shown.get("thread")?.as_object()?;
    let messages = shown.get("messages")?.as_array()?;
    Some(View {
        thread: Thread {
            status: text_of(thread.get("status")),
            active_run_id: text_or_none(thread.get("activeRunId")),
            latest_run_id: text_or_none(thread.get("latestRunId")),
            pending_request_count: thread
                .get("pendingRequestCount")
                .and_then(Value::as_f64)
                .unwrap_or(0.0),
        },
        messages: messages
            .iter()
            .filter_map(Value::as_object)
            .filter_map(|message| {
                Some(Message {
                    role: text_of(message.get("role")),
                    run_id: text_or_none(message.get("runId")),
                    text: message.get("text")?.as_str()?.to_owned(),
                })
            })
            .collect(),
    })
}

// The one JSON object a --json command prints, on its last line.
fn printed(stdout: &str) -> Option<Map<String, Value>> {
    let last = stdout.trim().lines().last().unwrap_or_default();
    match serde_json::from_str::<Value>(last) {
        Ok(Value::Object(printed)) => Some(printed),
        _ => None,
    }
}

fn error_detail(error: String) -> Map<String, Value> {
    Map::from_iter([("error".to_owned(), Value::String(error))])
}

fn brief(text: &str) -> String {
    text.trim().chars().take(BRIEF_CHARS).collect()
}

fn text_of(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
    }
}

fn text_or_none(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, fs, rc::Rc};

    use serde_json::json;

    use super::*;

    const PROMPT: &str = "PR https://github.com/org/pay/pull/7 で対応が必要なことを知らせます。";

    type Runs = Rc<RefCell<Vec<(Vec<String>, String)>>>;

    mod sending {
        use super::*;

        #[test]
        fn prompt_is_queued_on_the_thread_through_stdin() {
            let (mut workers, runs) = workers(ran(0, "{\"delivery\":\"queued\"}\n", ""));

            let sent = workers.send("t3", "t3:w1", "-- looks like an option");

            assert_eq!(
                sent,
                Sent {
                    outcome: SendOutcome::Done,
                    reported: "done".to_owned(),
                }
            );
            assert_eq!(
                *runs.borrow(),
                [(
                    strings(&["thread", "send", "w1", "-", "--json"]),
                    "-- looks like an option".to_owned()
                )]
            );
        }

        #[test]
        fn failure_is_not_sent_only_when_t3_never_reached_the_server() {
            for (name, ran_t3, expected) in [
                ("cannot_start", not_ran(false, "cannot run t3"), "not_sent"),
                (
                    "server_not_running",
                    ran(
                        1,
                        "",
                        "No running T3 Code server uses /x/.t3. Start T3 Code, then retry.",
                    ),
                    "not_sent",
                ),
                (
                    "sandbox_blocks_the_port",
                    ran(1, "", "Failed to reserve loopback port\nlisten EPERM"),
                    "not_sent",
                ),
                (
                    "after_reaching_the_server",
                    ran(
                        1,
                        "",
                        "Could not reach the T3 Code server at http://127.0.0.1:3773.",
                    ),
                    "unknown",
                ),
                (
                    "stopped_while_running",
                    not_ran(true, "t3 was stopped by SIGTERM"),
                    "unknown",
                ),
            ] {
                let (mut workers, _) = workers(ran_t3);

                let sent = workers.send("t3", "t3:w1", "p");

                let outcome = match sent.outcome {
                    SendOutcome::Done => "done",
                    SendOutcome::NotSent => "not_sent",
                    SendOutcome::Unknown(_) => "unknown",
                };
                assert_eq!(outcome, expected, "case: {name}");
                assert_eq!(sent.reported, expected, "case: {name}");
            }
        }

        #[test]
        fn unknown_send_keeps_what_t3_said() {
            let (mut workers, _) = workers(ran(1, "", &format!("  {}  ", "e".repeat(600))));

            let sent = workers.send("t3", "t3:w1", "p");

            let SendOutcome::Unknown(detail) = sent.outcome else {
                panic!("expected an unknown outcome, got {sent:?}");
            };
            assert_eq!(detail["error"], "e".repeat(500));
        }
    }

    mod looking {
        use super::*;

        #[test]
        fn reply_to_the_latest_message_with_our_text_is_the_answer() {
            let (mut workers, runs) = workers(shown(
                &thread(json!({ "latestRunId": "r2" })),
                &[
                    message("user", "r1", PROMPT),
                    message("assistant", "r1", "old answer"),
                    message("user", "r2", &format!("{PROMPT}\n")),
                    message("assistant", "r2", "working on it"),
                    message("assistant", "r2", "fixed"),
                ],
            ));

            let view = workers.look("t3", "t3:w1", Some(&sent()), Timestamp::UNIX_EPOCH);

            assert_eq!(
                view,
                Some(present(
                    false,
                    Some(answer(AnswerStatus::Completed, "fixed"))
                ))
            );
            assert_eq!(
                runs.borrow()[0].0,
                strings(&["thread", "show", "w1", "--messages", "50", "--json"])
            );
        }

        #[test]
        fn shown_thread_tells_how_far_the_run_got() {
            for (name, shown_thread, messages, expected) in [
                (
                    "still_running",
                    thread(
                        json!({ "status": "running", "activeRunId": "r1", "latestRunId": "r1" }),
                    ),
                    vec![message("user", "r1", PROMPT)],
                    present(true, Some(answer(AnswerStatus::InProgress, ""))),
                ),
                (
                    "stopped_by_the_owner",
                    thread(json!({ "status": "interrupted", "latestRunId": "r1" })),
                    vec![message("user", "r1", PROMPT)],
                    present(false, Some(answer(AnswerStatus::Other, ""))),
                ),
                (
                    "waiting_for_the_owner",
                    thread(json!({ "pendingRequestCount": 1, "latestRunId": "r1" })),
                    vec![
                        message("user", "r1", PROMPT),
                        message("assistant", "r1", "done"),
                    ],
                    present(true, Some(answer(AnswerStatus::Completed, "done"))),
                ),
                (
                    "message_not_yet_shown",
                    thread(json!({})),
                    vec![message("user", "r0", "something else")],
                    present(false, None),
                ),
                (
                    "run_followed_by_a_later_one",
                    thread(json!({ "latestRunId": "r2" })),
                    vec![
                        message("user", "r1", PROMPT),
                        message("assistant", "r1", "ok"),
                    ],
                    present(false, Some(answer(AnswerStatus::Completed, "ok"))),
                ),
            ] {
                let (mut workers, _) = workers(shown(&shown_thread, &messages));

                let view = workers.look("t3", "t3:w1", Some(&sent()), Timestamp::UNIX_EPOCH);

                assert_eq!(view, Some(expected), "case: {name}");
            }
        }

        #[test]
        fn failed_show_is_gone_only_for_a_thread_t3_code_no_longer_has() {
            for (name, ran_t3, expected) in [
                (
                    "thread_gone",
                    ran(1, "", "Thread w1 is no longer available."),
                    Some(WorkerView::Gone),
                ),
                (
                    "server_not_running",
                    ran(1, "", "No running T3 Code server uses /x/.t3."),
                    None,
                ),
                ("cannot_start", not_ran(false, "cannot run t3"), None),
                ("not_a_thread", ran(0, "Thread w1\n", ""), None),
            ] {
                let (mut workers, _) = workers(ran_t3);

                let view = workers.look("t3", "t3:w1", None, Timestamp::UNIX_EPOCH);

                assert_eq!(view, expected, "case: {name}");
            }
        }
    }

    #[test]
    fn t3_is_found_in_the_documented_order() {
        for (name, t3_bin, t3_home, on_path, expected) in [
            (
                "t3_bin",
                Some("/opt/t3"),
                None,
                Some("/bin/t3"),
                Some("/opt/t3"),
            ),
            ("relative_t3_bin_refused", Some("t3"), None, None, None),
            ("on_path", None, None, Some("/bin/t3"), Some("/bin/t3")),
            ("desktop_launcher", None, None, None, Some("/u/.t3/bin/t3")),
            (
                "launcher_under_t3code_home",
                None,
                Some("/data/t3"),
                None,
                Some("/data/t3/bin/t3"),
            ),
        ] {
            let found = t3_binary(
                t3_bin.map(OsStr::new),
                t3_home.map(OsStr::new),
                Path::new("/u"),
                |_| on_path.map(PathBuf::from),
            );

            assert_eq!(found, expected.map(PathBuf::from), "case: {name}");
        }
    }

    #[test]
    fn only_an_executable_file_on_path_is_found() {
        let root = tempfile::tempdir().expect("create a directory");
        let (plain, runnable) = (root.path().join("plain"), root.path().join("runnable"));
        for directory in [&plain, &runnable] {
            fs::create_dir(directory).expect("create a directory");
            fs::write(directory.join("t3"), "#!/bin/sh\n").expect("write t3");
        }
        fs::set_permissions(runnable.join("t3"), fs::Permissions::from_mode(0o755))
            .expect("make t3 executable");
        let path = std::env::join_paths([&plain, &runnable]).expect("join the directories");

        let found = find_on_path("t3", Some(&path));

        assert_eq!(found, Some(runnable.join("t3")));
        assert_eq!(find_on_path("t3", None), None);
    }

    fn workers(ran_t3: T3Ran) -> (T3Workers<'static>, Runs) {
        let runs = Rc::new(RefCell::new(Vec::new()));
        let kept = Rc::clone(&runs);
        let workers = T3Workers::new(Box::new(move |arguments, input, _| {
            kept.borrow_mut()
                .push((strings(arguments), input.to_owned()));
            ran_t3.clone()
        }));
        (workers, runs)
    }

    fn ran(code: i32, stdout: &str, stderr: &str) -> T3Ran {
        T3Ran::Ran {
            code,
            stdout: stdout.to_owned(),
            stderr: stderr.to_owned(),
        }
    }

    fn not_ran(started: bool, message: &str) -> T3Ran {
        T3Ran::NotRan {
            started,
            message: message.to_owned(),
        }
    }

    fn thread(fields: Value) -> Value {
        let mut thread = json!({
            "threadId": "w1",
            "status": "completed",
            "activeRunId": null,
            "latestRunId": null,
            "pendingRequestCount": 0,
        });
        if let (Some(thread), Value::Object(fields)) = (thread.as_object_mut(), fields) {
            thread.extend(fields);
        }
        thread
    }

    fn message(role: &str, run_id: &str, text: &str) -> Value {
        json!({ "role": role, "runId": run_id, "text": text })
    }

    fn shown(thread: &Value, messages: &[Value]) -> T3Ran {
        ran(
            0,
            &format!("{}\n", json!({ "thread": thread, "messages": messages })),
            "",
        )
    }

    fn sent() -> Delivered {
        Delivered {
            at: "2026-10-23T03:00:00.000Z".to_owned(),
            sender: "t3".to_owned(),
            prompt: PROMPT.to_owned(),
            findings: Vec::new(),
            follow_up: false,
        }
    }

    const fn present(busy: bool, answer: Option<Answer>) -> WorkerView {
        WorkerView::Present { busy, answer }
    }

    fn answer(status: AnswerStatus, text: &str) -> Answer {
        Answer {
            status,
            text: text.to_owned(),
        }
    }

    fn strings(arguments: &[&str]) -> Vec<String> {
        arguments
            .iter()
            .map(|argument| (*argument).to_owned())
            .collect()
    }
}
