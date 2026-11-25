use std::collections::BTreeMap;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::{field, pr::ItemKind};
use crate::app::time;

// A missing or mistyped field makes the record unreadable rather than empty.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WatchState {
    pub(crate) status: WatchStatus,
    #[serde(deserialize_with = "field::nullable")]
    pub(crate) reason: Option<String>,
    #[serde(deserialize_with = "field::nullable")]
    pub(crate) summary: Option<PrSummary>,
    #[serde(with = "time::optional_record")]
    pub(crate) checked_at: Option<Timestamp>,
    #[serde(with = "time::optional_record")]
    pub(crate) changed_at: Option<Timestamp>,
    // Keys of every event already queued or sent, so none is sent twice.
    pub(crate) seen: Vec<String>,
    pub(crate) pending: Vec<WatchEvent>,
    pub(crate) follow_up: Vec<Finding>,
    #[serde(deserialize_with = "field::nullable")]
    pub(crate) conflict_head: Option<String>,
    #[serde(deserialize_with = "field::nullable")]
    pub(crate) delivered: Option<Delivered>,
    // The chat the delivery fields belong to; a link naming another chat means the PR was handed over.
    #[serde(deserialize_with = "field::nullable")]
    pub(crate) worker: Option<String>,
    // The message whose delivery is unconfirmed; nothing more is sent for the PR until it is released.
    #[serde(deserialize_with = "field::nullable")]
    pub(crate) halted: Option<Map<String, Value>>,
    // When the worker began missing messages in a row.
    #[serde(default, with = "time::optional_record")]
    pub(crate) lost_since: Option<Timestamp>,
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
#[serde(rename_all = "camelCase")]
pub(crate) struct PrSummary {
    pub(crate) state: String,
    pub(crate) is_draft: bool,
    pub(crate) head: String,
    pub(crate) updated_at: String,
    pub(crate) mergeable: String,
    // The progress of each review bot that keeps one, by the bot's id.
    pub(crate) bots: BTreeMap<String, Option<BotStatus>>,
    #[serde(deserialize_with = "field::nullable")]
    pub(crate) rollup: Option<String>,
}

// A check run reads as a commit status: PENDING until it completes, then its conclusion, with no description.
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

// A comment body must never steer what the worker is told, so only its kind, author and place are kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Finding {
    pub(crate) id: String,
    pub(crate) kind: ItemKind,
    // The id of the review bot that wrote it, or `None` for a person.
    #[serde(deserialize_with = "field::nullable")]
    pub(crate) bot: Option<String>,
    pub(crate) label: String,
    #[serde(deserialize_with = "field::nullable")]
    pub(crate) path: Option<String>,
    #[serde(deserialize_with = "field::nullable")]
    pub(crate) line: Option<u64>,
    pub(crate) url: String,
    pub(crate) created_at: String,
    // A person's approval, which is told even without a body.
    #[serde(deserialize_with = "field::nullable")]
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
    rename_all_fields = "camelCase"
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
    #[serde(with = "time::record")]
    pub(crate) at: Timestamp,
    pub(crate) sender: String,
    pub(crate) prompt: String,
    pub(crate) findings: Vec<Finding>,
    pub(crate) follow_up: bool,
    // An older record has no events, so only its findings are told again.
    #[serde(default)]
    pub(crate) events: Vec<WatchEvent>,
    // A loss is logged and notified once, so the message records that it was found lost.
    #[serde(default)]
    pub(crate) lost: bool,
}

// Marks a bot whose reviews the watch has taken in.
pub(crate) fn bot_key(id: &str) -> String {
    format!("bot:{id}")
}

impl WatchStatus {
    // No command brings an ended PR back, so a watch may stop reading it for as long as it runs.
    // Naming a stopped one from another worker resumes it, so it stays readable.
    #[must_use]
    pub(crate) const fn is_over(self) -> bool {
        matches!(self, Self::Ended)
    }

    // Nothing is read or sent for these; a release or another chat's link resumes a halted or stopped one.
    #[must_use]
    pub(crate) const fn is_finished(self) -> bool {
        matches!(self, Self::Ended | Self::Stopped | Self::Halted)
    }

    // Only a watch stopped for a gone chat has let its chat go; a halted one may have just reached it.
    #[must_use]
    pub(crate) const fn holds_chat(self) -> bool {
        !matches!(self, Self::Stopped | Self::Ended)
    }

    // As the records and `status --json` write it.
    #[must_use]
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Watching => "watching",
            Self::Draft => "draft",
            Self::Ended => "ended",
            Self::Stopped => "stopped",
            Self::Halted => "halted",
        }
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn saved_state_reads_back_unchanged_with_unknown_keys_skipped() {
        let state = WatchState {
            status: WatchStatus::Halted,
            reason: Some("the send could not be confirmed".to_owned()),
            summary: Some(summary()),
            checked_at: Some(at("2026-10-23T02:00:00Z")),
            changed_at: Some(at("2026-10-23T01:00:00Z")),
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
            delivered: Some(Delivered {
                at: at("2026-10-23T01:45:00Z"),
                sender: "sender".to_owned(),
                prompt: "go".to_owned(),
                findings: vec![finding()],
                follow_up: false,
                events: vec![WatchEvent::Finding { finding: finding() }],
                lost: true,
            }),
            worker: Some("worker".to_owned()),
            halted: json!({ "at": "2026-10-23T02:00:00.000Z", "prompt": "go", "answer": null })
                .as_object()
                .cloned(),
            lost_since: Some(at("2026-10-23T01:30:00Z")),
        };
        let mut record = serde_json::to_value(&state).expect("write the state");
        record
            .as_object_mut()
            .expect("the state is an object")
            .insert("addedLater".to_owned(), json!(true));

        let read = WatchState::deserialize(record.clone()).expect("read the record");

        assert_eq!(read, state);
        assert_eq!(
            [
                &record["checkedAt"],
                &record["delivered"]["at"],
                &record["lostSince"]
            ],
            [
                "2026-10-23T02:00:00.000Z",
                "2026-10-23T01:45:00.000Z",
                "2026-10-23T01:30:00.000Z"
            ]
        );
    }

    #[test]
    fn state_status_tells_over_finished_and_holding_the_chat_apart() {
        for (status, over, finished, holds_chat) in [
            (WatchStatus::Watching, false, false, true),
            (WatchStatus::Draft, false, false, true),
            (WatchStatus::Ended, true, true, false),
            (WatchStatus::Stopped, false, true, false),
            (WatchStatus::Halted, false, true, true),
        ] {
            assert_eq!(
                (status.is_over(), status.is_finished(), status.holds_chat()),
                (over, finished, holds_chat),
                "case: {status:?}"
            );
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

    fn at(time: &str) -> Timestamp {
        time.parse().expect("parse the time")
    }
}
