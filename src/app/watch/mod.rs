pub(crate) mod classify;
pub(crate) mod config;
pub(crate) mod events;
mod field;
pub(crate) mod message;
pub(crate) mod pr;
pub(crate) mod reviewers;
pub(crate) mod state;

use jiff::{SignedDuration, Timestamp, Zoned};
use serde_json::{Map, Value};

use self::{
    config::{QuietHours, WatchConfig},
    message::{follow_up_of, message_of, missing_from},
    pr::PrDetail,
    state::{Delivered, Finding, PrSummary, WatchEvent, WatchState, WatchStatus},
};
use crate::app::time;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Opened<'config> {
    pub(crate) handed_over: Option<HandedOver>,
    // `None` leaves the PR unread this cycle.
    pub(crate) account: Option<&'config str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HandedOver {
    pub(crate) from: String,
    pub(crate) to: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Observed {
    Unreadable,
    Ended,
    // Nothing more to read this cycle.
    Kept { paused: bool },
    ReadDetail { paused: bool, resumed: bool },
}

// While the app cannot be asked, the caller delivers nothing and the next cycle asks again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorkerView {
    Gone,
    Present {
        // Whether a message would interrupt the worker now.
        busy: bool,
        // The turn the awaited message started, or `None` while none is seen.
        answer: Option<Answer>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Answer {
    pub(crate) status: AnswerStatus,
    pub(crate) text: String,
}

// A turn that ended any other way than completed was stopped by the owner, who then has the
// conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AnswerStatus {
    InProgress,
    Completed,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Delivery {
    Stopped,
    Hold(Option<AnswerNote>),
    Send(Option<AnswerNote>),
}

// What the answer to the last message showed, for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AnswerNote {
    NotSeen,
    Incomplete { missing: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Sending {
    pub(crate) prompt: String,
    findings: Vec<Finding>,
    follow_up: bool,
    // Saved before the call, so a watch stopped mid-send never sends the message again.
    pub(crate) marked: WatchState,
}

// `Unknown` means the message may have reached the worker; it carries the worker's own report,
// which the halted record keeps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SendOutcome {
    Done,
    NotSent,
    Unknown(Map<String, Value>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SendResult {
    Delivered { follow_up: bool, findings: usize },
    NotSent,
    Halted,
}

// Timers drift by a few hundred milliseconds, so a PR due at this cycle is not left to the next
// one.
const DUE_MARGIN: SignedDuration = SignedDuration::from_secs(5);

// An answer never seen in this long is given up on, so one lost turn cannot stop every later
// message to the worker.
const ANSWER_WAIT: SignedDuration = SignedDuration::from_hours(24);

const OPEN: &str = "OPEN";

pub(crate) fn open<'config>(
    state: &mut WatchState,
    worker: &str,
    repo: &str,
    config: &'config WatchConfig,
) -> Opened<'config> {
    let handed_over = hand_over(state, worker);
    if state.status.is_finished() {
        return Opened {
            handed_over,
            account: None,
        };
    }
    let account = config.account_for(repo);
    if account.is_none() {
        state.reason = Some("repository is not configured".to_owned());
    }
    Opened {
        handed_over,
        account,
    }
}

// Moves a PR's watch to the chat it was linked to last.
fn hand_over(state: &mut WatchState, worker: &str) -> Option<HandedOver> {
    if state.worker.as_deref() == Some(worker) {
        return None;
    }
    let previous = state.worker.replace(worker.to_owned());
    // A state that has never named its chat takes the linked one as its own.
    let previous = previous.filter(|_| state.status != WatchStatus::Ended)?;

    // What only the last chat was told goes to the new one again, which never answers what the
    // watch awaited from the last one.
    let told = state
        .delivered
        .take()
        .map(|delivered| delivered.findings)
        .unwrap_or_default()
        .into_iter()
        .chain(std::mem::take(&mut state.follow_up))
        .filter(|finding| !is_queued(&state.pending, finding))
        .map(|finding| WatchEvent::Finding { finding })
        .collect::<Vec<_>>();
    state.pending.splice(0..0, told);
    state.halted = None;
    // A watch stopped or halted for the last chat runs again.
    if matches!(state.status, WatchStatus::Stopped | WatchStatus::Halted) {
        state.status = WatchStatus::Watching;
        state.reason = None;
    }
    Some(HandedOver {
        from: previous,
        to: worker.to_owned(),
    })
}

pub(crate) fn is_due(state: &WatchState, now: Timestamp, config: &WatchConfig) -> bool {
    // A PR nothing has happened to for a while is polled less often.
    let idle_after = SignedDuration::try_from_secs_f64(config.idle_after_hours * 3600.0)
        .unwrap_or(SignedDuration::MAX);
    let interval = if Elapsed::since(state.changed_at.as_deref(), now).at_least(idle_after) {
        config.idle_interval_seconds
    } else {
        config.poll_interval_seconds
    };
    Elapsed::since(state.checked_at.as_deref(), now).at_least(seconds(interval) - DUE_MARGIN)
}

pub(crate) fn observe(
    state: &mut WatchState,
    summary: Option<&PrSummary>,
    now: Timestamp,
) -> Observed {
    state.checked_at = Some(time::format_timestamp(now));
    let Some(summary) = summary else {
        return Observed::Unreadable;
    };

    let changed = state.summary.as_ref() != Some(summary);
    if changed {
        state.changed_at = Some(time::format_timestamp(now));
    }
    if summary.state != OPEN {
        state.status = WatchStatus::Ended;
        state.reason = Some(summary.state.to_lowercase());
        state.summary = Some(summary.clone());
        return Observed::Ended;
    }

    if summary.is_draft {
        let paused = state.status != WatchStatus::Draft;
        state.status = WatchStatus::Draft;
        // A draft is still read the first time, so what it had before the watch started is never
        // sent once it is ready.
        if state.summary.is_some() {
            state.summary = Some(summary.clone());
            return Observed::Kept { paused };
        }
        return Observed::ReadDetail {
            paused,
            resumed: false,
        };
    }

    let resumed = state.status == WatchStatus::Draft;
    state.status = WatchStatus::Watching;
    if !changed && !resumed {
        return Observed::Kept { paused: false };
    }
    Observed::ReadDetail {
        paused: false,
        resumed,
    }
}

// Called only for a full read that succeeded, so after a failed one the summary stays as it was
// and the next cycle reads the detail again.
pub(crate) fn take_in<'state>(
    state: &'state mut WatchState,
    summary: &PrSummary,
    detail: &PrDetail,
    linked_at: &str,
    owner: &str,
    config: &WatchConfig,
) -> &'state [WatchEvent] {
    let found = events::next_events(detail, state, linked_at, owner, config);
    state.summary = Some(summary.clone());
    state.seen = found.seen;
    state.conflict_head = found.conflict_head;
    let queued = state.pending.len();
    state.pending.extend(found.events);
    &state.pending[queued..]
}

#[must_use]
pub(crate) fn awaits_delivery(state: &WatchState) -> bool {
    state.status == WatchStatus::Watching
        && (!state.pending.is_empty() || !state.follow_up.is_empty() || state.delivered.is_some())
}

// True when the reason is new, which is when it is worth a log line.
pub(crate) fn hold_for_sender(state: &mut WatchState, missing: &str) -> bool {
    if state.reason.as_deref() == Some(missing) {
        return false;
    }
    state.reason = Some(missing.to_owned());
    true
}

pub(crate) fn deliver(
    state: &mut WatchState,
    view: WorkerView,
    now: &Zoned,
    config: &WatchConfig,
) -> Delivery {
    let WorkerView::Present { busy, answer } = view else {
        state.status = WatchStatus::Stopped;
        state.reason = Some("worker thread is gone".to_owned());
        return Delivery::Stopped;
    };
    state.reason = None;

    let Awaited::Taken(note) = take_answer(state, answer.as_ref(), now.timestamp()) else {
        return Delivery::Hold(None);
    };
    if (state.pending.is_empty() && state.follow_up.is_empty())
        || in_quiet_hours(now, config.quiet_hours)
        || busy
    {
        return Delivery::Hold(note);
    }
    Delivery::Send(note)
}

fn take_answer(state: &mut WatchState, answer: Option<&Answer>, now: Timestamp) -> Awaited {
    let Some(delivered) = &state.delivered else {
        return Awaited::Taken(None);
    };
    let note = match answer {
        None if Elapsed::since(Some(delivered.at.as_str()), now).less_than(ANSWER_WAIT) => {
            return Awaited::Coming;
        }
        None => Some(AnswerNote::NotSeen),
        Some(Answer {
            status: AnswerStatus::InProgress,
            ..
        }) => return Awaited::Coming,
        Some(Answer {
            status: AnswerStatus::Completed,
            text,
        }) if !delivered.follow_up => {
            state.follow_up = missing_from(text, &delivered.findings);
            (!state.follow_up.is_empty()).then_some(AnswerNote::Incomplete {
                missing: state.follow_up.len(),
            })
        }
        Some(_) => None,
    };
    state.delivered = None;
    Awaited::Taken(note)
}

pub(crate) fn prepare_send(
    state: &WatchState,
    pr: &str,
    now: Timestamp,
    config: &WatchConfig,
) -> Sending {
    let instructions = config.instructions.as_deref();
    // The follow-up goes alone and first, so it asks only about what the last answer left out.
    let follow_up = !state.follow_up.is_empty();
    let (prompt, findings) = if follow_up {
        (
            follow_up_of(pr, &state.follow_up, instructions),
            state.follow_up.clone(),
        )
    } else {
        (
            message_of(pr, &state.pending, instructions),
            state
                .pending
                .iter()
                .filter_map(|event| match event {
                    WatchEvent::Finding { finding } => Some(finding.clone()),
                    _ => None,
                })
                .collect(),
        )
    };
    let marked = WatchState {
        status: WatchStatus::Halted,
        halted: Some(halted(now, &prompt, Value::Null)),
        ..state.clone()
    };
    Sending {
        prompt,
        findings,
        follow_up,
        marked,
    }
}

// `sent_at` is taken just before the call, so a turn started while the cycle read GitHub is never
// taken for the answer.
pub(crate) fn finish_send(
    state: &mut WatchState,
    sending: Sending,
    sender: &str,
    outcome: SendOutcome,
    sent_at: Timestamp,
    now: Timestamp,
) -> SendResult {
    match outcome {
        SendOutcome::Done => {
            if sending.follow_up {
                state.follow_up.clear();
            } else {
                state.pending.clear();
            }
            let findings = sending.findings.len();
            state.delivered = Some(Delivered {
                at: time::format_timestamp(sent_at),
                sender: sender.to_owned(),
                prompt: sending.prompt,
                findings: sending.findings,
                follow_up: sending.follow_up,
            });
            SendResult::Delivered {
                follow_up: sending.follow_up,
                findings,
            }
        }
        SendOutcome::NotSent => SendResult::NotSent,
        // The message may have reached the worker, so it is never sent again on its own.
        SendOutcome::Unknown(detail) => {
            state.status = WatchStatus::Halted;
            state.halted = Some(halted(now, &sending.prompt, Value::Object(detail)));
            SendResult::Halted
        }
    }
}

fn is_queued(pending: &[WatchEvent], finding: &Finding) -> bool {
    pending.iter().any(
        |event| matches!(event, WatchEvent::Finding { finding: queued } if queued.id == finding.id),
    )
}

fn in_quiet_hours(now: &Zoned, quiet: Option<QuietHours>) -> bool {
    let Some(QuietHours { start, end }) = quiet else {
        return false;
    };
    let minute = u16::from(now.hour().unsigned_abs()) * 60 + u16::from(now.minute().unsigned_abs());
    if start <= end {
        (start..end).contains(&minute)
    } else {
        minute >= start || minute < end
    }
}

fn halted(now: Timestamp, prompt: &str, answer: Value) -> Map<String, Value> {
    Map::from_iter([
        ("at".to_owned(), Value::String(time::format_timestamp(now))),
        ("prompt".to_owned(), Value::String(prompt.to_owned())),
        ("answer".to_owned(), answer),
    ])
}

fn seconds(seconds: u64) -> SignedDuration {
    i64::try_from(seconds).map_or(SignedDuration::MAX, SignedDuration::from_secs)
}

// A message waits while the answer to the last one is still coming, so it never interrupts it.
enum Awaited {
    Coming,
    Taken(Option<AnswerNote>),
}

// A time never recorded is infinitely long ago, and one that does not parse compares as neither
// longer nor shorter, as the TypeScript watch's `NaN` did.
enum Elapsed {
    Never,
    Unreadable,
    Since(SignedDuration),
}

impl Elapsed {
    fn since(time: Option<&str>, now: Timestamp) -> Self {
        match time.map(str::parse::<Timestamp>) {
            None => Self::Never,
            Some(Err(_)) => Self::Unreadable,
            Some(Ok(time)) => Self::Since(now.duration_since(time)),
        }
    }

    fn at_least(&self, duration: SignedDuration) -> bool {
        match self {
            Self::Never => true,
            Self::Unreadable => false,
            Self::Since(elapsed) => *elapsed >= duration,
        }
    }

    fn less_than(&self, duration: SignedDuration) -> bool {
        match self {
            Self::Never | Self::Unreadable => false,
            Self::Since(elapsed) => *elapsed < duration,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use jiff::tz::TimeZone;
    use rstest::rstest;
    use serde_json::json;

    use super::*;
    use crate::app::watch::{
        config::Repository,
        pr::{ItemKind, PrComment},
        state::{BotStatus, bot_key},
    };

    const PR: &str = "https://github.com/org/pay/pull/7";
    const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const NOON: &str = "2026-10-23T12:00:00Z";

    mod hand_over {
        use super::*;

        #[test]
        fn same_chat_keeps_its_state() {
            let mut state = WatchState {
                worker: Some("w1".to_owned()),
                delivered: Some(delivered(vec![finding(1)])),
                ..WatchState::default()
            };
            let before = state.clone();

            let config = config();

            let opened = open(&mut state, "w1", "org/pay", &config);

            assert_eq!(opened.handed_over, None);
            assert_eq!(state, before);
        }

        #[test]
        fn first_chat_named_is_taken_as_its_own() {
            let mut state = WatchState {
                delivered: Some(delivered(vec![finding(1)])),
                ..WatchState::default()
            };

            let config = config();

            let opened = open(&mut state, "w1", "org/pay", &config);

            assert_eq!(opened.handed_over, None);
            assert_eq!(state.worker.as_deref(), Some("w1"));
            assert!(state.delivered.is_some());
        }

        #[test]
        fn new_chat_is_told_again_what_only_the_last_chat_was_told() {
            let mut state = WatchState {
                worker: Some("old".to_owned()),
                delivered: Some(delivered(vec![finding(1), finding(2)])),
                follow_up: vec![finding(3)],
                pending: vec![
                    WatchEvent::Finding {
                        finding: finding(2),
                    },
                    conflict(),
                ],
                halted: Some(Map::new()),
                ..WatchState::default()
            };

            let config = config();

            let opened = open(&mut state, "new", "org/pay", &config);

            assert_eq!(
                opened.handed_over,
                Some(HandedOver {
                    from: "old".to_owned(),
                    to: "new".to_owned(),
                })
            );
            assert_eq!(
                state,
                WatchState {
                    worker: Some("new".to_owned()),
                    pending: vec![
                        WatchEvent::Finding {
                            finding: finding(1),
                        },
                        WatchEvent::Finding {
                            finding: finding(3),
                        },
                        WatchEvent::Finding {
                            finding: finding(2),
                        },
                        conflict(),
                    ],
                    ..WatchState::default()
                }
            );
        }

        #[rstest]
        #[case::stopped(WatchStatus::Stopped)]
        #[case::halted(WatchStatus::Halted)]
        fn watch_stopped_for_the_last_chat_runs_again(#[case] status: WatchStatus) {
            let mut state = WatchState {
                status,
                reason: Some("worker thread is gone".to_owned()),
                worker: Some("old".to_owned()),
                ..WatchState::default()
            };

            let config = config();

            let opened = open(&mut state, "new", "org/pay", &config);

            assert_eq!(opened.account, Some("owner"));
            assert_eq!((state.status, state.reason), (WatchStatus::Watching, None));
        }

        #[test]
        fn ended_watch_takes_the_new_chat_without_telling_it_anything() {
            let mut state = WatchState {
                status: WatchStatus::Ended,
                worker: Some("old".to_owned()),
                delivered: Some(delivered(vec![finding(1)])),
                ..WatchState::default()
            };

            let config = config();

            let opened = open(&mut state, "new", "org/pay", &config);

            assert_eq!(
                opened,
                Opened {
                    handed_over: None,
                    account: None,
                }
            );
            assert_eq!(state.worker.as_deref(), Some("new"));
            assert!(state.pending.is_empty());
            assert!(state.delivered.is_some());
        }
    }

    mod opening {
        use super::*;

        #[rstest]
        #[case::ended(WatchStatus::Ended)]
        #[case::stopped(WatchStatus::Stopped)]
        #[case::halted(WatchStatus::Halted)]
        fn finished_watch_is_left_unread(#[case] status: WatchStatus) {
            let mut state = WatchState {
                status,
                worker: Some("w1".to_owned()),
                ..WatchState::default()
            };

            let config = config();

            let opened = open(&mut state, "w1", "org/pay", &config);

            assert_eq!(opened.account, None);
            assert_eq!(state.reason, None);
        }

        #[test]
        fn repository_outside_the_config_is_left_unread_with_the_reason() {
            let mut state = WatchState::default();

            let config = config();

            let opened = open(&mut state, "w1", "org/other", &config);

            assert_eq!(opened.account, None);
            assert_eq!(
                state.reason.as_deref(),
                Some("repository is not configured")
            );
        }

        #[rstest]
        #[case::watching(WatchStatus::Watching)]
        #[case::draft(WatchStatus::Draft)]
        fn running_watch_is_read_with_the_repository_account(#[case] status: WatchStatus) {
            let mut state = WatchState {
                status,
                ..WatchState::default()
            };

            let config = config();

            let opened = open(&mut state, "w1", "Org/Pay", &config);

            assert_eq!(opened.account, Some("owner"));
        }
    }

    mod polling {
        use super::*;

        #[test]
        fn pr_is_due_once_its_interval_has_passed() {
            for (name, checked_at, changed_at, expected) in [
                ("never_checked", None, None, true),
                (
                    "checked_within_the_margin",
                    Some("2026-10-23T11:59:05Z"),
                    Some(NOON),
                    true,
                ),
                (
                    "checked_too_recently",
                    Some("2026-10-23T11:59:10Z"),
                    Some(NOON),
                    false,
                ),
                (
                    "idle_checked_too_recently",
                    Some("2026-10-23T11:55:00Z"),
                    Some("2026-10-20T11:00:00Z"),
                    false,
                ),
                (
                    "idle_checked_long_ago",
                    Some("2026-10-23T11:50:05Z"),
                    Some("2026-10-20T11:00:00Z"),
                    true,
                ),
                (
                    "never_changed_counts_as_idle",
                    Some("2026-10-23T11:55:00Z"),
                    None,
                    false,
                ),
                ("unreadable_check_time", Some("soon"), Some(NOON), false),
            ] {
                let state = WatchState {
                    checked_at: checked_at.map(str::to_owned),
                    changed_at: changed_at.map(str::to_owned),
                    ..WatchState::default()
                };

                assert_eq!(
                    is_due(&state, at(NOON), &config()),
                    expected,
                    "case: {name}"
                );
            }
        }

        #[test]
        fn unreadable_pr_only_records_the_check() {
            let mut state = WatchState::default();

            let observed = observe(&mut state, None, at(NOON));

            assert_eq!(observed, Observed::Unreadable);
            assert_eq!(
                state,
                WatchState {
                    checked_at: Some("2026-10-23T12:00:00.000Z".to_owned()),
                    ..WatchState::default()
                }
            );
        }

        #[test]
        fn closed_pr_ends_its_watch() {
            let mut state = WatchState {
                summary: Some(summary()),
                ..WatchState::default()
            };
            let merged = PrSummary {
                state: "MERGED".to_owned(),
                ..summary()
            };

            let observed = observe(&mut state, Some(&merged), at(NOON));

            assert_eq!(observed, Observed::Ended);
            assert_eq!(
                (state.status, state.reason.as_deref(), state.summary),
                (WatchStatus::Ended, Some("merged"), Some(merged))
            );
        }

        #[test]
        fn draft_is_read_once_then_paused_until_it_is_ready() {
            let mut state = WatchState::default();
            let draft = PrSummary {
                is_draft: true,
                ..summary()
            };

            let first = observe(&mut state, Some(&draft), at(NOON));
            state.summary = Some(draft.clone());
            let again = observe(&mut state, Some(&draft), at(NOON));
            let ready = observe(&mut state, Some(&summary()), at(NOON));

            assert_eq!(
                [first, again, ready],
                [
                    Observed::ReadDetail {
                        paused: true,
                        resumed: false,
                    },
                    Observed::Kept { paused: false },
                    Observed::ReadDetail {
                        paused: false,
                        resumed: true,
                    },
                ]
            );
            assert_eq!(state.status, WatchStatus::Watching);
        }

        #[test]
        fn open_pr_is_read_in_full_only_when_its_summary_changed() {
            let mut state = WatchState {
                summary: Some(summary()),
                changed_at: Some("2026-10-22T00:00:00.000Z".to_owned()),
                ..WatchState::default()
            };
            let updated = PrSummary {
                updated_at: "2026-10-23T11:00:00Z".to_owned(),
                ..summary()
            };

            let unchanged = observe(&mut state, Some(&summary()), at(NOON));
            let unchanged_since = state.changed_at.clone();
            let changed = observe(&mut state, Some(&updated), at(NOON));

            assert_eq!(unchanged, Observed::Kept { paused: false });
            assert_eq!(unchanged_since.as_deref(), Some("2026-10-22T00:00:00.000Z"));
            assert_eq!(
                changed,
                Observed::ReadDetail {
                    paused: false,
                    resumed: false,
                }
            );
            assert_eq!(
                state.changed_at.as_deref(),
                Some("2026-10-23T12:00:00.000Z")
            );
        }

        #[test]
        fn first_read_takes_what_is_on_the_pr_as_seen_and_later_reads_queue_what_is_new() {
            let mut state = WatchState::default();
            let first = detail(vec![comment(1, "2026-10-23T00:00:00Z")]);
            let later = detail(vec![
                comment(1, "2026-10-23T00:00:00Z"),
                comment(2, "2026-10-23T11:00:00Z"),
            ]);

            let queued_first = take_in(
                &mut state,
                &summary(),
                &first,
                LINKED_AT,
                "owner",
                &config(),
            )
            .len();
            let queued_later = take_in(
                &mut state,
                &summary(),
                &later,
                LINKED_AT,
                "owner",
                &config(),
            )
            .iter()
            .map(|event| match event {
                WatchEvent::Finding { finding } => finding.id.clone(),
                other => format!("{other:?}"),
            })
            .collect::<Vec<_>>();

            assert_eq!(queued_first, 0);
            assert_eq!(queued_later, ["inline:2"]);
            assert_eq!(state.summary, Some(summary()));
            assert_eq!(state.pending.len(), 1);
        }
    }

    mod delivery {
        use super::*;

        #[test]
        fn watch_with_something_to_send_or_an_answer_to_await_awaits_delivery() {
            for (name, state, expected) in [
                ("nothing", WatchState::default(), false),
                (
                    "pending",
                    WatchState {
                        pending: vec![conflict()],
                        ..WatchState::default()
                    },
                    true,
                ),
                (
                    "follow_up",
                    WatchState {
                        follow_up: vec![finding(1)],
                        ..WatchState::default()
                    },
                    true,
                ),
                (
                    "awaited_answer",
                    WatchState {
                        delivered: Some(delivered(vec![])),
                        ..WatchState::default()
                    },
                    true,
                ),
                (
                    "draft_with_pending",
                    WatchState {
                        status: WatchStatus::Draft,
                        pending: vec![conflict()],
                        ..WatchState::default()
                    },
                    false,
                ),
            ] {
                assert_eq!(awaits_delivery(&state), expected, "case: {name}");
            }
        }

        #[test]
        fn missing_sender_is_recorded_and_logged_only_when_it_changes() {
            let mut state = WatchState::default();

            let first = hold_for_sender(&mut state, "no chat is registered");
            let again = hold_for_sender(&mut state, "no chat is registered");

            assert_eq!([first, again], [true, false]);
            assert_eq!(state.reason.as_deref(), Some("no chat is registered"));
        }

        #[test]
        fn gone_worker_stops_the_watch() {
            let mut state = pending_state();

            let delivery = deliver(&mut state, WorkerView::Gone, &zoned(NOON), &config());

            assert_eq!(delivery, Delivery::Stopped);
            assert_eq!(
                (state.status, state.reason.as_deref()),
                (WatchStatus::Stopped, Some("worker thread is gone"))
            );
        }

        #[test]
        fn idle_worker_gets_what_is_pending() {
            let mut state = WatchState {
                reason: Some("no chat is registered".to_owned()),
                ..pending_state()
            };

            let delivery = deliver(&mut state, idle(None), &zoned(NOON), &config());

            assert_eq!(delivery, Delivery::Send(None));
            assert_eq!(state.reason, None);
        }

        #[test]
        fn message_waits_while_it_would_interrupt_or_has_nothing_to_say() {
            let quiet = WatchConfig {
                quiet_hours: Some(QuietHours {
                    start: 11 * 60,
                    end: 13 * 60,
                }),
                ..config()
            };
            for (name, state, view, config) in [
                (
                    "busy_worker",
                    pending_state(),
                    WorkerView::Present {
                        busy: true,
                        answer: None,
                    },
                    config(),
                ),
                ("quiet_hours", pending_state(), idle(None), quiet),
                (
                    "nothing_pending",
                    WatchState::default(),
                    idle(None),
                    config(),
                ),
            ] {
                let mut state = state;

                let delivery = deliver(&mut state, view, &zoned(NOON), &config);

                assert_eq!(delivery, Delivery::Hold(None), "case: {name}");
            }
        }

        #[test]
        fn answer_still_to_come_holds_the_next_message() {
            for (name, answer, now) in [
                (
                    "turn_in_progress",
                    Some(answer(AnswerStatus::InProgress, "")),
                    NOON,
                ),
                ("not_seen_within_a_day", None, "2026-10-24T09:59:59Z"),
            ] {
                let mut state = WatchState {
                    delivered: Some(delivered(vec![finding(1)])),
                    ..pending_state()
                };

                let delivery = deliver(&mut state, idle(answer), &zoned(now), &config());

                assert_eq!(delivery, Delivery::Hold(None), "case: {name}");
                assert!(state.delivered.is_some(), "case: {name}");
            }
        }

        #[test]
        fn answer_is_taken_before_the_next_message_goes_out() {
            for (name, answer, now, note, follow_up) in [
                (
                    "complete_answer",
                    Some(answer(
                        AnswerStatus::Completed,
                        &format!("{PR}#discussion_r1"),
                    )),
                    NOON,
                    None,
                    vec![],
                ),
                (
                    "answer_missing_a_link",
                    Some(answer(AnswerStatus::Completed, "指摘なし")),
                    NOON,
                    Some(AnswerNote::Incomplete { missing: 1 }),
                    vec![finding(1)],
                ),
                (
                    "turn_stopped_by_the_owner",
                    Some(answer(AnswerStatus::Other, "")),
                    NOON,
                    None,
                    vec![],
                ),
                (
                    "not_seen_for_a_day",
                    None,
                    "2026-10-24T10:00:00Z",
                    Some(AnswerNote::NotSeen),
                    vec![],
                ),
            ] {
                let mut state = WatchState {
                    delivered: Some(delivered(vec![finding(1)])),
                    ..pending_state()
                };

                let delivery = deliver(&mut state, idle(answer), &zoned(now), &config());

                assert_eq!(delivery, Delivery::Send(note), "case: {name}");
                assert_eq!(state.delivered, None, "case: {name}");
                assert_eq!(state.follow_up, follow_up, "case: {name}");
            }
        }

        #[test]
        fn answer_to_a_follow_up_is_never_followed_up_again() {
            let mut state = WatchState {
                delivered: Some(Delivered {
                    follow_up: true,
                    ..delivered(vec![finding(1)])
                }),
                ..WatchState::default()
            };

            let delivery = deliver(
                &mut state,
                idle(Some(answer(AnswerStatus::Completed, "指摘なし"))),
                &zoned(NOON),
                &config(),
            );

            assert_eq!(delivery, Delivery::Hold(None));
            assert_eq!((state.delivered, state.follow_up), (None, vec![]));
        }

        #[test]
        fn quiet_hours_may_run_over_midnight() {
            for (name, quiet, now, expected) in [
                ("off", None, "2026-10-23T23:00:00Z", false),
                (
                    "inside_overnight",
                    Some((22, 5)),
                    "2026-10-23T23:00:00Z",
                    true,
                ),
                (
                    "after_midnight",
                    Some((22, 5)),
                    "2026-10-23T04:59:00Z",
                    true,
                ),
                ("at_the_end", Some((22, 5)), "2026-10-23T05:00:00Z", false),
                (
                    "inside_daytime",
                    Some((9, 17)),
                    "2026-10-23T09:00:00Z",
                    true,
                ),
                (
                    "outside_daytime",
                    Some((9, 17)),
                    "2026-10-23T17:30:00Z",
                    false,
                ),
            ] {
                let quiet = quiet.map(|(start, end)| QuietHours {
                    start: start * 60,
                    end: end * 60,
                });

                assert_eq!(in_quiet_hours(&zoned(now), quiet), expected, "case: {name}");
            }
        }
    }

    mod sending {
        use super::*;

        #[test]
        fn pending_events_go_out_marked_halted_before_the_call() {
            let state = pending_state();

            let sending = prepare_send(&state, PR, at(NOON), &config());

            let prompt = message_of(PR, &state.pending, Some("$task-worker"));
            assert_eq!(sending.prompt, prompt);
            assert_eq!(
                sending.marked,
                WatchState {
                    status: WatchStatus::Halted,
                    halted: json!({ "at": "2026-10-23T12:00:00.000Z", "prompt": prompt, "answer": null })
                        .as_object()
                        .cloned(),
                    ..state
                }
            );
        }

        #[test]
        fn follow_up_goes_alone_and_first() {
            let state = WatchState {
                follow_up: vec![finding(2)],
                ..pending_state()
            };

            let sending = prepare_send(&state, PR, at(NOON), &config());

            assert_eq!(
                sending.prompt,
                follow_up_of(PR, &[finding(2)], Some("$task-worker"))
            );
        }

        #[test]
        fn delivered_message_is_awaited_from_when_it_was_sent() {
            let mut state = pending_state();
            let sending = prepare_send(&state, PR, at(NOON), &config());
            let prompt = sending.prompt.clone();

            let sent = finish_send(
                &mut state,
                sending,
                "sender-chat",
                SendOutcome::Done,
                at("2026-10-23T12:00:03Z"),
                at(NOON),
            );

            assert_eq!(
                sent,
                SendResult::Delivered {
                    follow_up: false,
                    findings: 1,
                }
            );
            assert_eq!(
                state,
                WatchState {
                    delivered: Some(Delivered {
                        at: "2026-10-23T12:00:03.000Z".to_owned(),
                        sender: "sender-chat".to_owned(),
                        prompt,
                        findings: vec![finding(1)],
                        follow_up: false,
                    }),
                    ..WatchState::default()
                }
            );
        }

        #[test]
        fn delivered_follow_up_leaves_the_pending_events_queued() {
            let mut state = WatchState {
                follow_up: vec![finding(2)],
                ..pending_state()
            };
            let sending = prepare_send(&state, PR, at(NOON), &config());

            let sent = finish_send(
                &mut state,
                sending,
                "sender-chat",
                SendOutcome::Done,
                at(NOON),
                at(NOON),
            );

            assert_eq!(
                sent,
                SendResult::Delivered {
                    follow_up: true,
                    findings: 1,
                }
            );
            assert_eq!(state.pending, pending_state().pending);
            assert!(state.follow_up.is_empty());
            assert_eq!(
                state.delivered.map(|delivered| delivered.follow_up),
                Some(true)
            );
        }

        #[test]
        fn message_not_sent_stays_queued_for_the_next_cycle() {
            let mut state = pending_state();
            let sending = prepare_send(&state, PR, at(NOON), &config());

            let sent = finish_send(
                &mut state,
                sending,
                "sender-chat",
                SendOutcome::NotSent,
                at(NOON),
                at(NOON),
            );

            assert_eq!(sent, SendResult::NotSent);
            assert_eq!(state, pending_state());
        }

        #[test]
        fn message_that_may_have_arrived_halts_the_watch_with_the_worker_account() {
            let mut state = pending_state();
            let sending = prepare_send(&state, PR, at(NOON), &config());
            let prompt = sending.prompt.clone();
            let detail = json!({ "outcome": "unknown" })
                .as_object()
                .cloned()
                .expect("an object");

            let sent = finish_send(
                &mut state,
                sending,
                "sender-chat",
                SendOutcome::Unknown(detail),
                at(NOON),
                at(NOON),
            );

            assert_eq!(sent, SendResult::Halted);
            assert_eq!(state.status, WatchStatus::Halted);
            assert_eq!(
                state.halted,
                json!({
                    "at": "2026-10-23T12:00:00.000Z",
                    "prompt": prompt,
                    "answer": { "outcome": "unknown" },
                })
                .as_object()
                .cloned()
            );
        }
    }

    // The records harnexus-task watch left on a Mac, read and run as a restarted watch would.
    mod legacy_records {
        use super::*;

        struct LegacyRecord {
            number: u64,
            url: String,
            worker: String,
            linked_at: String,
            state: WatchState,
        }

        #[derive(Default)]
        struct Cycle {
            sent: Vec<(String, String)>,
            handed_over: Vec<HandedOver>,
        }

        #[test]
        fn records_keep_their_queue_awaited_answer_and_pending_hand_over() {
            let records = records();

            let shown = records
                .iter()
                .map(|record| {
                    (
                        record.number,
                        record.worker.as_str(),
                        record.state.status,
                        record.state.pending.len(),
                        record.state.delivered.is_some(),
                        record.state.worker.as_deref() != Some(record.worker.as_str()),
                    )
                })
                .collect::<Vec<_>>();

            assert_eq!(
                shown,
                [
                    (7, "next-chat", WatchStatus::Watching, 0, true, true),
                    (8, "w8", WatchStatus::Watching, 0, false, false),
                    (9, "w9", WatchStatus::Watching, 1, false, false),
                    (10, "w10", WatchStatus::Halted, 0, false, false),
                ]
            );
        }

        #[test]
        fn cycle_goes_on_without_resending_and_hands_a_pr_over() {
            let mut records = records();

            let cycle = run_cycle(&mut records, NOON);

            let sent = cycle
                .sent
                .iter()
                .map(|(worker, prompt)| {
                    (
                        worker.as_str(),
                        prompt.contains("discussion_r71"),
                        prompt.contains("discussion_r91"),
                    )
                })
                .collect::<Vec<_>>();
            assert_eq!(sent, [("next-chat", true, false), ("w9", false, true)]);
            assert_eq!(
                cycle.handed_over,
                [HandedOver {
                    from: "old-chat".to_owned(),
                    to: "next-chat".to_owned(),
                }]
            );
            let [handed, older, queued, _] = &records[..] else {
                panic!("four records");
            };
            assert_eq!(
                (
                    handed.state.worker.as_deref(),
                    handed
                        .state
                        .delivered
                        .as_ref()
                        .map(|delivered| delivered.sender.as_str()),
                ),
                (Some("next-chat"), Some("sender-chat"))
            );
            assert!(queued.state.pending.is_empty());
            assert_eq!(
                older.state.seen,
                [
                    "inline:81".to_owned(),
                    format!("coderabbit:{HEAD}"),
                    bot_key("coderabbit")
                ]
            );
            assert_eq!(
                older.state.summary.as_ref().map(|summary| &summary.bots),
                Some(&legacy_summary().bots)
            );
        }

        #[test]
        fn restarted_watch_sends_nothing_twice() {
            let mut records = records();
            let first = run_cycle(&mut records, NOON).sent.len();

            restart(&mut records);
            let later = run_cycle(&mut records, "2026-10-23T12:02:00Z").sent.len();
            restart(&mut records);
            let latest = run_cycle(&mut records, "2026-10-23T12:04:00Z").sent.len();

            assert_eq!((first, later, latest), (2, 0, 0));
        }

        #[test]
        fn halted_pr_stays_halted_and_gets_nothing() {
            let mut records = records();

            let cycle = run_cycle(&mut records, NOON);

            assert!(cycle.sent.iter().all(|(worker, _)| worker != "w10"));
            let halted = records
                .iter()
                .find(|record| record.number == 10)
                .expect("PR 10");
            assert_eq!(halted.state.status, WatchStatus::Halted);
            assert!(halted.state.halted.is_some());
        }

        // GitHub shows every PR the same summary and only the comments its record already holds,
        // and every worker is idle with no answer seen yet.
        fn run_cycle(records: &mut [LegacyRecord], now: &str) -> Cycle {
            let config = WatchConfig {
                quiet_hours: None,
                ..config()
            };
            let now = zoned(now);
            let summary = legacy_summary();
            let mut cycle = Cycle::default();
            let mut polled = Vec::new();
            for (index, record) in records.iter_mut().enumerate() {
                let opened = open(&mut record.state, &record.worker, "org/pay", &config);
                cycle.handed_over.extend(opened.handed_over);
                if opened.account.is_some() {
                    polled.push(index);
                }
            }

            for &index in &polled {
                let record = &mut records[index];
                if !is_due(&record.state, now.timestamp(), &config) {
                    continue;
                }
                if let Observed::ReadDetail { .. } =
                    observe(&mut record.state, Some(&summary), now.timestamp())
                {
                    let detail = PrDetail {
                        summary: summary.clone(),
                        failed_required: Vec::new(),
                        comments: vec![PrComment {
                            id: format!("inline:{}1", record.number),
                            author: "coderabbitai[bot]".to_owned(),
                            url: format!("{}#discussion_r{}1", record.url, record.number),
                            created_at: "2026-10-21T00:00:00Z".to_owned(),
                            body: "**🟠 Major**".to_owned(),
                            ..comment(0, "")
                        }],
                        no_actionable: Vec::new(),
                    };
                    take_in(
                        &mut record.state,
                        &summary,
                        &detail,
                        &record.linked_at,
                        "owner",
                        &config,
                    );
                }
            }

            for &index in &polled {
                let record = &mut records[index];
                if !awaits_delivery(&record.state) {
                    continue;
                }
                if let Delivery::Send(_) = deliver(&mut record.state, idle(None), &now, &config) {
                    let sending =
                        prepare_send(&record.state, &record.url, now.timestamp(), &config);
                    cycle
                        .sent
                        .push((record.worker.clone(), sending.prompt.clone()));
                    finish_send(
                        &mut record.state,
                        sending,
                        "sender-chat",
                        SendOutcome::Done,
                        now.timestamp(),
                        now.timestamp(),
                    );
                }
            }
            cycle
        }

        // A new process reads what the last one saved.
        fn restart(records: &mut [LegacyRecord]) {
            for record in records {
                let saved = serde_json::to_string(&record.state).expect("save the state");
                record.state = serde_json::from_str(&saved).expect("read the state");
            }
        }

        fn records() -> Vec<LegacyRecord> {
            [
                (
                    7,
                    include_str!("../../../tests/fixtures/legacy-watch/02bff83354bf169d/link.json"),
                    include_str!(
                        "../../../tests/fixtures/legacy-watch/02bff83354bf169d/state.json"
                    ),
                ),
                (
                    8,
                    include_str!("../../../tests/fixtures/legacy-watch/3d0a4555db96a9b0/link.json"),
                    include_str!(
                        "../../../tests/fixtures/legacy-watch/3d0a4555db96a9b0/state.json"
                    ),
                ),
                (
                    9,
                    include_str!("../../../tests/fixtures/legacy-watch/6db070b27f9194fb/link.json"),
                    include_str!(
                        "../../../tests/fixtures/legacy-watch/6db070b27f9194fb/state.json"
                    ),
                ),
                (
                    10,
                    include_str!("../../../tests/fixtures/legacy-watch/e4fa9c7ae05e70a7/link.json"),
                    include_str!(
                        "../../../tests/fixtures/legacy-watch/e4fa9c7ae05e70a7/state.json"
                    ),
                ),
            ]
            .into_iter()
            .map(|(number, link, state)| {
                let link = serde_json::from_str::<Value>(link).expect("read the link");
                let text = |key: &str| link[key].as_str().expect("a text field").to_owned();
                LegacyRecord {
                    number,
                    url: text("url"),
                    worker: text("worker"),
                    linked_at: text("linkedAt"),
                    state: serde_json::from_str(state).expect("read the state"),
                }
            })
            .collect()
        }

        fn legacy_summary() -> PrSummary {
            PrSummary {
                updated_at: "2026-10-23T01:00:00Z".to_owned(),
                bots: BTreeMap::from([(
                    "coderabbit".to_owned(),
                    Some(BotStatus {
                        state: "SUCCESS".to_owned(),
                        description: "Review completed".to_owned(),
                        created_at: "2026-10-22T00:00:00Z".to_owned(),
                    }),
                )]),
                ..summary()
            }
        }
    }

    const LINKED_AT: &str = "2026-10-23T10:00:00.000Z";

    fn config() -> WatchConfig {
        WatchConfig {
            repositories: BTreeMap::from([(
                "org/pay".to_owned(),
                Repository {
                    account: "owner".to_owned(),
                },
            )]),
            review_bots: vec![reviewers::coderabbit()],
            ignore: Vec::new(),
            poll_interval_seconds: 60,
            idle_interval_seconds: 600,
            idle_after_hours: 72.0,
            quiet_hours: Some(QuietHours {
                start: 22 * 60,
                end: 5 * 60,
            }),
            instructions: Some("$task-worker".to_owned()),
        }
    }

    fn summary() -> PrSummary {
        PrSummary {
            state: "OPEN".to_owned(),
            is_draft: false,
            head: HEAD.to_owned(),
            updated_at: "2026-10-23T01:00:00Z".to_owned(),
            mergeable: "MERGEABLE".to_owned(),
            bots: BTreeMap::from([("coderabbit".to_owned(), None)]),
            rollup: Some("SUCCESS".to_owned()),
        }
    }

    fn detail(comments: Vec<PrComment>) -> PrDetail {
        PrDetail {
            summary: summary(),
            failed_required: Vec::new(),
            comments,
            no_actionable: Vec::new(),
        }
    }

    fn comment(id: u64, created_at: &str) -> PrComment {
        PrComment {
            id: format!("inline:{id}"),
            kind: ItemKind::Inline,
            author: "reviewer".to_owned(),
            url: format!("{PR}#discussion_r{id}"),
            created_at: created_at.to_owned(),
            body: "please rename".to_owned(),
            path: Some("src/pay.kt".to_owned()),
            line: Some(42),
            reply: false,
            resolved: false,
            approved: false,
        }
    }

    fn finding(id: u64) -> Finding {
        Finding {
            id: format!("inline:{id}"),
            kind: ItemKind::Inline,
            bot: None,
            label: "reviewer".to_owned(),
            path: Some("src/pay.kt".to_owned()),
            line: Some(42),
            url: format!("{PR}#discussion_r{id}"),
            created_at: "2026-10-23T11:00:00Z".to_owned(),
            approval: None,
        }
    }

    fn conflict() -> WatchEvent {
        WatchEvent::Conflict {
            head: HEAD.to_owned(),
        }
    }

    fn delivered(findings: Vec<Finding>) -> Delivered {
        Delivered {
            at: "2026-10-23T10:00:00.000Z".to_owned(),
            sender: "sender-chat".to_owned(),
            prompt: "go".to_owned(),
            findings,
            follow_up: false,
        }
    }

    fn pending_state() -> WatchState {
        WatchState {
            pending: vec![WatchEvent::Finding {
                finding: finding(1),
            }],
            ..WatchState::default()
        }
    }

    fn answer(status: AnswerStatus, text: &str) -> Answer {
        Answer {
            status,
            text: text.to_owned(),
        }
    }

    fn idle(answer: Option<Answer>) -> WorkerView {
        WorkerView::Present {
            busy: false,
            answer,
        }
    }

    fn at(time: &str) -> Timestamp {
        time.parse().expect("parse the time")
    }

    fn zoned(time: &str) -> Zoned {
        at(time).to_zoned(TimeZone::UTC)
    }
}
