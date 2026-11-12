use std::sync::LazyLock;

use jiff::Timestamp;
use regex::Regex;
use serde_json::{Map, Value};

use crate::app::watch::{
    Answer, AnswerStatus, SendOutcome, WorkerView,
    state::Delivered,
    worker::{self, Sent, Workers},
};

// A call to the app as one thread, answered with the bridge's outcome.
pub(crate) type CallApp<'call> =
    Box<dyn FnMut(&str, &str, Map<String, Value>) -> Map<String, Value> + 'call>;

// The chat registered as the sender, read each time so a new registration takes effect.
pub(crate) type Registered<'read> = Box<dyn FnMut() -> Result<Option<String>, String> + 'read>;

pub(crate) struct CodexWorkers<'app> {
    call: CallApp<'app>,
    registered: Registered<'app>,
}

struct ThreadView {
    active: bool,
    // Newest first, as the app lists them.
    turns: Vec<Turn>,
}

struct Page {
    active: bool,
    turns: Vec<Turn>,
    next_cursor: Option<String>,
}

enum Lookup<T> {
    Gone,
    Unknown,
    Found(T),
}

struct Turn {
    status: String,
    started_at: Option<f64>,
    completed_at: Option<f64>,
    origin: Origin,
    // A delegated turn's source thread and message, which tell our message apart from another thread's.
    delegation: Option<Delegation>,
    answer: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    Owner,
    Delegated,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Delegation {
    source: String,
    input: String,
}

// Outcomes the call socket reports for a request that never reached the app.
// `unsupported` is the client's own outcome for a bridge it never wrote to.
const NOT_SENT: [&str; 4] = ["not_sent", "rejected", "invalid", "unsupported"];

// A runaway cursor stops here; the answer wait then gives up as for a lost turn.
const MAX_PAGES: usize = 20;

const OUTPUT_CHARS: u64 = 2000;

// What the app answers for a thread that was deleted or never existed.
const NO_THREAD: &str = "No Codex thread found";

// The owner may still be talking to a worker, so a message waits this long after the owner's last turn.
const OWNER_GRACE_SECONDS: f64 = 5.0 * 60.0;

// The app stamps turns in whole seconds.
const CLOCK_SKEW_SECONDS: f64 = 5.0;

const DELEGATING_TOOLS: [&str; 2] = ["create_thread", "send_message_to_thread"];

static SOURCE_THREAD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"<source_thread_id>\s*([^<\s]+)\s*</source_thread_id>")
        .expect("the source thread pattern is valid")
});

// A thread read can cut the input short, so what is there is kept for the match.
static INPUT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)<input>(.*?)(?:</input>|$)").expect("the input pattern is valid")
});

impl<'app> CodexWorkers<'app> {
    #[must_use]
    pub(crate) fn new(call: CallApp<'app>, registered: Registered<'app>) -> Self {
        Self { call, registered }
    }

    // `Unknown` when the app could not be asked, as while it is closed or the Mac sleeps.
    fn read_thread(
        &mut self,
        caller: &str,
        thread: &str,
        sent_at: Option<&str>,
    ) -> Lookup<ThreadView> {
        let first = match self.read_page(caller, thread, None) {
            Lookup::Found(page) => page,
            Lookup::Gone => return Lookup::Gone,
            Lookup::Unknown => return Lookup::Unknown,
        };
        let mut turns = first.turns;
        let after = sent_at.map(turns_after);
        let mut cursor = first.next_cursor;

        // Later talk could push the answer off the first page, so older pages are read back to the send time.
        for _ in 1..MAX_PAGES {
            let oldest = turns.last().and_then(|turn| turn.started_at);
            let (Some(after), Some(next), Some(oldest)) = (after, cursor.take(), oldest) else {
                break;
            };
            if oldest < after {
                break;
            }
            match self.read_page(caller, thread, Some(&next)) {
                Lookup::Found(older) => {
                    turns.extend(older.turns);
                    cursor = older.next_cursor;
                }
                Lookup::Gone => return Lookup::Gone,
                Lookup::Unknown => return Lookup::Unknown,
            }
        }

        Lookup::Found(ThreadView {
            active: first.active,
            turns,
        })
    }

    fn read_page(&mut self, caller: &str, thread: &str, cursor: Option<&str>) -> Lookup<Page> {
        // A Codex thread shows a delegation only in the call's output, so outputs are read but cut short.
        let mut arguments = Map::from_iter([
            ("threadId".to_owned(), Value::from(thread)),
            ("turnLimit".to_owned(), Value::from(5)),
            ("includeOutputs".to_owned(), Value::from(true)),
            (
                "maxOutputCharsPerItem".to_owned(),
                Value::from(OUTPUT_CHARS),
            ),
        ]);
        if let Some(cursor) = cursor {
            arguments.insert("cursor".to_owned(), Value::from(cursor));
        }
        let answer = (self.call)(caller, "read_thread", arguments);
        page_of(&answer)
    }
}

impl Workers for CodexWorkers<'_> {
    fn sender_of(&mut self, worker: &str) -> Result<String, String> {
        worker::sender_for((self.registered)(), worker)
    }

    fn look(
        &mut self,
        sender: &str,
        worker: &str,
        awaited: Option<&Delivered>,
        now: Timestamp,
    ) -> Option<WorkerView> {
        let view = match self.read_thread(sender, worker, awaited.map(|sent| sent.at.as_str())) {
            Lookup::Found(view) => view,
            Lookup::Gone => return Some(WorkerView::Gone),
            Lookup::Unknown => return None,
        };
        let answer = awaited
            .and_then(|sent| answer_to(&view.turns, sent))
            .map(answer_of);
        Some(WorkerView::Present {
            busy: is_busy(&view, now),
            answer,
        })
    }

    fn send(&mut self, sender: &str, worker: &str, prompt: &str) -> Sent {
        let arguments = Map::from_iter([
            ("threadId".to_owned(), Value::from(worker)),
            ("prompt".to_owned(), Value::from(prompt)),
        ]);
        let detail = (self.call)(sender, "send_message_to_thread", arguments);
        let reported = js_string(detail.get("outcome"));
        let outcome = if reported == "done" {
            SendOutcome::Done
        } else if NOT_SENT.contains(&reported.as_str()) {
            SendOutcome::NotSent
        } else {
            SendOutcome::Unknown(detail)
        };
        Sent { outcome, reported }
    }
}

fn page_of(answer: &Map<String, Value>) -> Lookup<Page> {
    let text = result_text(answer);
    // Protocol 1 names a failed tool by its kind and repeats the app's text as the message.
    let failure = answer
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or(&text);
    let done = answer
        .get("outcome")
        .is_some_and(|outcome| outcome == "done");
    if !done {
        return if failure.contains(NO_THREAD) {
            Lookup::Gone
        } else {
            Lookup::Unknown
        };
    }

    let Some(Value::Object(read)) = serde_json::from_str::<Value>(&text).ok() else {
        return Lookup::Unknown;
    };
    let Some(thread) = read.get("thread").and_then(Value::as_object) else {
        return Lookup::Unknown;
    };
    let status = thread
        .get("status")
        .and_then(Value::as_object)
        .and_then(|status| status.get("type"));
    let turns = read
        .get("turns")
        .and_then(Value::as_array)
        .map(|turns| {
            turns
                .iter()
                .filter_map(Value::as_object)
                .map(turn_of)
                .collect()
        })
        .unwrap_or_default();
    let page = read.get("page").and_then(Value::as_object);
    let next_cursor = page
        .filter(|page| page.get("hasMore") == Some(&Value::Bool(true)))
        .and_then(|page| page.get("nextCursor")?.as_str())
        .map(str::to_owned);
    Lookup::Found(Page {
        active: status.is_some_and(|status| status == "active"),
        turns,
        next_cursor,
    })
}

fn result_text(answer: &Map<String, Value>) -> String {
    let content = answer
        .get("result")
        .and_then(Value::as_object)
        .and_then(|result| result.get("content")?.as_array());
    content
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("text")?.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn turn_of(turn: &Map<String, Value>) -> Turn {
    let items = turn
        .get("items")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let first = items.first();
    let delegation = first.and_then(delegation_of);
    let origin = if delegation.is_some() {
        Origin::Delegated
    } else if first.is_some_and(|first| first.get("type").is_some_and(|kind| kind == "userMessage"))
    {
        Origin::Owner
    } else {
        Origin::Other
    };
    Turn {
        status: js_string(turn.get("status")),
        started_at: turn.get("startedAt").and_then(Value::as_f64),
        completed_at: turn.get("completedAt").and_then(Value::as_f64),
        origin,
        delegation,
        answer: final_text_of(items),
    }
}

// A Claude thread shows another thread's message as a user message carrying the delegation.
// A Codex thread shows it as the output of the call that sent it.
fn delegation_of(first: &Value) -> Option<Delegation> {
    let first = first.as_object()?;
    if let Some(carried) = first.get("codexDelegation").and_then(Value::as_object)
        && let (Some(source), Some(input)) = (
            carried.get("sourceThreadId").and_then(Value::as_str),
            carried.get("input").and_then(Value::as_str),
        )
    {
        return Some(Delegation {
            source: source.to_owned(),
            input: input.to_owned(),
        });
    }

    let kind = first.get("type");
    if kind.is_some_and(|kind| kind == "functionCallOutput")
        && DELEGATING_TOOLS.contains(&js_string(first.get("name")).as_str())
    {
        let output = first.get("output").and_then(Value::as_object);
        return parse_delegation(output.and_then(|output| output.get("text")));
    }
    if kind.is_some_and(|kind| kind == "userMessage")
        && let Some(content) = first.get("content").and_then(Value::as_array)
    {
        let found = content
            .iter()
            .find(|part| part.get("text").is_some_and(Value::is_string));
        return parse_delegation(found.and_then(|part| part.get("text")));
    }
    None
}

fn parse_delegation(text: Option<&Value>) -> Option<Delegation> {
    let text = text?.as_str()?;
    if !text.trim_start().starts_with("<codex_delegation>") {
        return None;
    }
    let source = SOURCE_THREAD
        .captures_iter(text)
        .find_map(|found| found.get(1))
        .map_or("", |source| source.as_str());
    let input = INPUT
        .captures(text)
        .and_then(|found| found.get(1))
        .map_or("", |input| input.as_str());
    Some(Delegation {
        source: source.to_owned(),
        input: unescape_xml(input),
    })
}

fn unescape_xml(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn final_text_of(items: &[Value]) -> String {
    let messages = items
        .iter()
        .filter(|item| item.get("type").is_some_and(|kind| kind == "agentMessage"))
        .collect::<Vec<_>>();
    let finals = messages
        .iter()
        .copied()
        .filter(|item| {
            item.get("phase")
                .is_some_and(|phase| phase == "final_answer")
        })
        .collect::<Vec<_>>();
    let chosen = if finals.is_empty() {
        messages.last().copied().into_iter().collect()
    } else {
        finals
    };
    chosen
        .iter()
        .map(|item| js_string(item.get("text")))
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_busy(view: &ThreadView, now: Timestamp) -> bool {
    let latest = view.turns.first();
    if view.active || latest.is_some_and(|turn| turn.status == "inProgress") {
        return true;
    }
    let now = now.as_duration().as_secs_f64();
    latest.is_some_and(|turn| {
        turn.origin == Origin::Owner
            && turn
                .completed_at
                .is_some_and(|completed| now - completed < OWNER_GRACE_SECONDS)
    })
}

// The turn our message started: sent from our thread with our text, no earlier than we sent it.
fn answer_to<'turns>(turns: &'turns [Turn], sent: &Delivered) -> Option<&'turns Turn> {
    let after = turns_after(&sent.at);
    // Turns are listed newest first, so `rfind` takes the earliest turn that matches.
    turns.iter().rfind(|turn| {
        turn.delegation.as_ref().is_some_and(|delegation| {
            delegation.source == sent.sender && is_prompt_of(&delegation.input, &sent.prompt)
        }) && turn.started_at.is_some_and(|started| started >= after)
    })
}

// A Codex thread's copy may be cut short, so a nonempty start of the prompt counts as the prompt.
fn is_prompt_of(input: &str, prompt: &str) -> bool {
    !input.trim().is_empty() && prompt.starts_with(input.trim_end())
}

// A time that does not parse is `NaN`, which no turn is at or after, as in the TypeScript watch.
fn turns_after(sent_at: &str) -> f64 {
    sent_at.parse::<Timestamp>().map_or(f64::NAN, |sent| {
        sent.as_duration().as_secs_f64() - CLOCK_SKEW_SECONDS
    })
}

fn answer_of(turn: &Turn) -> Answer {
    let status = match turn.status.as_str() {
        "inProgress" => AnswerStatus::InProgress,
        "completed" => AnswerStatus::Completed,
        _ => AnswerStatus::Other,
    };
    Answer {
        status,
        text: turn.answer.clone(),
    }
}

// JavaScript's `String(value ?? "")`, for the fields the app may leave out.
fn js_string(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use rstest::rstest;
    use serde_json::json;

    use super::*;
    use crate::app::time;

    const NOW_SECONDS: f64 = 1_000_000.0;

    #[test]
    fn escaped_delegation_in_a_user_message_is_matched() {
        let prompt = "read <file> & reply";
        let read = json!({
            "thread": { "status": { "type": "idle" } },
            "turns": [{
                "status": "completed",
                "startedAt": 100,
                "items": [
                    {
                        "type": "userMessage",
                        "content": [{
                            "text": "  <codex_delegation><source_thread_id>reviewer</source_thread_id><input>read &lt;file&gt; &amp; reply</input></codex_delegation>",
                        }],
                    },
                    { "type": "agentMessage", "phase": "final_answer", "text": "done" },
                ],
            }],
        });

        let view = look(answering(read), Some(&sent(100_000, prompt)));

        assert_eq!(view, Some(present(false, Some(completed("done")))));
    }

    #[test]
    fn codex_worker_delegation_is_read_from_the_call_output_even_cut_short() {
        let prompt = "$task-worker\nPR <https://github.com/org/pay/pull/7> で対応が必要なこと";
        let calls = Rc::new(RefCell::new(Vec::new()));
        let kept = Rc::clone(&calls);
        let read = json!({
            "thread": { "status": { "type": "idle" } },
            "turns": [{
                "status": "completed",
                "startedAt": 100,
                "completedAt": 160,
                "items": [
                    {
                        "type": "functionCallOutput",
                        "name": "send_message_to_thread",
                        "output": {
                            "text": "<codex_delegation>\n  <source_thread_id>reviewer</source_thread_id>\n  <input>$task-worker\nPR &lt;https://github.com/org/pay/pull/7&gt; で対応",
                            "truncated": true,
                        },
                    },
                    { "type": "agentMessage", "phase": "commentary", "text": "読むのだ" },
                    { "type": "agentMessage", "phase": "final_answer", "text": "指摘なし" },
                ],
            }],
        });
        let call: CallApp<'_> = Box::new(move |caller, tool, arguments| {
            kept.borrow_mut()
                .push((caller.to_owned(), tool.to_owned(), arguments));
            done(&read)
        });

        let view = look(call, Some(&sent(100_000, prompt)));

        let calls = calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            (calls[0].0.as_str(), calls[0].1.as_str()),
            ("reviewer", "read_thread")
        );
        assert_eq!(calls[0].2["threadId"], "worker");
        assert_eq!(calls[0].2["includeOutputs"], true);
        assert_eq!(view, Some(present(false, Some(completed("指摘なし")))));
    }

    #[test]
    fn older_pages_are_read_until_they_reach_the_answer_to_the_sent_message() {
        let sent = sent(100_000, "$task-worker\nPR");
        let (call, cursors) = paged_app(vec![
            vec![owner_turn(400.0), owner_turn(390.0)],
            vec![owner_turn(300.0), delegated_turn(100.0, &sent.prompt)],
            vec![owner_turn(50.0)],
            vec![owner_turn(10.0)],
        ]);

        let view = look(call, Some(&sent));

        assert_eq!(view, Some(present(false, Some(completed("answered")))));
        assert_eq!(
            *cursors.borrow(),
            [None, Some("page:1".to_owned()), Some("page:2".to_owned())]
        );
    }

    #[test]
    fn only_the_latest_page_is_read_while_no_answer_is_awaited() {
        let (call, cursors) = paged_app(vec![vec![owner_turn(400.0)], vec![owner_turn(300.0)]]);

        let view = look(call, None);

        assert_eq!(view, Some(present(false, None)));
        assert_eq!(*cursors.borrow(), [None]);
    }

    #[test]
    fn failed_read_is_gone_only_for_a_thread_the_app_does_not_have() {
        for (name, answer, expected) in [
            (
                "deleted_thread",
                json!({
                    "version": 1,
                    "outcome": "rejected",
                    "message": "No Codex thread found for threadId: w",
                    "result": {
                        "content": [{ "type": "text", "text": "No Codex thread found for threadId: w" }],
                        "isError": true,
                    },
                }),
                Some(WorkerView::Gone),
            ),
            (
                "unreachable_app",
                json!({ "outcome": "not_sent", "message": "ENOENT" }),
                None,
            ),
            (
                "bridge_without_protocol_1",
                json!({ "outcome": "unsupported", "message": "old bridge" }),
                None,
            ),
        ] {
            let answer = object(answer);
            let call: CallApp<'_> = Box::new(move |_, _, _| answer.clone());

            assert_eq!(look(call, None), expected, "case: {name}");
        }
    }

    #[test]
    fn busy_is_told_from_the_latest_turn() {
        let at = |seconds_ago: f64| NOW_SECONDS - seconds_ago;
        for (name, active, latest, busy) in [
            ("running_thread", true, owner_turn(at(600.0)), true),
            (
                "turn_in_progress",
                false,
                json!({ "status": "inProgress", "startedAt": at(600.0), "completedAt": null, "items": owner_items() }),
                true,
            ),
            ("owner_a_minute_ago", false, owner_turn(at(60.0)), true),
            ("owner_ten_minutes_ago", false, owner_turn(at(600.0)), false),
            (
                "another_thread_a_minute_ago",
                false,
                delegated_turn(at(60.0), "hi"),
                false,
            ),
        ] {
            let read = json!({
                "thread": { "status": { "type": if active { "active" } else { "idle" } } },
                "turns": [latest],
            });

            let view = look(answering(read), None);

            assert_eq!(view, Some(present(busy, None)), "case: {name}");
        }
    }

    #[rstest]
    #[case::done("done", SendOutcome::Done)]
    #[case::not_sent("not_sent", SendOutcome::NotSent)]
    #[case::rejected("rejected", SendOutcome::NotSent)]
    #[case::invalid("invalid", SendOutcome::NotSent)]
    #[case::unsupported("unsupported", SendOutcome::NotSent)]
    #[case::unknown(
        "unknown",
        SendOutcome::Unknown(object(json!({ "version": 1, "outcome": "unknown" })))
    )]
    fn protocol_1_outcome_is_read_as_a_send_outcome(
        #[case] outcome: &str,
        #[case] expected: SendOutcome,
    ) {
        let answer = object(json!({ "version": 1, "outcome": outcome }));
        let mut workers = CodexWorkers::new(Box::new(move |_, _, _| answer.clone()), no_sender());

        let sent = workers.send("reviewer", "worker", "hi");

        assert_eq!(
            sent,
            Sent {
                outcome: expected,
                reported: outcome.to_owned(),
            }
        );
    }

    fn look(call: CallApp<'_>, awaited: Option<&Delivered>) -> Option<WorkerView> {
        let now = Timestamp::from_second(1_000_000).expect("a valid time");
        CodexWorkers::new(call, no_sender()).look("reviewer", "worker", awaited, now)
    }

    fn no_sender() -> Registered<'static> {
        Box::new(|| Ok(None))
    }

    fn answering(read: Value) -> CallApp<'static> {
        Box::new(move |_, _, _| done(&read))
    }

    fn done(read: &Value) -> Map<String, Value> {
        object(json!({
            "outcome": "done",
            "result": { "content": [{ "type": "text", "text": read.to_string() }] },
        }))
    }

    type Cursors = Rc<RefCell<Vec<Option<String>>>>;

    fn paged_app(pages: Vec<Vec<Value>>) -> (CallApp<'static>, Cursors) {
        let cursors = Rc::new(RefCell::new(Vec::new()));
        let kept = Rc::clone(&cursors);
        let call: CallApp<'static> = Box::new(move |_, _, arguments| {
            let cursor = arguments
                .get("cursor")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let index = cursor
                .as_deref()
                .and_then(|cursor| cursor.strip_prefix("page:")?.parse::<usize>().ok())
                .unwrap_or(0);
            kept.borrow_mut().push(cursor);
            let more = index + 1 < pages.len();
            done(&json!({
                "thread": { "status": { "type": "idle" } },
                "turns": pages.get(index).cloned().unwrap_or_default(),
                "page": { "hasMore": more, "nextCursor": more.then(|| format!("page:{}", index + 1)) },
            }))
        });
        (call, cursors)
    }

    fn owner_items() -> Value {
        json!([{ "type": "userMessage", "content": [{ "type": "text", "text": "hi" }] }])
    }

    fn owner_turn(started_at: f64) -> Value {
        json!({
            "status": "completed",
            "startedAt": started_at,
            "completedAt": started_at,
            "items": owner_items(),
        })
    }

    fn delegated_turn(started_at: f64, input: &str) -> Value {
        json!({
            "status": "completed",
            "startedAt": started_at,
            "completedAt": started_at,
            "items": [
                { "type": "userMessage", "codexDelegation": { "sourceThreadId": "reviewer", "input": input } },
                { "type": "agentMessage", "phase": "final_answer", "text": "answered" },
            ],
        })
    }

    fn sent(at_ms: i64, prompt: &str) -> Delivered {
        Delivered {
            at: time::format_timestamp(Timestamp::from_millisecond(at_ms).expect("a valid time")),
            sender: "reviewer".to_owned(),
            prompt: prompt.to_owned(),
            findings: Vec::new(),
            follow_up: false,
        }
    }

    const fn present(busy: bool, answer: Option<Answer>) -> WorkerView {
        WorkerView::Present { busy, answer }
    }

    fn completed(text: &str) -> Answer {
        Answer {
            status: AnswerStatus::Completed,
            text: text.to_owned(),
        }
    }

    fn object(value: Value) -> Map<String, Value> {
        let Value::Object(object) = value else {
            panic!("expected an object, got {value}");
        };
        object
    }
}
