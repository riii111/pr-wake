use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{
    batch::ReportReason,
    one_line,
    state::{Request, ReviewState, Status},
};

// The requests the owner chose from the queue, handed to the running review under an id the caller waits on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Selected {
    pub(crate) id: String,
    pub(crate) keys: Vec<String>,
}

// What the running review has done with a selection so far; a key without an outcome is not handled yet.
// The caller writes it before the handoff, so a reader finds it whether or not the review has taken the selection.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SelectionRecord {
    pub(crate) keys: Vec<String>,
    #[serde(default)]
    pub(crate) accepted: bool,
    pub(crate) outcomes: BTreeMap<String, SelectedOutcome>,
    pub(crate) complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "outcome",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub(crate) enum SelectedOutcome {
    Started {
        thread: String,
    },
    // A re-request sent to the chat linked to the PR.
    SentToLinked {
        thread: String,
    },
    // No chat could be started, so the prompt waits in a new chat's composer, whose id is unknown.
    Opened {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    Reported {
        reason: ReportReason,
    },
    // Withdrawn, handled or superseded since it was listed, so nothing was started for it.
    NotQueued {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<Status>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        newer: Option<String>,
    },
    Failed {
        error: String,
        // The chat the review went to, when it is known despite the failure.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thread: Option<String>,
    },
    // The review stopped before reaching it, so it stays queued.
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct QueueEntry {
    pub(crate) key: String,
    pub(crate) repo: String,
    pub(crate) number: u64,
    pub(crate) title: String,
    pub(crate) url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) requester: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SelectionReport {
    pub(crate) selection: String,
    // Whether the running review has taken the selection at all.
    pub(crate) accepted: bool,
    pub(crate) complete: bool,
    pub(crate) requests: Vec<ReportedRequest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReportedRequest {
    pub(crate) key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) number: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) url: Option<String>,
    #[serde(flatten)]
    pub(crate) outcome: ReportedOutcome,
    // Only from a chat id the review confirmed, so a composer-only fallback never gets one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) link: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub(crate) enum ReportedOutcome {
    Known(SelectedOutcome),
    Pending { outcome: Pending },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Pending {
    Pending,
}

// The id names a file, so it is kept to the characters the caller generates.
#[must_use]
pub(crate) fn is_selection_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
}

#[must_use]
pub(crate) fn queue(state: &ReviewState) -> Vec<QueueEntry> {
    // A superseded request would only be refused when chosen, so the queue shows what can still start.
    state
        .queued_in_order()
        .iter()
        .filter(|request| state.newer_request(&request.found.key).is_none())
        .map(|request| QueueEntry {
            key: request.found.key.clone(),
            repo: request.found.repo.clone(),
            number: request.found.number,
            title: request.found.title.clone(),
            url: pr_url(&request.found.repo, request.found.number),
            requester: request.found.requester.clone(),
        })
        .collect()
}

#[must_use]
pub(crate) fn queue_lines(entries: &[QueueEntry]) -> String {
    if entries.is_empty() {
        return "no queued requests\n".to_owned();
    }
    let mut lines = entries
        .iter()
        .map(|entry| {
            format!(
                "{}#{} {}  {}",
                entry.repo,
                entry.number,
                one_line(&entry.title),
                entry.url
            )
        })
        .collect::<Vec<_>>();
    lines.push(String::new());
    lines.join("\n")
}

#[must_use]
pub(crate) fn report(id: &str, record: &SelectionRecord, state: &ReviewState) -> SelectionReport {
    let requests = record
        .keys
        .iter()
        .map(|key| {
            let request = state.requests.get(key);
            let outcome = record.outcomes.get(key).cloned().map_or(
                ReportedOutcome::Pending {
                    outcome: Pending::Pending,
                },
                ReportedOutcome::Known,
            );
            reported(key, request, outcome)
        })
        .collect();
    SelectionReport {
        selection: id.to_owned(),
        accepted: record.accepted,
        complete: record.complete,
        requests,
    }
}

impl SelectionRecord {
    #[must_use]
    pub(crate) fn of(keys: &[String]) -> Self {
        let mut unique = Vec::new();
        for key in keys {
            if !unique.contains(key) {
                unique.push(key.clone());
            }
        }
        Self {
            keys: unique,
            ..Self::default()
        }
    }
}

impl SelectedOutcome {
    fn thread(&self) -> Option<&str> {
        match self {
            Self::Started { thread } | Self::SentToLinked { thread } => Some(thread),
            Self::Failed { thread, .. } => thread.as_deref(),
            Self::Opened { .. }
            | Self::Reported { .. }
            | Self::NotQueued { .. }
            | Self::Stopped => None,
        }
    }
}

fn reported(key: &str, request: Option<&Request>, outcome: ReportedOutcome) -> ReportedRequest {
    let link = match &outcome {
        ReportedOutcome::Known(outcome) => outcome
            .thread()
            .map(|thread| format!("codex://threads/{thread}")),
        ReportedOutcome::Pending { .. } => None,
    };
    ReportedRequest {
        key: key.to_owned(),
        repo: request.map(|request| request.found.repo.clone()),
        number: request.map(|request| request.found.number),
        title: request.map(|request| request.found.title.clone()),
        url: request.map(|request| pr_url(&request.found.repo, request.found.number)),
        outcome,
        link,
    }
}

fn pr_url(repo: &str, number: u64) -> String {
    format!("https://github.com/{repo}/pull/{number}")
}

#[cfg(test)]
mod tests {
    use jiff::Timestamp;
    use serde_json::json;

    use super::*;
    use crate::app::review::state::Found;

    #[test]
    fn queue_lists_only_queued_requests_in_order_with_their_links() {
        let mut state = ReviewState::default();
        state.apply_poll(
            vec![found(1, 10), found(2, 20), found(3, 30)],
            Timestamp::UNIX_EPOCH,
            false,
        );
        state
            .requests
            .get_mut("acme/app#2@20")
            .expect("request")
            .status = Status::Launched;

        let entries = queue(&state);

        assert_eq!(
            serde_json::to_value(&entries).expect("write the queue"),
            json!([
                {
                    "key": "acme/app#1@10",
                    "repo": "acme/app",
                    "number": 1,
                    "title": "PR 1",
                    "url": "https://github.com/acme/app/pull/1",
                    "requester": "tanaka",
                },
                {
                    "key": "acme/app#3@30",
                    "repo": "acme/app",
                    "number": 3,
                    "title": "PR 3",
                    "url": "https://github.com/acme/app/pull/3",
                    "requester": "tanaka",
                },
            ])
        );
        assert_eq!(
            queue_lines(&entries),
            "acme/app#1 PR 1  https://github.com/acme/app/pull/1\nacme/app#3 PR 3  https://github.com/acme/app/pull/3\n"
        );
        assert_eq!(queue_lines(&[]), "no queued requests\n");
    }

    #[test]
    fn report_links_only_confirmed_chats_and_leaves_the_rest_pending() {
        let mut state = ReviewState::default();
        state.apply_poll(
            vec![found(1, 10), found(2, 20), found(3, 30)],
            Timestamp::UNIX_EPOCH,
            false,
        );
        let selected = [
            "acme/app#1@10",
            "acme/app#2@20",
            "acme/app#3@30",
            "gone#9@9",
        ]
        .map(str::to_owned)
        .to_vec();
        let mut record = SelectionRecord::of(&selected);
        record.accepted = true;
        record.outcomes.insert(
            "acme/app#1@10".to_owned(),
            SelectedOutcome::Started {
                thread: "new-chat".to_owned(),
            },
        );
        record.outcomes.insert(
            "acme/app#2@20".to_owned(),
            SelectedOutcome::Opened {
                reason: Some("no harnexus".to_owned()),
            },
        );
        record.outcomes.insert(
            "gone#9@9".to_owned(),
            SelectedOutcome::NotQueued {
                status: None,
                newer: None,
            },
        );

        let written = report("7-1", &record, &state);

        assert_eq!(
            serde_json::to_value(&written).expect("write the report"),
            json!({
                "selection": "7-1",
                "accepted": true,
                "complete": false,
                "requests": [
                    {
                        "key": "acme/app#1@10",
                        "repo": "acme/app",
                        "number": 1,
                        "title": "PR 1",
                        "url": "https://github.com/acme/app/pull/1",
                        "outcome": "started",
                        "thread": "new-chat",
                        "link": "codex://threads/new-chat",
                    },
                    {
                        "key": "acme/app#2@20",
                        "repo": "acme/app",
                        "number": 2,
                        "title": "PR 2",
                        "url": "https://github.com/acme/app/pull/2",
                        "outcome": "opened",
                        "reason": "no harnexus",
                    },
                    {
                        "key": "acme/app#3@30",
                        "repo": "acme/app",
                        "number": 3,
                        "title": "PR 3",
                        "url": "https://github.com/acme/app/pull/3",
                        "outcome": "pending",
                    },
                    {"key": "gone#9@9", "outcome": "notQueued"},
                ],
            })
        );
    }

    #[test]
    fn report_of_a_selection_not_taken_yet_is_all_pending() {
        let record = SelectionRecord::of(&["acme/app#1@10".to_owned()]);

        let written = report("7-1", &record, &ReviewState::default());

        assert!(!written.accepted);
        assert!(!written.complete);
        assert_eq!(
            written.requests[0].outcome,
            ReportedOutcome::Pending {
                outcome: Pending::Pending
            }
        );
    }

    #[test]
    fn selection_id_is_only_letters_digits_and_hyphens() {
        for (name, id, expected) in [
            ("generated", "1763000000000000000-4242", true),
            ("empty", "", false),
            ("path", "../state", false),
            ("dot", "a.json", false),
        ] {
            assert_eq!(is_selection_id(id), expected, "case: {name}");
        }
    }

    fn found(number: u64, event: u64) -> Found {
        Found {
            key: format!("acme/app#{number}@{event}"),
            repo: "acme/app".to_owned(),
            number,
            title: format!("PR {number}"),
            requester: Some("tanaka".to_owned()),
            team: None,
        }
    }
}
