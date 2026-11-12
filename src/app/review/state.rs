use std::collections::{BTreeMap, HashSet};

use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

use super::request::pr_key;

// Saved in the layout the Go review wrote, so either version reads the other's state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReviewState {
    version: Version,
    #[serde(with = "blank")]
    pub(crate) login: Option<String>,
    pub(crate) requests: BTreeMap<String, Request>,
    pub(crate) prs: BTreeMap<String, CachedPr>,
    #[serde(with = "poll_time")]
    pub(crate) last_poll: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Request {
    #[serde(flatten)]
    pub(crate) found: Found,
    pub(crate) status: Status,
    #[serde(with = "stamp")]
    pub(crate) first_seen: Timestamp,
    #[serde(with = "stamp")]
    pub(crate) updated_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) choice: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) head_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) worktree: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
}

// The key names the review_requested event, so only a re-request is notified again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Found {
    pub(crate) key: String,
    pub(crate) repo: String,
    pub(crate) number: u64,
    pub(crate) title: String,
    #[serde(with = "blank")]
    pub(crate) requester: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) team: Option<String>,
}

// What the last read of a PR's events found, kept until the PR is updated again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CachedPr {
    pub(crate) updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) found: Option<Found>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Status {
    // Pending when the state was first created; never notified.
    Baseline,
    Queued,
    Notifying,
    Declined,
    Timeout,
    Dismissed,
    // Closed unanswered to make room for a newer request; treated like a timeout.
    Replaced,
    // The request disappeared from GitHub before it was shown.
    Withdrawn,
    Launching,
    Launched,
    Failed,
}

// Only version 1 exists, so any other is refused rather than read as it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Version;

// A late search result must not notify a finished request again, so it outlives the search by this long.
const RETENTION: SignedDuration = SignedDuration::from_hours(90 * 24);

const INTERRUPTED_LAUNCH: &str =
    "interrupted while launching; check the Codex App before starting it again";

impl ReviewState {
    #[must_use]
    pub(crate) fn is_new(&self) -> bool {
        self.last_poll.is_none() && self.requests.is_empty()
    }

    // The keys of the requests to notify, in order; none on a baseline poll.
    pub(crate) fn apply_poll(
        &mut self,
        found: Vec<Found>,
        now: Timestamp,
        baseline: bool,
    ) -> Vec<String> {
        let active = found
            .iter()
            .map(|found| found.key.clone())
            .collect::<HashSet<_>>();
        let active_prs = found
            .iter()
            .map(|found| pr_key(&found.repo, found.number))
            .collect::<HashSet<_>>();

        let mut added = Vec::new();
        for found in found {
            if let Some(request) = self.requests.get_mut(&found.key) {
                request.found.title = found.title;
                // Search can briefly miss a PR, so a request it lists again is still owed its notification.
                if request.status == Status::Withdrawn {
                    request.status = Status::Queued;
                    request.updated_at = now;
                }
                continue;
            }
            if !baseline {
                added.push(found.key.clone());
            }
            self.requests.insert(
                found.key.clone(),
                Request {
                    found,
                    status: if baseline {
                        Status::Baseline
                    } else {
                        Status::Queued
                    },
                    first_seen: now,
                    updated_at: now,
                    choice: None,
                    head_sha: None,
                    worktree: None,
                    error: None,
                },
            );
        }

        self.requests.retain(|key, request| {
            if active.contains(key) {
                return true;
            }
            if request.status == Status::Queued {
                request.status = Status::Withdrawn;
                request.updated_at = now;
                return true;
            }
            active_prs.contains(&pr_key(&request.found.repo, request.found.number))
                || now.duration_since(request.updated_at) <= RETENTION
        });

        added.sort();
        added
    }

    // The oldest queued request, so requests are notified in the order they came.
    #[must_use]
    pub(crate) fn next_queued(&self) -> Option<&str> {
        self.requests
            .values()
            .filter(|request| request.status == Status::Queued)
            .min_by(|left, right| {
                (left.first_seen, &left.found.key).cmp(&(right.first_seen, &right.found.key))
            })
            .map(|request| request.found.key.as_str())
    }

    #[must_use]
    pub(crate) fn queued_count(&self) -> usize {
        self.requests
            .values()
            .filter(|request| request.status == Status::Queued)
            .count()
    }

    // Retries interrupted notifications, but never a launch that may already have opened a chat.
    pub(crate) fn recover(&mut self, now: Timestamp) {
        for request in self.requests.values_mut() {
            match request.status {
                Status::Notifying => {
                    request.status = Status::Queued;
                    request.updated_at = now;
                }
                Status::Launching => {
                    request.status = Status::Failed;
                    request.updated_at = now;
                    request.error = Some(INTERRUPTED_LAUNCH.to_owned());
                }
                _ => {}
            }
        }
    }
}

impl Status {
    #[must_use]
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Baseline => "baseline",
            Self::Queued => "queued",
            Self::Notifying => "notifying",
            Self::Declined => "declined",
            Self::Timeout => "timeout",
            Self::Dismissed => "dismissed",
            Self::Replaced => "replaced",
            Self::Withdrawn => "withdrawn",
            Self::Launching => "launching",
            Self::Launched => "launched",
            Self::Failed => "failed",
        }
    }
}

impl Serialize for Version {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u8(1)
    }
}

impl<'de> Deserialize<'de> for Version {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match u64::deserialize(deserializer)? {
            1 => Ok(Self),
            other => Err(D::Error::custom(format!("unsupported version {other}"))),
        }
    }
}

// Read in any offset, as the Go review wrote its local one, and written in UTC.
mod stamp {
    use jiff::Timestamp;
    use serde::{Deserialize, Deserializer, Serializer, de::Error as _};

    use crate::app::time;

    pub(super) fn serialize<S: Serializer>(
        at: &Timestamp,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&time::format_timestamp(*at))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Timestamp, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(D::Error::custom)
    }
}

// Go wrote a time never set as its zero time, which the state keeps as `None`.
mod poll_time {
    use jiff::Timestamp;
    use serde::{Deserializer, Serializer};

    const GO_ZERO: &str = "0001-01-01T00:00:00Z";

    #[expect(clippy::ref_option, reason = "serde passes the field by reference")]
    pub(super) fn serialize<S: Serializer>(
        at: &Option<Timestamp>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match at {
            Some(at) => super::stamp::serialize(at, serializer),
            None => serializer.serialize_str(GO_ZERO),
        }
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Timestamp>, D::Error> {
        let at = super::stamp::deserialize(deserializer)?;
        let zero = GO_ZERO.parse::<Timestamp>().ok();
        Ok((Some(at) != zero).then_some(at))
    }
}

// Go wrote an unknown login or requester as an empty string, which the state keeps as `None`.
mod blank {
    use serde::{Deserialize, Deserializer, Serializer};

    #[expect(clippy::ref_option, reason = "serde passes the field by reference")]
    pub(super) fn serialize<S: Serializer>(
        text: &Option<String>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(text.as_deref().unwrap_or_default())
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<String>, D::Error> {
        let text = String::deserialize(deserializer)?;
        Ok((!text.is_empty()).then_some(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: &str = "2026-11-12T03:00:00Z";

    #[test]
    fn search_that_missed_a_pr_once_leaves_its_request_queued() {
        let mut state = ReviewState::default();
        let now = at(NOW);

        state.apply_poll(vec![found("acme/app", 1, 10)], now, false);
        state.apply_poll(Vec::new(), now, false);
        let missed = status_of(&state, "acme/app#1@10");
        state.apply_poll(vec![found("acme/app", 1, 10)], now, false);

        assert_eq!(missed, Status::Withdrawn);
        assert_eq!(status_of(&state, "acme/app#1@10"), Status::Queued);
    }

    #[test]
    fn baseline_poll_records_pending_requests_without_notifying_them() {
        let mut state = ReviewState::default();
        let was_new = state.is_new();

        let added = state.apply_poll(vec![found("acme/app", 1, 10)], at(NOW), true);

        assert!(was_new);
        assert!(added.is_empty());
        assert_eq!(status_of(&state, "acme/app#1@10"), Status::Baseline);
    }

    #[test]
    fn new_requests_are_returned_in_key_order_and_the_title_follows_github() {
        let mut state = ReviewState::default();
        state.apply_poll(vec![found("acme/app", 1, 10)], at(NOW), false);
        let mut renamed = found("acme/app", 1, 10);
        renamed.title = "Renamed".to_owned();

        let added = state.apply_poll(
            vec![found("acme/app", 3, 30), renamed, found("acme/app", 2, 20)],
            at(NOW),
            false,
        );

        assert_eq!(added, ["acme/app#2@20", "acme/app#3@30"]);
        assert_eq!(state.requests["acme/app#1@10"].found.title, "Renamed");
    }

    #[test]
    fn finished_request_is_dropped_only_after_retention_once_its_pr_is_not_pending() {
        for (name, pending, age_days, kept) in [
            ("pending_pr_kept", true, 120, true),
            ("recent_kept", false, 89, true),
            ("old_dropped", false, 91, false),
        ] {
            let mut state = ReviewState::default();
            let then = at(NOW);
            state.apply_poll(vec![found("acme/app", 1, 10)], then, false);
            state
                .requests
                .get_mut("acme/app#1@10")
                .expect("request")
                .status = Status::Declined;
            let later = then
                .checked_add(SignedDuration::from_hours(age_days * 24))
                .expect("a later time");
            let rerequested = if pending {
                vec![found("Acme/App", 1, 11)]
            } else {
                Vec::new()
            };

            state.apply_poll(rerequested, later, false);

            assert_eq!(
                state.requests.contains_key("acme/app#1@10"),
                kept,
                "case: {name}"
            );
        }
    }

    #[test]
    fn next_queued_is_the_first_seen_then_the_lowest_key() {
        let mut state = ReviewState::default();
        state.apply_poll(
            vec![found("acme/app", 5, 50)],
            at("2026-11-12T03:00:01Z"),
            false,
        );
        state.apply_poll(
            vec![
                found("acme/app", 5, 50),
                found("acme/app", 3, 30),
                found("acme/app", 2, 20),
            ],
            at("2026-11-12T03:00:02Z"),
            false,
        );
        state
            .requests
            .get_mut("acme/app#5@50")
            .expect("request")
            .status = Status::Declined;

        let next = state.next_queued();

        assert_eq!(next, Some("acme/app#2@20"));
        assert_eq!(state.queued_count(), 2);
    }

    #[test]
    fn recover_requeues_a_notification_but_never_repeats_a_launch() {
        let mut state = ReviewState::default();
        state.apply_poll(
            vec![found("acme/app", 1, 10), found("acme/app", 2, 20)],
            at(NOW),
            false,
        );
        state
            .requests
            .get_mut("acme/app#1@10")
            .expect("request")
            .status = Status::Notifying;
        state
            .requests
            .get_mut("acme/app#2@20")
            .expect("request")
            .status = Status::Launching;

        state.recover(at("2026-11-12T03:05:00Z"));

        assert_eq!(status_of(&state, "acme/app#1@10"), Status::Queued);
        let launch = &state.requests["acme/app#2@20"];
        assert_eq!(launch.status, Status::Failed);
        assert_eq!(launch.error.as_deref(), Some(INTERRUPTED_LAUNCH));
        assert_eq!(launch.updated_at, at("2026-11-12T03:05:00Z"));
    }

    mod record {
        use super::*;

        // As the Go review wrote it: nanoseconds with the local offset, an empty requester and no team.
        const GO_STATE: &str = r#"{
  "version": 1,
  "login": "me",
  "requests": {
    "acme/app#1@10": {
      "key": "acme/app#1@10",
      "repo": "acme/app",
      "number": 1,
      "title": "T",
      "requester": "",
      "status": "launched",
      "firstSeen": "2026-11-12T12:00:00.123456789+09:00",
      "updatedAt": "2026-11-12T12:01:00+09:00",
      "choice": "code",
      "headSha": "0123456789abcdef0123456789abcdef01234567",
      "worktree": "/w/acme/app/pr-1-0123456789ab",
      "unknownField": true
    }
  },
  "prs": {
    "acme/app#1": {
      "updatedAt": "2026-11-12T03:00:00Z",
      "found": {
        "key": "acme/app#1@10",
        "repo": "acme/app",
        "number": 1,
        "title": "T",
        "requester": "tanaka",
        "team": "acme/backend"
      }
    }
  },
  "lastPoll": "0001-01-01T00:00:00Z"
}
"#;

        #[test]
        fn go_state_is_read_with_unset_values_as_none() {
            let state = serde_json::from_str::<ReviewState>(GO_STATE).expect("read the state");

            let request = &state.requests["acme/app#1@10"];
            assert_eq!(state.login.as_deref(), Some("me"));
            assert_eq!(state.last_poll, None);
            assert_eq!(request.found.requester, None);
            assert_eq!(request.status, Status::Launched);
            assert_eq!(request.first_seen, at("2026-11-12T03:00:00.123456789Z"));
            assert_eq!(request.error, None);
            let cached = state.prs["acme/app#1"].found.as_ref().expect("cached");
            assert_eq!(cached.team.as_deref(), Some("acme/backend"));
        }

        #[test]
        fn state_is_written_with_go_keys_and_millisecond_utc_times() {
            let mut state = ReviewState::default();
            state.apply_poll(vec![found("acme/app", 1, 10)], at(NOW), false);

            let written = serde_json::to_value(&state).expect("write the state");

            assert_eq!(
                written,
                serde_json::json!({
                    "version": 1,
                    "login": "",
                    "requests": {
                        "acme/app#1@10": {
                            "key": "acme/app#1@10",
                            "repo": "acme/app",
                            "number": 1,
                            "title": "T",
                            "requester": "",
                            "status": "queued",
                            "firstSeen": "2026-11-12T03:00:00.000Z",
                            "updatedAt": "2026-11-12T03:00:00.000Z"
                        }
                    },
                    "prs": {},
                    "lastPoll": "0001-01-01T00:00:00Z"
                })
            );
        }

        #[test]
        fn written_state_reads_back_the_same() {
            let mut state = serde_json::from_str::<ReviewState>(GO_STATE).expect("read the state");
            state.last_poll = Some(at(NOW));
            state.last_error = Some("HTTP 403".to_owned());
            // Written to the millisecond, as the TypeScript watch writes its times.
            state
                .requests
                .get_mut("acme/app#1@10")
                .expect("request")
                .first_seen = at("2026-11-12T03:00:00.123Z");

            let again = serde_json::from_str::<ReviewState>(
                &serde_json::to_string(&state).expect("write the state"),
            )
            .expect("read it back");

            assert_eq!(again, state);
        }

        #[test]
        fn state_that_go_would_not_have_written_is_refused() {
            for (name, content) in [
                (
                    "other_version",
                    GO_STATE.replace(r#""version": 1"#, r#""version": 2"#),
                ),
                (
                    "unknown_status",
                    GO_STATE.replace(r#""status": "launched""#, r#""status": "opened""#),
                ),
                (
                    "unreadable_time",
                    GO_STATE.replace("2026-11-12T12:01:00+09:00", "noon"),
                ),
                (
                    "missing_requests",
                    r#"{"version":1,"login":"","prs":{},"lastPoll":"0001-01-01T00:00:00Z"}"#
                        .to_owned(),
                ),
            ] {
                let read = serde_json::from_str::<ReviewState>(&content);

                assert!(read.is_err(), "case: {name}");
            }
        }
    }

    fn found(repo: &str, number: u64, event: u64) -> Found {
        Found {
            key: format!("{repo}#{number}@{event}"),
            repo: repo.to_owned(),
            number,
            title: "T".to_owned(),
            requester: None,
            team: None,
        }
    }

    fn status_of(state: &ReviewState, key: &str) -> Status {
        state.requests[key].status
    }

    fn at(text: &str) -> Timestamp {
        text.parse().expect("parse the time")
    }
}
