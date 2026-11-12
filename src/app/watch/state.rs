use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::{
    field::{self, Recorded},
    pr::ItemKind,
};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", from = "StoredWatchState")]
pub(crate) struct WatchState {
    pub(crate) status: WatchStatus,
    pub(crate) reason: Option<String>,
    pub(crate) summary: Option<PrSummary>,
    pub(crate) checked_at: Option<String>,
    pub(crate) changed_at: Option<String>,
    // Keys of every event already queued or sent, so none is sent twice.
    pub(crate) seen: Vec<String>,
    pub(crate) pending: Vec<WatchEvent>,
    pub(crate) follow_up: Vec<Finding>,
    pub(crate) conflict_head: Option<String>,
    pub(crate) delivered: Option<Delivered>,
    // The chat the delivery fields belong to; a different chat in the link means the PR was
    // handed over.
    pub(crate) worker: Option<String>,
    // The message whose delivery could not be confirmed; nothing more is sent for the PR until it
    // is released.
    pub(crate) halted: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum WatchStatus {
    #[default]
    Watching,
    Draft,
    Ended,
    Stopped,
    Halted,
}

// What the light poll reads; a change in any of it is what makes the full read worth its cost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", from = "StoredSummary")]
pub(crate) struct PrSummary {
    pub(crate) state: String,
    pub(crate) is_draft: bool,
    pub(crate) head: String,
    pub(crate) updated_at: String,
    pub(crate) mergeable: String,
    // The progress of each review bot that keeps one, by the bot's id.
    pub(crate) bots: BTreeMap<String, Option<BotStatus>>,
    pub(crate) rollup: Option<String>,
}

// A check run is read into a commit status's terms: PENDING until it completes, then its
// conclusion, with an empty description.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BotStatus {
    pub(crate) state: String,
    pub(crate) description: String,
    pub(crate) created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FailedCheck {
    pub(crate) name: String,
    #[serde(deserialize_with = "field::nullable")]
    pub(crate) url: Option<String>,
}

// Only a comment's kind, author and place are kept, so a body can never steer what the worker is
// told.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", from = "StoredFinding")]
pub(crate) struct Finding {
    pub(crate) id: String,
    pub(crate) kind: ItemKind,
    // The id of the review bot that wrote it, or `None` for a person.
    pub(crate) bot: Option<String>,
    pub(crate) label: String,
    pub(crate) path: Option<String>,
    pub(crate) line: Option<u64>,
    pub(crate) url: String,
    pub(crate) created_at: String,
    // A person's approval, which is told even without a body.
    pub(crate) approval: Option<Approval>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Approval {
    WithBody,
    WithoutBody,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    from = "StoredEvent"
)]
pub(crate) enum WatchEvent {
    Finding {
        finding: Finding,
    },
    ReviewDone {
        // The bot's id and the name shown to the worker.
        bot: String,
        name: String,
        head: String,
        findings: usize,
        summary_agrees: bool,
    },
    Conflict {
        head: String,
    },
    CiFailed {
        head: String,
        check: FailedCheck,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Delivered {
    pub(crate) at: String,
    pub(crate) sender: String,
    pub(crate) prompt: String,
    pub(crate) findings: Vec<Finding>,
    pub(crate) follow_up: bool,
}

// Marks a bot whose reviews the watch has taken in.
const BOT_KEY: &str = "bot:";

pub(crate) fn bot_key(id: &str) -> String {
    format!("{BOT_KEY}{id}")
}

impl WatchStatus {
    // No command brings an ended PR back, so a watch may stop reading it for as long as it runs;
    // a stopped one stays readable, since naming it from another worker resumes it.
    #[must_use]
    pub(crate) const fn is_over(self) -> bool {
        matches!(self, Self::Ended)
    }

    // Nothing is read or sent for these; a release or another chat's link resumes a halted or
    // stopped one.
    #[must_use]
    pub(crate) const fn is_finished(self) -> bool {
        matches!(self, Self::Ended | Self::Stopped | Self::Halted)
    }

    // Only a watch stopped for a gone chat has let its chat go; a halted one may have just reached
    // it.
    #[must_use]
    pub(crate) const fn holds_chat(self) -> bool {
        !matches!(self, Self::Stopped | Self::Ended)
    }
}

impl WatchEvent {
    // The type the record names the event by.
    #[must_use]
    pub(crate) const fn kind(&self) -> &'static str {
        match self {
            Self::Finding { .. } => "finding",
            Self::ReviewDone { .. } => "review_done",
            Self::Conflict { .. } => "conflict",
            Self::CiFailed { .. } => "ci_failed",
        }
    }
}

// Unknown keys in a stored record are skipped, since the TypeScript watch kept them readable;
// a missing or mistyped field makes the record unreadable rather than empty.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredWatchState {
    status: WatchStatus,
    #[serde(deserialize_with = "field::nullable")]
    reason: Option<String>,
    #[serde(deserialize_with = "field::nullable")]
    summary: Option<PrSummary>,
    #[serde(deserialize_with = "field::nullable")]
    checked_at: Option<String>,
    #[serde(deserialize_with = "field::nullable")]
    changed_at: Option<String>,
    seen: Vec<String>,
    pending: Vec<WatchEvent>,
    follow_up: Vec<Finding>,
    #[serde(deserialize_with = "field::nullable")]
    conflict_head: Option<String>,
    #[serde(deserialize_with = "field::nullable")]
    delivered: Option<Delivered>,
    // A state written before a PR followed its chat has none, which the watch takes as the linked
    // one.
    #[serde(default)]
    worker: Option<String>,
    #[serde(deserialize_with = "field::nullable")]
    halted: Option<Map<String, Value>>,
}

// Records written before review bots were configurable name CodeRabbit in their own fields, and
// are read as the coderabbit preset.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredSummary {
    state: String,
    is_draft: bool,
    head: String,
    updated_at: String,
    mergeable: String,
    #[serde(default, deserialize_with = "field::present")]
    bots: Option<BTreeMap<String, Option<BotStatus>>>,
    #[serde(deserialize_with = "field::nullable")]
    rollup: Option<String>,
    #[serde(default)]
    coderabbit: Option<BotStatus>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredFinding {
    id: String,
    kind: ItemKind,
    #[serde(default)]
    bot: Recorded<Option<String>>,
    label: String,
    #[serde(deserialize_with = "field::nullable")]
    path: Option<String>,
    #[serde(deserialize_with = "field::nullable")]
    line: Option<u64>,
    url: String,
    created_at: String,
    // A finding written before approvals were told is never one.
    #[serde(default)]
    approval: Option<Approval>,
    #[serde(default, deserialize_with = "field::present")]
    coderabbit: Option<bool>,
}

#[derive(Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
enum StoredEvent {
    Finding {
        finding: Finding,
    },
    ReviewDone {
        bot: String,
        name: String,
        head: String,
        findings: usize,
        summary_agrees: bool,
    },
    Conflict {
        head: String,
    },
    CiFailed {
        head: String,
        check: FailedCheck,
    },
    CoderabbitDone {
        head: String,
        findings: usize,
        summary_agrees: bool,
    },
}

impl From<StoredWatchState> for WatchState {
    fn from(stored: StoredWatchState) -> Self {
        let mut seen = stored.seen;
        // Its CodeRabbit key is taken as the bot already known, so the watch does not read
        // CodeRabbit as newly added.
        if stored.summary.is_some() && !seen.iter().any(|key| key.starts_with(BOT_KEY)) {
            seen.push(bot_key("coderabbit"));
        }
        Self {
            status: stored.status,
            reason: stored.reason,
            summary: stored.summary,
            checked_at: stored.checked_at,
            changed_at: stored.changed_at,
            seen,
            pending: stored.pending,
            follow_up: stored.follow_up,
            conflict_head: stored.conflict_head,
            delivered: stored.delivered,
            worker: stored.worker,
            halted: stored.halted,
        }
    }
}

impl From<StoredSummary> for PrSummary {
    fn from(stored: StoredSummary) -> Self {
        Self {
            state: stored.state,
            is_draft: stored.is_draft,
            head: stored.head,
            updated_at: stored.updated_at,
            mergeable: stored.mergeable,
            bots: stored
                .bots
                .unwrap_or_else(|| BTreeMap::from([("coderabbit".to_owned(), stored.coderabbit)])),
            rollup: stored.rollup,
        }
    }
}

impl From<StoredFinding> for Finding {
    fn from(stored: StoredFinding) -> Self {
        Self {
            id: stored.id,
            kind: stored.kind,
            bot: match stored.bot {
                Recorded::Present(bot) => bot,
                Recorded::Absent => {
                    (stored.coderabbit == Some(true)).then(|| "coderabbit".to_owned())
                }
            },
            label: stored.label,
            path: stored.path,
            line: stored.line,
            url: stored.url,
            created_at: stored.created_at,
            approval: stored.approval,
        }
    }
}

impl From<StoredEvent> for WatchEvent {
    fn from(stored: StoredEvent) -> Self {
        match stored {
            StoredEvent::Finding { finding } => Self::Finding { finding },
            StoredEvent::ReviewDone {
                bot,
                name,
                head,
                findings,
                summary_agrees,
            } => Self::ReviewDone {
                bot,
                name,
                head,
                findings,
                summary_agrees,
            },
            StoredEvent::Conflict { head } => Self::Conflict { head },
            StoredEvent::CiFailed { head, check } => Self::CiFailed { head, check },
            StoredEvent::CoderabbitDone {
                head,
                findings,
                summary_agrees,
            } => Self::ReviewDone {
                bot: "coderabbit".to_owned(),
                name: "CodeRabbit".to_owned(),
                head,
                findings,
                summary_agrees,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn state_written_before_review_bots_were_configurable_reads_as_coderabbit() {
        let legacy_finding = json!({
            "id": "inline:1",
            "kind": "inline",
            "label": "CodeRabbit・Major",
            "path": "src/pay.kt",
            "line": 42,
            "url": "https://github.com/org/pay/pull/7#discussion_r1",
            "createdAt": "2026-10-23T00:30:00Z",
            "coderabbit": true,
        });
        let record = json!({
            "status": "watching",
            "reason": null,
            "summary": {
                "state": "OPEN",
                "isDraft": false,
                "head": HEAD,
                "updatedAt": "2026-10-23T01:00:00Z",
                "mergeable": "MERGEABLE",
                "rollup": "SUCCESS",
                "coderabbit": coderabbit_status(),
            },
            "checkedAt": null,
            "changedAt": null,
            "seen": ["inline:1"],
            "pending": [
                { "type": "coderabbit_done", "head": HEAD, "findings": 1, "summaryAgrees": true },
                { "type": "finding", "finding": legacy_finding },
            ],
            "followUp": [legacy_finding],
            "conflictHead": null,
            "delivered": {
                "at": "2026-10-23T02:00:00.000Z",
                "sender": "sender",
                "prompt": "go",
                "findings": [legacy_finding],
                "followUp": false,
            },
            "worker": null,
            "halted": null,
        });

        let state = WatchState::deserialize(record).expect("read the record");

        assert_eq!(
            state,
            WatchState {
                summary: Some(summary()),
                seen: vec!["inline:1".to_owned(), bot_key("coderabbit")],
                pending: vec![
                    WatchEvent::ReviewDone {
                        bot: "coderabbit".to_owned(),
                        name: "CodeRabbit".to_owned(),
                        head: HEAD.to_owned(),
                        findings: 1,
                        summary_agrees: true,
                    },
                    WatchEvent::Finding { finding: finding() },
                ],
                follow_up: vec![finding()],
                delivered: Some(Delivered {
                    at: "2026-10-23T02:00:00.000Z".to_owned(),
                    sender: "sender".to_owned(),
                    prompt: "go".to_owned(),
                    findings: vec![finding()],
                    follow_up: false,
                }),
                ..WatchState::default()
            }
        );
    }

    #[test]
    fn legacy_finding_bot_follows_its_own_field_first() {
        for (name, fields, expected) in [
            ("person", json!({}), None),
            (
                "coderabbit_flag",
                json!({ "coderabbit": true }),
                Some("coderabbit"),
            ),
            ("cleared_flag", json!({ "coderabbit": false }), None),
            (
                "bot_field_over_flag",
                json!({ "bot": "other", "coderabbit": true }),
                Some("other"),
            ),
            (
                "null_bot_over_flag",
                json!({ "bot": null, "coderabbit": true }),
                None,
            ),
        ] {
            let mut record = json!({
                "id": "inline:1",
                "kind": "inline",
                "label": "x",
                "path": null,
                "line": null,
                "url": "https://github.com/org/pay/pull/7#discussion_r1",
                "createdAt": "2026-10-23T00:30:00Z",
            });
            if let (Some(record), Some(fields)) = (record.as_object_mut(), fields.as_object()) {
                record.extend(fields.clone());
            }

            let finding = Finding::deserialize(record).expect("read the finding");

            assert_eq!(finding.bot.as_deref(), expected, "case: {name}");
        }
    }

    #[test]
    fn state_without_a_worker_reads_as_one_with_no_worker() {
        let mut record = serde_json::to_value(WatchState::default()).expect("write the state");
        record
            .as_object_mut()
            .expect("the state is an object")
            .remove("worker");

        let state = WatchState::deserialize(record).expect("read the record");

        assert_eq!(state, WatchState::default());
    }

    #[test]
    fn seen_gains_the_coderabbit_key_only_for_a_read_pr_without_any_bot_key() {
        for (name, summary, seen, expected) in [
            ("unread_pr", None, vec!["inline:1"], vec!["inline:1"]),
            (
                "read_pr_without_bot_key",
                Some(summary()),
                vec!["inline:1"],
                vec!["inline:1", "bot:coderabbit"],
            ),
            (
                "read_pr_with_another_bot_key",
                Some(summary()),
                vec!["bot:claude"],
                vec!["bot:claude"],
            ),
        ] {
            let record = serde_json::to_value(WatchState {
                summary,
                seen: seen.into_iter().map(str::to_owned).collect(),
                ..WatchState::default()
            })
            .expect("write the state");

            let state = WatchState::deserialize(record).expect("read the record");

            assert_eq!(state.seen, expected, "case: {name}");
        }
    }

    #[test]
    fn saved_state_reads_back_unchanged_with_unknown_keys_skipped() {
        let state = WatchState {
            status: WatchStatus::Halted,
            reason: Some("the send could not be confirmed".to_owned()),
            summary: Some(summary()),
            checked_at: Some("2026-10-23T02:00:00.000Z".to_owned()),
            changed_at: Some("2026-10-23T01:00:00.000Z".to_owned()),
            seen: vec![bot_key("coderabbit"), format!("coderabbit:{HEAD}")],
            pending: vec![
                WatchEvent::Finding { finding: finding() },
                WatchEvent::Conflict {
                    head: HEAD.to_owned(),
                },
                WatchEvent::CiFailed {
                    head: HEAD.to_owned(),
                    check: FailedCheck {
                        name: "build".to_owned(),
                        url: None,
                    },
                },
            ],
            follow_up: vec![finding()],
            conflict_head: Some(HEAD.to_owned()),
            delivered: None,
            worker: Some("worker".to_owned()),
            halted: json!({ "at": "2026-10-23T02:00:00.000Z", "prompt": "go", "answer": null })
                .as_object()
                .cloned(),
        };
        let mut record = serde_json::to_value(&state).expect("write the state");
        record
            .as_object_mut()
            .expect("the state is an object")
            .insert("addedLater".to_owned(), json!(true));

        let read = WatchState::deserialize(record).expect("read the record");

        assert_eq!(read, state);
    }

    #[test]
    fn state_status_tells_over_and_finished_apart() {
        for (status, over, finished) in [
            (WatchStatus::Watching, false, false),
            (WatchStatus::Draft, false, false),
            (WatchStatus::Ended, true, true),
            (WatchStatus::Stopped, false, true),
            (WatchStatus::Halted, false, true),
        ] {
            assert_eq!(
                (status.is_over(), status.is_finished()),
                (over, finished),
                "case: {status:?}"
            );
        }
    }

    #[test]
    fn only_a_stopped_or_ended_watch_lets_its_chat_go() {
        for (status, holds) in [
            (WatchStatus::Watching, true),
            (WatchStatus::Draft, true),
            (WatchStatus::Halted, true),
            (WatchStatus::Stopped, false),
            (WatchStatus::Ended, false),
        ] {
            assert_eq!(status.holds_chat(), holds, "case: {status:?}");
        }
    }

    #[test]
    fn event_kind_is_the_type_its_record_names() {
        for event in [
            WatchEvent::Finding { finding: finding() },
            WatchEvent::ReviewDone {
                bot: "coderabbit".to_owned(),
                name: "CodeRabbit".to_owned(),
                head: HEAD.to_owned(),
                findings: 1,
                summary_agrees: true,
            },
            WatchEvent::Conflict {
                head: HEAD.to_owned(),
            },
            WatchEvent::CiFailed {
                head: HEAD.to_owned(),
                check: FailedCheck {
                    name: "build".to_owned(),
                    url: None,
                },
            },
        ] {
            let record = serde_json::to_value(&event).expect("write the event");

            assert_eq!(record["type"], event.kind(), "case: {event:?}");
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

    fn coderabbit_status() -> Value {
        json!({
            "state": "SUCCESS",
            "description": "Review completed",
            "createdAt": "2026-10-23T01:00:00Z",
        })
    }

    fn finding() -> Finding {
        Finding {
            id: "inline:1".to_owned(),
            kind: ItemKind::Inline,
            bot: Some("coderabbit".to_owned()),
            label: "CodeRabbit・Major".to_owned(),
            path: Some("src/pay.kt".to_owned()),
            line: Some(42),
            url: "https://github.com/org/pay/pull/7#discussion_r1".to_owned(),
            created_at: "2026-10-23T00:30:00Z".to_owned(),
            approval: None,
        }
    }
}
