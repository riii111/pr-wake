use std::{collections::HashSet, path::Path, process, time::Duration};

use jiff::{Timestamp, ZonedDateTime};

use super::{
    Failure,
    log::{Entry, Log},
};
use crate::{
    app::watch::{
        self, AnswerNote, Delivery, Observed, SendResult,
        config::WatchConfig,
        pr::{GitHub, PrLink, PrRef},
        state::{PrSummary, WatchEvent, WatchState},
        worker::Workers,
    },
    infra::{
        record::ProcessLock,
        stop::StopRequest,
        watch_record::{self, WatchPaths},
    },
};

pub(crate) struct CycleDeps<'deps> {
    pub(crate) paths: &'deps WatchPaths,
    pub(crate) github: &'deps mut dyn GitHub,
    pub(crate) workers: &'deps mut dyn Workers,
    pub(crate) now: &'deps dyn Fn() -> ZonedDateTime,
    pub(crate) log: &'deps dyn Log,
    // Shows the owner a notification with a subtitle and a message, without waiting.
    pub(crate) notify: &'deps dyn Fn(&str, &str) -> Result<(), String>,
}

pub(crate) struct Watch<'deps> {
    pub(crate) cycle: CycleDeps<'deps>,
    pub(crate) stop: &'deps StopRequest,
    // Waits between cycles, returning early on a stop request.
    pub(crate) pause: &'deps mut dyn FnMut(Duration),
    pub(crate) replaced: Option<&'deps dyn Fn() -> bool>,
    // Why the watch must end before its next cycle, such as a bridge it may no longer write to.
    pub(crate) ended: &'deps dyn Fn() -> Option<String>,
}

struct Tracked {
    link: PrLink,
    state: WatchState,
    account: String,
    // What is on disk, so a state is written only when the cycle changed it.
    before: WatchState,
}

// A failed cycle is retried after this, so a config or network error never ends the watch.
const RETRY: Duration = Duration::from_secs(60);

// Runs until a stop request, an end, or a replaced program.
pub(crate) fn watch_forever(watch: Watch<'_>) -> Result<(), Failure> {
    let Watch {
        mut cycle,
        stop,
        pause,
        replaced,
        ended,
    } = watch;
    let _lock = acquire_lock(&cycle.paths.state)?;
    cycle
        .log
        .write(Entry::new("started").with("pid", process::id()));

    // The PRs this process found over, which later cycles leave unread.
    let mut over = HashSet::new();
    loop {
        if stop.is_requested() {
            cycle.log.write(Entry::new("stop_requested"));
            return Ok(());
        }
        if let Some(reason) = ended() {
            cycle
                .log
                .write(Entry::new("ended_watch").with("reason", reason.as_str()));
            return Err(Failure::Refused(reason));
        }
        // A replaced program must neither run a cycle nor end mid-send, so it is checked before each cycle.
        if replaced.is_some_and(|replaced| replaced()) {
            cycle.log.write(Entry::new("replaced"));
            return Err(Failure::Replaced);
        }

        let wait = match run_cycle(&mut cycle, &mut over) {
            Ok(config) => Duration::from_secs(config.poll_interval_seconds),
            Err(failure) => {
                // A failure's message may carry a path or what a command printed, so the log records the tag alone.
                cycle
                    .log
                    .write(Entry::new("cycle_failed").with("error", failure.tag()));
                RETRY
            }
        };
        pause(wait);
    }
}

pub(crate) fn run_cycle(
    deps: &mut CycleDeps<'_>,
    over: &mut HashSet<String>,
) -> Result<WatchConfig, Failure> {
    let config = watch_record::load_config(&deps.paths.config).map_err(Failure::Config)?;
    let links = watch_record::list_links(&deps.paths.state, over).map_err(Failure::State)?;
    let now = (deps.now)();

    let mut tracked = Vec::new();
    for link in links {
        if let Some(entry) = open_entry(deps, &config, link, over)? {
            tracked.push(entry);
        }
    }

    for (account, due) in due_by_account(&tracked, now.timestamp(), &config) {
        poll_account(deps, &config, &account, &mut tracked, &due, now.timestamp());
    }

    for entry in &mut tracked {
        deliver(deps, &config, entry, &now);
        if entry.before != entry.state {
            save(deps.paths, &entry.link.pr, &entry.state)?;
        }
        if entry.state.status.is_over() {
            over.insert(watch_record::pr_key(&entry.link.pr));
        }
    }
    Ok(config)
}

fn acquire_lock(root: &Path) -> Result<ProcessLock, Failure> {
    match watch_record::hold_watch_lock(root) {
        Ok(Some(lock)) => Ok(lock),
        Ok(None) => Err(Failure::Refused("another PR watch is running".to_owned())),
        Err(error) => Err(Failure::State(error)),
    }
}

fn open_entry(
    deps: &CycleDeps<'_>,
    config: &WatchConfig,
    link: PrLink,
    over: &mut HashSet<String>,
) -> Result<Option<Tracked>, Failure> {
    // Left as it is on disk, so the tracking in it is never reset by a save.
    let Ok(mut state) = watch_record::load_watch_state(&deps.paths.state, &link.pr) else {
        deps.log.write(
            Entry::new("state_unreadable")
                .with("pr", link.url.as_str())
                .with("error", "StateUnreadable"),
        );
        return Ok(None);
    };
    if state.status.is_over() {
        over.insert(watch_record::pr_key(&link.pr));
    }

    let before = state.clone();
    let opened = watch::open(&mut state, &link.worker, &link.pr.repo, config);
    if let Some(handed) = opened.handed_over {
        deps.log.write(
            Entry::new("handed_over")
                .with("pr", link.url.as_str())
                .with("from", handed.from)
                .with("to", handed.to),
        );
    }
    let Some(account) = opened.account.map(str::to_owned) else {
        if state != before {
            save(deps.paths, &link.pr, &state)?;
        }
        return Ok(None);
    };
    Ok(Some(Tracked {
        link,
        state,
        account,
        before,
    }))
}

// Each account's PRs are read in one query, in the order the accounts first appear.
fn due_by_account(
    tracked: &[Tracked],
    now: Timestamp,
    config: &WatchConfig,
) -> Vec<(String, Vec<usize>)> {
    let mut due: Vec<(String, Vec<usize>)> = Vec::new();
    for (index, entry) in tracked.iter().enumerate() {
        if !watch::is_due(&entry.state, now, config) {
            continue;
        }
        match due
            .iter_mut()
            .find(|(account, _)| *account == entry.account)
        {
            Some((_, indexes)) => indexes.push(index),
            None => due.push((entry.account.clone(), vec![index])),
        }
    }
    due
}

fn poll_account(
    deps: &mut CycleDeps<'_>,
    config: &WatchConfig,
    account: &str,
    tracked: &mut [Tracked],
    due: &[usize],
    now: Timestamp,
) {
    let prs = due
        .iter()
        .map(|&index| tracked[index].link.pr.clone())
        .collect::<Vec<_>>();
    let summaries = match deps.github.summaries(account, &prs, &config.review_bots) {
        Ok(summaries) => summaries,
        Err(error) => {
            deps.log.write(
                Entry::new("poll_failed")
                    .with("account", account)
                    .with("error", error.kind.tag()),
            );
            return;
        }
    };

    for (position, &index) in due.iter().enumerate() {
        let summary = summaries.get(position).cloned().flatten();
        apply_summary(deps, config, &mut tracked[index], summary, now);
    }
}

fn apply_summary(
    deps: &mut CycleDeps<'_>,
    config: &WatchConfig,
    entry: &mut Tracked,
    summary: Option<PrSummary>,
    now: Timestamp,
) {
    let pr = entry.link.url.as_str();
    let observed = watch::observe(&mut entry.state, summary.as_ref(), now);
    match observed {
        Observed::Unreadable => {
            deps.log.write(
                Entry::new("pr_unreadable")
                    .with("pr", pr)
                    .with("account", entry.account.as_str()),
            );
        }
        Observed::Ended => {
            deps.log.write(
                Entry::new("ended")
                    .with("pr", pr)
                    .with("reason", entry.state.reason.clone()),
            );
        }
        Observed::Kept { paused: true } | Observed::ReadDetail { paused: true, .. } => {
            deps.log.write(Entry::new("paused").with("pr", pr));
        }
        Observed::Kept { paused: false } | Observed::ReadDetail { paused: false, .. } => {}
    }
    let (Observed::ReadDetail { resumed, .. }, Some(summary)) = (observed, summary) else {
        return;
    };
    if resumed {
        deps.log.write(Entry::new("resumed").with("pr", pr));
    }

    let detail = match deps
        .github
        .detail(&entry.account, &entry.link.pr, &config.review_bots)
    {
        Ok(detail) => detail,
        // The summary is left as it was, so the next cycle reads the detail again.
        Err(error) => {
            deps.log.write(
                Entry::new("detail_failed")
                    .with("pr", pr)
                    .with("error", error.kind.tag()),
            );
            return;
        }
    };

    let queued = watch::take_in(
        &mut entry.state,
        &summary,
        &detail,
        entry.link.linked_at,
        &entry.account,
        config,
    );
    if !queued.is_empty() {
        let kinds = queued.iter().map(WatchEvent::kind).collect::<Vec<_>>();
        deps.log
            .write(Entry::new("queued").with("pr", pr).with("kinds", kinds));
    }
}

fn deliver(
    deps: &mut CycleDeps<'_>,
    config: &WatchConfig,
    entry: &mut Tracked,
    now: &ZonedDateTime,
) {
    if !watch::awaits_delivery(&entry.state) {
        return;
    }
    let worker = entry.link.worker.as_str();
    let sender = match deps.workers.sender_of(worker) {
        Ok(sender) => sender,
        Err(missing) => {
            if watch::hold_for_sender(&mut entry.state, &missing) {
                deps.log.write(
                    Entry::new("no_sender")
                        .with("pr", entry.link.url.as_str())
                        .with("worker", worker)
                        .with("reason", missing),
                );
            }
            return;
        }
    };
    let Some(view) = deps.workers.look(
        &sender,
        worker,
        entry.state.delivered.as_ref(),
        now.timestamp(),
    ) else {
        return;
    };

    match watch::deliver(&mut entry.state, view, now, config) {
        Delivery::Stopped => deps.log.write(
            Entry::new("stopped")
                .with("pr", entry.link.url.as_str())
                .with("reason", entry.state.reason.clone()),
        ),
        Delivery::Hold(note) => tell_answer(deps, entry, note),
        Delivery::Send(note) => {
            tell_answer(deps, entry, note);
            send(deps, config, entry, &sender, now.timestamp());
        }
    }
}

const LOST_NOTICE: &str = "ワーカーが通知を受け取れていません";
const RESEND_NOTICE: &str = "ワーカーが通知を受け取れていません。送り直します";

fn tell_answer(deps: &CycleDeps<'_>, entry: &Tracked, note: Option<AnswerNote>) {
    let (log, pr) = (deps.log, entry.link.url.as_str());
    match note {
        None => {}
        Some(AnswerNote::NotSeen { resent, first }) => {
            log.write(
                Entry::new("answer_not_seen")
                    .with("pr", pr)
                    .with("resent", resent),
            );
            // Later losses in a row only repeat what the owner was told, so only the first is shown.
            if first {
                let subtitle = format!("{} #{}", entry.link.pr.repo, entry.link.pr.number);
                let message = if resent == 0 {
                    LOST_NOTICE
                } else {
                    RESEND_NOTICE
                };
                if let Err(error) = (deps.notify)(&subtitle, message) {
                    log.write(
                        Entry::new("notify_failed")
                            .with("pr", pr)
                            .with("error", error),
                    );
                }
            }
        }
        Some(AnswerNote::Incomplete { missing }) => log.write(
            Entry::new("answer_incomplete")
                .with("pr", pr)
                .with("missing", missing),
        ),
    }
}

fn send(
    deps: &mut CycleDeps<'_>,
    config: &WatchConfig,
    entry: &mut Tracked,
    sender: &str,
    now: Timestamp,
) {
    let pr = entry.link.url.as_str();
    let worker = entry.link.worker.as_str();
    let sending = watch::prepare_send(&entry.state, pr, now, config);
    // A watch stopped mid-send must never send the message again, so the halted mark is saved before the call.
    if save(deps.paths, &entry.link.pr, &sending.marked).is_err() {
        deps.log.write(
            Entry::new("state_unwritable")
                .with("pr", pr)
                .with("error", "StateUnwritable"),
        );
        return;
    }
    // Compared against the mark, so whatever the outcome the state is written again.
    entry.before = sending.marked.clone();

    let sent_at = (deps.now)().timestamp();
    let sent = deps.workers.send(sender, worker, &sending.prompt);
    let result = watch::finish_send(
        &mut entry.state,
        sending,
        sender,
        sent.outcome,
        sent_at,
        now,
    );

    let logged = match result {
        SendResult::Delivered {
            follow_up,
            findings,
        } => Entry::new("sent")
            .with("pr", pr)
            .with("worker", worker)
            .with("followUp", follow_up)
            .with("findings", findings),
        SendResult::NotSent => Entry::new("send_not_sent")
            .with("pr", pr)
            .with("outcome", sent.reported),
        SendResult::Halted => Entry::new("halted")
            .with("pr", pr)
            .with("outcome", sent.reported),
    };
    deps.log.write(logged);
}

fn save(paths: &WatchPaths, pr: &PrRef, state: &WatchState) -> Result<(), Failure> {
    watch_record::save_watch_state(&paths.state, pr, state).map_err(Failure::State)
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::BTreeMap, fs};

    use jiff::{SignedDuration, tz::TimeZone};
    use serde_json::{Map, Value, json};

    use super::*;
    use crate::{
        app::{
            time,
            watch::{
                pr::{ItemKind, PrComment, PrDetail, ReadError, ReadErrorKind},
                reviewers::ReviewBot,
                state::{BotStatus, WatchStatus, bot_key},
            },
        },
        infra::{
            harnexus::{CallApp, CodexWorkers},
            t3::{T3Ran, T3Workers},
        },
        runtime::watch::AppWorkers,
    };

    const PR: &str = "https://github.com/org/pay/pull/7";
    const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const LINKED_AT: &str = "2026-10-20T00:00:00Z";
    const NOON: &str = "2026-10-23T12:00:00Z";

    // What GitHub, the apps and the clock show the watch.
    struct World {
        summary: PrSummary,
        comments: Vec<PrComment>,
        detail_fails: bool,
        send_outcome: &'static str,
        // The chat registered with watch --sender.
        sender: Option<String>,
        now: Timestamp,
        // A worker in T3 Code as t3 thread show prints it.
        t3_thread: Value,
        t3_messages: Vec<Value>,
    }

    #[derive(Default)]
    struct Calls {
        summaries: usize,
        details: usize,
        sent: Vec<SentCall>,
        t3: Vec<(Vec<String>, String)>,
    }

    #[derive(Debug, Clone)]
    struct SentCall {
        caller: String,
        thread: String,
        prompt: String,
        // The status on disk when the call was made.
        saved: WatchStatus,
    }

    struct Fixture {
        _root: tempfile::TempDir,
        paths: WatchPaths,
        world: RefCell<World>,
        calls: RefCell<Calls>,
        logs: Recorded,
        notices: RefCell<Vec<(String, String)>>,
    }

    #[derive(Default)]
    struct Recorded(RefCell<Vec<Entry>>);

    struct FakeGitHub<'fixture>(&'fixture Fixture);

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Read {
        First,
        Again,
    }

    mod t3_worker {
        use super::*;

        #[test]
        fn idle_worker_is_sent_through_t3_code_without_a_sender_and_its_reply_taken_as_the_answer()
        {
            let fixture = Fixture::new(json!({}), Read::Again, "t3:mcp:w1");
            fixture.update(|world| {
                world.sender = None;
                world.comments = vec![comment()];
                world.t3_thread = t3_thread(json!({ "status": "running", "activeRunId": "r0" }));
            });

            fixture.cycle();
            let waited = fixture.calls.borrow().t3.len();

            fixture.update(|world| {
                world.t3_thread = t3_thread(json!({ "latestRunId": "r0" }));
            });
            fixture.later(1);
            fixture.cycle();
            let (arguments, input) = fixture.calls.borrow().t3[0].clone();

            fixture.update(|world| {
                world.t3_messages = vec![
                    json!({ "role": "user", "runId": "r1", "text": input }),
                    json!({ "role": "assistant", "runId": "r1", "text": format!("{PR}#discussion_r1 を直してpushした") }),
                ];
                world.t3_thread = t3_thread(json!({ "latestRunId": "r1" }));
            });
            fixture.later(1);
            fixture.cycle();

            assert_eq!(waited, 0);
            assert!(fixture.calls.borrow().sent.is_empty());
            assert_eq!(fixture.calls.borrow().t3.len(), 1);
            assert_eq!(arguments, ["thread", "send", "mcp:w1", "-", "--json"]);
            assert!(input.contains(PR), "{input}");
            let after = fixture.state();
            assert_eq!(after.status, WatchStatus::Watching);
            assert_eq!(after.pending, []);
            assert_eq!(after.delivered, None);
        }
    }

    mod sending {
        use super::*;

        #[test]
        fn new_findings_go_in_one_message_from_the_registered_chat_after_the_halted_mark_is_saved()
        {
            let fixture = Fixture::new(
                json!({ "instructions": "$task-worker" }),
                Read::Again,
                "worker",
            );
            fixture.update(|world| {
                world.comments = vec![
                    comment(),
                    PrComment {
                        id: "inline:3".to_owned(),
                        created_at: "2026-10-19T00:00:00Z".to_owned(),
                        url: format!("{PR}#discussion_r3"),
                        ..comment()
                    },
                ];
            });

            fixture.cycle();

            let sent = fixture.sent();
            assert_eq!(sent.len(), 1);
            assert_eq!(
                (
                    sent[0].caller.as_str(),
                    sent[0].thread.as_str(),
                    sent[0].saved
                ),
                ("reviewer", "worker", WatchStatus::Halted)
            );
            assert!(
                sent[0].prompt.starts_with("$task-worker\n"),
                "{}",
                sent[0].prompt
            );
            assert!(sent[0].prompt.contains(&format!("{PR}#discussion_r1")));
            assert!(!sent[0].prompt.contains("discussion_r3"));
            let saved = fixture.state();
            assert_eq!(saved.status, WatchStatus::Watching);
            assert_eq!(saved.pending, []);
            assert_eq!(
                saved.delivered.map(|delivered| delivered.findings.len()),
                Some(1)
            );
        }

        #[test]
        fn unconfirmed_send_halts_and_nothing_is_sent_for_the_pr_again() {
            let fixture = Fixture::new(json!({}), Read::Again, "worker");
            fixture.update(|world| {
                world.comments = vec![comment()];
                world.send_outcome = "unknown";
            });
            fixture.cycle();

            fixture.update(|world| world.send_outcome = "done");
            fixture.later(5);
            fixture.cycle();

            let saved = fixture.state();
            assert_eq!(fixture.sent().len(), 1);
            assert_eq!(saved.status, WatchStatus::Halted);
            assert!(
                saved
                    .halted
                    .as_ref()
                    .is_some_and(|halted| halted["prompt"].is_string()),
                "{saved:?}"
            );
            assert!(
                fixture
                    .logged("halted")
                    .iter()
                    .any(|entry| entry.fields.contains(&("outcome", json!("unknown"))))
            );
        }

        #[test]
        fn message_stays_queued_while_the_app_is_not_reachable() {
            let fixture = Fixture::new(json!({}), Read::Again, "worker");
            fixture.update(|world| {
                world.comments = vec![comment()];
                world.send_outcome = "not_sent";
            });
            fixture.cycle();

            fixture.update(|world| world.send_outcome = "done");
            fixture.later(1);
            fixture.cycle();

            assert_eq!(fixture.sent().len(), 2);
            assert_eq!(fixture.state().pending, []);
            assert_eq!(fixture.logged("send_not_sent").len(), 1);
        }
    }

    mod polling {
        use super::*;

        #[test]
        fn failed_first_read_of_the_detail_is_logged_by_its_kind_and_tried_again_next_cycle() {
            let fixture = Fixture::new(json!({}), Read::First, "worker");
            fixture.update(|world| {
                world.comments = vec![comment()];
                world.detail_fails = true;
            });
            fixture.cycle();

            assert_eq!(fixture.state().summary, None);
            let failed = fixture.logged("detail_failed");
            assert_eq!(failed.len(), 1);
            assert!(
                failed[0]
                    .fields
                    .contains(&("error", json!("GithubUnreachable")))
            );
            let logs = format!("{:?}", fixture.logs.0.borrow());
            assert!(!logs.contains("GitHub is down"), "{logs}");

            fixture.update(|world| world.detail_fails = false);
            fixture.later(1);
            fixture.cycle();

            assert_eq!(fixture.calls.borrow().details, 2);
            assert!(fixture.sent().is_empty());
            assert!(fixture.state().seen.contains(&"inline:1".to_owned()));
        }

        #[test]
        fn draft_is_paused_without_reading_its_detail_and_resumed_when_ready() {
            let fixture = Fixture::new(json!({}), Read::Again, "worker");
            fixture.update(|world| {
                world.summary = PrSummary {
                    is_draft: true,
                    ..summary()
                };
                world.comments = vec![comment()];
            });
            fixture.cycle();

            assert_eq!(fixture.calls.borrow().details, 0);
            assert_eq!(fixture.state().status, WatchStatus::Draft);

            fixture.update(|world| world.summary = summary());
            fixture.later(1);
            fixture.cycle();

            assert_eq!(fixture.calls.borrow().details, 1);
            assert_eq!(fixture.sent().len(), 1);
            assert_eq!(
                fixture
                    .events()
                    .into_iter()
                    .filter(|event| ["paused", "resumed"].contains(event))
                    .collect::<Vec<_>>(),
                ["paused", "resumed"]
            );
        }

        #[test]
        fn pr_of_a_repository_the_config_does_not_list_is_left_unpolled() {
            let fixture = Fixture::new(json!({ "repositories": {} }), Read::Again, "worker");

            fixture.cycle();

            assert_eq!(fixture.calls.borrow().summaries, 0);
            assert!(fixture.sent().is_empty());
            assert_eq!(
                fixture.state().reason.as_deref(),
                Some("repository is not configured")
            );
        }

        #[test]
        fn pr_found_over_is_left_unread_for_as_long_as_the_process_runs() {
            let fixture = Fixture::new(json!({}), Read::Again, "worker");
            let mut over = HashSet::new();
            fixture.update(|world| {
                world.summary = PrSummary {
                    state: "MERGED".to_owned(),
                    ..summary()
                };
            });
            fixture.cycle_over(&mut over);
            fs::write(fixture.state_path(), "{").expect("damage the state");

            fixture.cycle_over(&mut over);
            let skipped = fixture.logged("state_unreadable").len();
            fixture.cycle();
            let read = fixture.logged("state_unreadable").len();

            assert_eq!((over.len(), skipped, read), (1, 0, 1));
        }

        #[test]
        fn record_that_cannot_be_read_is_left_as_it_is_and_nothing_is_polled_or_sent() {
            let fixture = Fixture::new(json!({}), Read::Again, "worker");
            let tracked = WatchState {
                summary: Some(summary()),
                seen: vec![bot_key("coderabbit"), "inline:1".to_owned()],
                pending: vec![WatchEvent::Conflict {
                    head: HEAD.to_owned(),
                }],
                ..WatchState::default()
            };
            save(&fixture.paths, &pay(), &tracked).expect("save the state");
            let damaged = fs::read_to_string(fixture.state_path())
                .expect("read the state")
                .replace(r#""type": "conflict""#, r#""type": "merged""#);
            fs::write(fixture.state_path(), &damaged).expect("damage the state");

            fixture.cycle();

            assert_eq!(
                fs::read_to_string(fixture.state_path()).expect("read the state"),
                damaged
            );
            assert_eq!(fixture.calls.borrow().summaries, 0);
            assert!(fixture.sent().is_empty());
            assert_eq!(fixture.logged("state_unreadable").len(), 1);
        }
    }

    mod lost_message {
        use super::*;

        #[test]
        fn message_an_idle_worker_never_started_is_told_once_and_sent_again_after_quiet_hours() {
            let fixture = Fixture::new(
                json!({ "quietHours": { "start": "12:05", "end": "13:00" } }),
                Read::Again,
                "worker",
            );
            fixture.update(|world| world.comments = vec![comment()]);
            fixture.cycle();
            fixture.later(10);
            fixture.cycle();
            let held = fixture.sent().len();

            fixture.later(50);
            fixture.cycle();
            fixture.later(30);
            fixture.cycle();

            assert_eq!(held, 1);
            assert_eq!(fixture.sent().len(), 3);
            assert!(fixture.prompt(1).contains("discussion_r1"));
            assert_eq!(
                *fixture.notices.borrow(),
                [("org/pay #7".to_owned(), RESEND_NOTICE.to_owned())]
            );
            let lost = fixture.logged("answer_not_seen");
            assert_eq!(lost.len(), 2);
            assert!(lost[0].fields.contains(&("resent", json!(2))));
            assert_eq!(fixture.state().lost_since, Some(at("2026-10-23T12:10:00Z")));
        }
    }

    mod hand_over {
        use super::*;

        #[test]
        fn halted_pr_goes_to_the_chat_that_takes_it_with_what_it_still_had_to_send() {
            let fixture = Fixture::new(json!({}), Read::Again, "worker");
            fixture.update(|world| {
                world.comments = vec![comment()];
                world.send_outcome = "unknown";
            });
            fixture.cycle();
            let halted = fixture.state().status;

            fixture.link_to("next");
            fixture.update(|world| world.send_outcome = "done");
            fixture.later(1);
            fixture.cycle();

            assert_eq!(halted, WatchStatus::Halted);
            let saved = fixture.state();
            assert_eq!(
                (saved.status, saved.halted, saved.worker.as_deref()),
                (WatchStatus::Watching, None, Some("next"))
            );
            assert_eq!(
                fixture.sent().last().map(|sent| sent.thread.clone()),
                Some("next".to_owned())
            );
            let handed = fixture.logged("handed_over");
            assert_eq!(handed.len(), 1);
            assert!(
                [("from", json!("worker")), ("to", json!("next"))]
                    .iter()
                    .all(|field| handed[0].fields.contains(field)),
                "{handed:?}"
            );
        }
    }

    mod forever {
        use std::cell::Cell;

        use super::*;

        #[test]
        fn replaced_program_ends_the_watch_before_the_next_cycle() {
            let fixture = Fixture::new(json!({}), Read::Again, "worker");
            fs::remove_file(fixture.link_path()).expect("unlink the PR");
            let (checks, slept) = (Cell::new(0), Cell::new(0));
            let replaced = || {
                checks.set(checks.get() + 1);
                checks.get() > 1
            };

            let ended =
                fixture.watch_forever(&StopRequest::default(), Some(&replaced), &mut |_| {
                    slept.set(slept.get() + 1);
                });

            assert!(matches!(ended, Err(Failure::Replaced)), "{ended:?}");
            assert!(ended.is_err_and(|failure| failure.to_string().contains("was replaced")));
            // One cycle ran before the sleep, and none after the check that found it replaced.
            assert_eq!((checks.get(), slept.get()), (2, 1));
            assert_eq!(fixture.events(), ["started", "replaced"]);
        }

        #[test]
        fn watch_refuses_to_start_while_another_holds_the_lock() {
            let fixture = Fixture::new(json!({}), Read::Again, "worker");
            let _held = watch_record::hold_watch_lock(&fixture.paths.state)
                .expect("lock")
                .expect("the lock is free");

            let ended = fixture.watch_forever(&StopRequest::default(), None, &mut |_| {});

            assert!(
                matches!(&ended, Err(Failure::Refused(reason)) if reason == "another PR watch is running"),
                "{ended:?}"
            );
            assert!(fixture.sent().is_empty());
            let calls = fixture.calls.borrow();
            assert_eq!((calls.summaries, calls.details), (0, 0));
        }

        #[test]
        fn stop_request_ends_the_watch_cleanly_after_the_cycle() {
            let fixture = Fixture::new(json!({}), Read::Again, "worker");
            fixture.update(|world| world.comments = vec![comment()]);
            let stop = StopRequest::default();

            let ended = fixture.watch_forever(&stop, None, &mut |_| stop.request());

            assert!(ended.is_ok(), "{ended:?}");
            assert_eq!(fixture.sent().len(), 1);
            assert_eq!(fixture.events().first(), Some(&"started"));
            assert_eq!(fixture.events().last(), Some(&"stop_requested"));
        }

        #[test]
        fn failed_cycle_is_logged_and_tried_again_after_a_minute() {
            let fixture = Fixture::new(json!({}), Read::Again, "worker");
            fs::remove_file(&fixture.paths.config).expect("remove the config");
            let stop = StopRequest::default();
            let waits = RefCell::new(Vec::new());

            let ended = fixture.watch_forever(&stop, None, &mut |wait| {
                waits.borrow_mut().push(wait);
                stop.request();
            });

            assert!(ended.is_ok(), "{ended:?}");
            assert_eq!(*waits.borrow(), [Duration::from_mins(1)]);
            let failed = fixture.logged("cycle_failed");
            assert_eq!(failed.len(), 1);
            assert!(
                failed[0]
                    .fields
                    .contains(&("error", json!("ConfigUnreadable")))
            );
        }
    }

    impl Fixture {
        // Starts from a PR the watch already read once, unless the test checks the first read.
        fn new(config: Value, read: Read, worker: &str) -> Self {
            let root = tempfile::tempdir().expect("create a directory");
            let paths = WatchPaths {
                config: root.path().join("config/harnexus/pr-watch.json"),
                state: root.path().join("state/harnexus-crew/watch"),
            };
            let mut written = json!({
                "repositories": { "org/pay": { "account": "owner" } },
                "quietHours": null,
            });
            if let (Some(written), Value::Object(config)) = (written.as_object_mut(), config) {
                written.extend(config);
            }
            fs::create_dir_all(paths.config.parent().expect("a parent"))
                .expect("create the config directory");
            fs::write(&paths.config, written.to_string()).expect("write the config");

            let fixture = Self {
                _root: root,
                paths,
                world: RefCell::new(World {
                    summary: summary(),
                    comments: Vec::new(),
                    detail_fails: false,
                    send_outcome: "done",
                    sender: Some("reviewer".to_owned()),
                    now: at(NOON),
                    t3_thread: t3_thread(json!({})),
                    t3_messages: Vec::new(),
                }),
                calls: RefCell::new(Calls::default()),
                logs: Recorded::default(),
                notices: RefCell::default(),
            };

            fixture.link_to(worker);
            if read == Read::Again {
                let read = WatchState {
                    summary: Some(PrSummary {
                        updated_at: "2026-10-22T00:00:00Z".to_owned(),
                        ..summary()
                    }),
                    seen: vec![bot_key("coderabbit")],
                    ..WatchState::default()
                };
                save(&fixture.paths, &pay(), &read).expect("save the state");
            }
            fixture
        }

        fn update(&self, change: impl FnOnce(&mut World)) {
            change(&mut self.world.borrow_mut());
        }

        fn later(&self, minutes: i64) {
            self.update(|world| {
                world.now += SignedDuration::from_mins(minutes);
                world.summary.updated_at = time::format_timestamp(world.now);
            });
        }

        fn link_to(&self, worker: &str) {
            let link = PrLink {
                pr: pay(),
                url: PR.to_owned(),
                worker: worker.to_owned(),
                linked_at: at(LINKED_AT),
            };
            watch_record::record_link(&self.paths.state, &link).expect("record the link");
        }

        fn cycle(&self) {
            self.cycle_over(&mut HashSet::new());
        }

        fn cycle_over(&self, over: &mut HashSet<String>) {
            let mut github = FakeGitHub(self);
            let mut workers = self.workers();
            let now = || self.world.borrow().now.to_zoned_datetime(TimeZone::UTC);
            let mut deps = CycleDeps {
                paths: &self.paths,
                github: &mut github,
                workers: &mut workers,
                now: &now,
                log: &self.logs,
                notify: &|subtitle, message| {
                    self.notify(subtitle, message);
                    Ok(())
                },
            };

            run_cycle(&mut deps, over).expect("the cycle runs");
        }

        fn watch_forever(
            &self,
            stop: &StopRequest,
            replaced: Option<&dyn Fn() -> bool>,
            pause: &mut dyn FnMut(Duration),
        ) -> Result<(), Failure> {
            let mut github = FakeGitHub(self);
            let mut workers = self.workers();
            let now = || self.world.borrow().now.to_zoned_datetime(TimeZone::UTC);
            watch_forever(Watch {
                cycle: CycleDeps {
                    paths: &self.paths,
                    github: &mut github,
                    workers: &mut workers,
                    now: &now,
                    log: &self.logs,
                    notify: &|subtitle, message| {
                        self.notify(subtitle, message);
                        Ok(())
                    },
                },
                stop,
                pause,
                replaced,
                ended: &|| None,
            })
        }

        fn notify(&self, subtitle: &str, message: &str) {
            self.notices
                .borrow_mut()
                .push((subtitle.to_owned(), message.to_owned()));
        }

        // The real workers of both apps, with the app and the t3 CLI answering from the world.
        fn workers(&self) -> AppWorkers<CodexWorkers<'_>, T3Workers<'_>> {
            let call: CallApp<'_> =
                Box::new(|caller, tool, arguments| self.call_app(caller, tool, &arguments));
            AppWorkers {
                codex: CodexWorkers::new(call, Box::new(|| Ok(self.world.borrow().sender.clone()))),
                t3: T3Workers::new(Box::new(|arguments, input, _| {
                    self.run_t3(arguments, input)
                })),
            }
        }

        // The owner's last turn ended long before noon, so the worker never reads as busy.
        fn call_app(
            &self,
            caller: &str,
            tool: &str,
            arguments: &Map<String, Value>,
        ) -> Map<String, Value> {
            if tool == "read_thread" {
                let noon = at(NOON).as_second();
                let read = json!({
                    "thread": { "id": arguments["threadId"], "status": { "type": "idle" } },
                    "turns": [{
                        "status": "completed",
                        "startedAt": noon - 3600,
                        "completedAt": noon - 3500,
                        "items": [{ "type": "userMessage", "content": [{ "type": "text", "text": "go" }] }],
                    }],
                });
                return object(json!({
                    "outcome": "done",
                    "result": { "content": [{ "type": "text", "text": read.to_string() }] },
                }));
            }

            self.calls.borrow_mut().sent.push(SentCall {
                caller: caller.to_owned(),
                thread: arguments["threadId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                prompt: arguments["prompt"].as_str().unwrap_or_default().to_owned(),
                saved: self.state().status,
            });
            object(json!({ "outcome": self.world.borrow().send_outcome }))
        }

        fn run_t3(&self, arguments: &[&str], input: &str) -> T3Ran {
            if arguments.get(1) == Some(&"show") {
                let world = self.world.borrow();
                let shown = json!({ "thread": world.t3_thread, "messages": world.t3_messages });
                return T3Ran::Ran {
                    code: 0,
                    stdout: shown.to_string(),
                    stderr: String::new(),
                };
            }
            self.calls.borrow_mut().t3.push((
                arguments
                    .iter()
                    .map(|argument| (*argument).to_owned())
                    .collect(),
                input.to_owned(),
            ));
            T3Ran::Ran {
                code: 0,
                stdout: "{}".to_owned(),
                stderr: String::new(),
            }
        }

        fn state(&self) -> WatchState {
            watch_record::load_watch_state(&self.paths.state, &pay()).expect("load the state")
        }

        fn state_path(&self) -> std::path::PathBuf {
            self.paths
                .state
                .join(watch_record::pr_key(&pay()))
                .join("state.json")
        }

        fn link_path(&self) -> std::path::PathBuf {
            self.paths
                .state
                .join(watch_record::pr_key(&pay()))
                .join("link.json")
        }

        fn sent(&self) -> Vec<SentCall> {
            self.calls.borrow().sent.clone()
        }

        fn prompt(&self, index: usize) -> String {
            self.calls.borrow().sent[index].prompt.clone()
        }

        fn logged(&self, event: &str) -> Vec<Entry> {
            self.logs
                .0
                .borrow()
                .iter()
                .filter(|entry| entry.event == event)
                .cloned()
                .collect()
        }

        fn events(&self) -> Vec<&'static str> {
            self.logs
                .0
                .borrow()
                .iter()
                .map(|entry| entry.event)
                .collect()
        }
    }

    impl Log for Recorded {
        fn write(&self, entry: Entry) {
            self.0.borrow_mut().push(entry);
        }
    }

    impl GitHub for FakeGitHub<'_> {
        fn summaries(
            &mut self,
            _account: &str,
            prs: &[PrRef],
            _bots: &[ReviewBot],
        ) -> Result<Vec<Option<PrSummary>>, ReadError> {
            self.0.calls.borrow_mut().summaries += 1;
            let world = self.0.world.borrow();
            Ok(prs.iter().map(|_| Some(world.summary.clone())).collect())
        }

        fn detail(
            &mut self,
            _account: &str,
            pr: &PrRef,
            _bots: &[ReviewBot],
        ) -> Result<PrDetail, ReadError> {
            self.0.calls.borrow_mut().details += 1;
            let world = self.0.world.borrow();
            if world.detail_fails {
                return Err(ReadError {
                    kind: ReadErrorKind::Unreachable,
                    message: "GitHub is down".to_owned(),
                });
            }
            Ok(PrDetail {
                summary: world.summary.clone(),
                url: format!("https://github.com/{}/pull/{}", pr.repo, pr.number),
                title: "Pay".to_owned(),
                failed_required: Vec::new(),
                comments: world.comments.clone(),
                no_actionable: Vec::new(),
            })
        }

        fn open_pulls(&mut self, _account: &str, _repo: &str) -> Result<Vec<PrRef>, ReadError> {
            Ok(Vec::new())
        }
    }

    fn summary() -> PrSummary {
        PrSummary {
            state: "OPEN".to_owned(),
            is_draft: false,
            head: HEAD.to_owned(),
            updated_at: "2026-10-23T01:00:00Z".to_owned(),
            mergeable: "MERGEABLE".to_owned(),
            bots: BTreeMap::from([(
                "coderabbit".to_owned(),
                Some(BotStatus {
                    state: "SUCCESS".to_owned(),
                    description: "Review completed".to_owned(),
                    created_at: "2026-10-23T01:00:00Z".to_owned(),
                }),
            )]),
            rollup: Some("SUCCESS".to_owned()),
        }
    }

    fn comment() -> PrComment {
        PrComment {
            id: "inline:1".to_owned(),
            kind: ItemKind::Inline,
            author: "coderabbitai[bot]".to_owned(),
            url: format!("{PR}#discussion_r1"),
            created_at: "2026-10-23T00:30:00Z".to_owned(),
            body: "**🟠 Major**".to_owned(),
            path: Some("src/pay.kt".to_owned()),
            line: Some(42),
            reply: false,
            resolved: false,
            approved: false,
        }
    }

    fn t3_thread(fields: Value) -> Value {
        let mut thread = json!({
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

    fn pay() -> PrRef {
        PrRef {
            repo: "org/pay".to_owned(),
            number: 7,
        }
    }

    fn at(time: &str) -> Timestamp {
        time.parse().expect("parse the time")
    }

    fn object(value: Value) -> Map<String, Value> {
        let Value::Object(object) = value else {
            panic!("expected an object, got {value}");
        };
        object
    }
}
