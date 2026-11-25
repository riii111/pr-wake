use std::collections::{BTreeMap, HashSet};

use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

use super::{batch::Digest, request::pr_key};
use crate::app::time;

// Existing state files must keep loading, so the layout keeps their version and fields.
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
    // The start of the latest scheduled batch, so a restart in the same slot does not run it again.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "optional_stamp"
    )]
    pub(crate) last_batch: Option<Timestamp>,
    // Kept until shown, so a restart before a batch's time still shows its notification once.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) pending_digests: Vec<PendingDigest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PendingDigest {
    #[serde(with = "time::record")]
    pub(crate) ready: Timestamp,
    pub(crate) digest: Digest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Request {
    #[serde(flatten)]
    pub(crate) found: Found,
    pub(crate) status: Status,
    #[serde(with = "time::record")]
    pub(crate) first_seen: Timestamp,
    #[serde(with = "time::record")]
    pub(crate) updated_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) choice: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) head_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) worktree: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
    // Whether a chat confirmed the review started or the linked chat took it, so a draft left in a composer proves nothing.
    #[serde(default, skip_serializing_if = "<&bool as std::ops::Not>::not")]
    pub(crate) review_started: bool,
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
    Reported,
}

// Only version 1 exists, so any other is refused rather than read as it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Version;

// A late search result must not notify a finished request again, so it outlives the search by this long.
const RETENTION: SignedDuration = SignedDuration::from_hours(90 * 24);
// Notifications of a PR reviewed weeks ago are rare, so only recent PRs are tracked.
const TRACKED: SignedDuration = SignedDuration::from_hours(30 * 24);

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
                    review_started: false,
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

    #[must_use]
    pub(crate) fn next_queued(&self) -> Option<&str> {
        self.requests
            .values()
            .filter(|request| request.status == Status::Queued)
            .min_by(|left, right| left.order().cmp(&right.order()))
            .map(|request| request.found.key.as_str())
    }

    #[must_use]
    pub(crate) fn queued_in_order(&self) -> Vec<Request> {
        let mut queued = self
            .requests
            .values()
            .filter(|request| request.status == Status::Queued)
            .cloned()
            .collect::<Vec<_>>();
        queued.sort_by(|left, right| left.order().cmp(&right.order()));
        queued
    }

    #[must_use]
    pub(crate) fn queued_count(&self) -> usize {
        self.requests
            .values()
            .filter(|request| request.status == Status::Queued)
            .count()
    }

    #[must_use]
    pub(crate) fn startable(&self, repo: &str, number: u64) -> Option<&Request> {
        let pr = pr_key(repo, number);
        self.requests
            .values()
            .filter(|request| {
                matches!(request.status, Status::Queued | Status::Reported)
                    && pr_key(&request.found.repo, request.found.number) == pr
            })
            .max_by(|left, right| left.order().cmp(&right.order()))
    }

    // A later request of the same PR, which a review of this one would duplicate.
    #[must_use]
    pub(crate) fn newer_request(&self, key: &str) -> Option<&str> {
        let request = self.requests.get(key)?;
        let pr = pr_key(&request.found.repo, request.found.number);
        self.requests
            .values()
            // Only a queued one is still owed a review, so a withdrawn or already handled newer event never hides an older one GitHub names again.
            .filter(|other| {
                other.status == Status::Queued
                    && pr_key(&other.found.repo, other.found.number) == pr
                    && other.order() > request.order()
            })
            .max_by(|left, right| left.order().cmp(&right.order()))
            .map(|other| other.found.key.as_str())
    }

    // Whether a strictly newer request of the PR began its review at `head`, which covers what this request asks for.
    #[must_use]
    pub(crate) fn reviewed_at(&self, key: &str, head: &str) -> bool {
        // Newer means a larger GitHub event id, as an older event can be first seen late; an unknown event has no order and is never suppressed.
        let Some((request, event)) = self
            .requests
            .get(key)
            .and_then(|request| Some((request, event_id(&request.found.key)?)))
        else {
            return false;
        };
        let pr = pr_key(&request.found.repo, request.found.number);
        self.requests.values().any(|other| {
            other.review_started
                && other.head_sha.as_deref() == Some(head)
                && pr_key(&other.found.repo, other.found.number) == pr
                && event_id(&other.found.key).is_some_and(|other| other > event)
        })
    }

    #[must_use]
    pub(crate) fn tracked_prs(&self, now: Timestamp) -> HashSet<String> {
        self.requests
            .values()
            .filter(|request| {
                request.status != Status::Baseline
                    && now.duration_since(request.updated_at) <= TRACKED
            })
            .map(|request| pr_key(&request.found.repo, request.found.number))
            .collect()
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

impl Request {
    // Requests are taken in the order they were first seen, by key among those seen at once.
    fn order(&self) -> (Timestamp, &str) {
        (self.first_seen, &self.found.key)
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
            Self::Reported => "reported",
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

// The review_requested event a key names, which an unknown request lacks.
fn event_id(key: &str) -> Option<u64> {
    key.rsplit_once('@')?.1.parse().ok()
}

mod optional_stamp {
    use jiff::Timestamp;
    use serde::{Deserializer, Serializer};

    use crate::app::time;

    #[expect(clippy::ref_option, reason = "serde passes the field by reference")]
    pub(super) fn serialize<S: Serializer>(
        at: &Option<Timestamp>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match at {
            Some(at) => time::record::serialize(at, serializer),
            None => serializer.serialize_none(),
        }
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Timestamp>, D::Error> {
        time::record::deserialize(deserializer).map(Some)
    }
}

// Existing state files write a time never set as the zero time, so it stands for `None` both ways.
mod poll_time {
    use jiff::Timestamp;
    use serde::{Deserializer, Serializer};

    use crate::app::time;

    const ZERO_TIME: &str = "0001-01-01T00:00:00Z";

    #[expect(clippy::ref_option, reason = "serde passes the field by reference")]
    pub(super) fn serialize<S: Serializer>(
        at: &Option<Timestamp>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match at {
            Some(at) => time::record::serialize(at, serializer),
            None => serializer.serialize_str(ZERO_TIME),
        }
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Timestamp>, D::Error> {
        let at = time::record::deserialize(deserializer)?;
        let zero = ZERO_TIME.parse::<Timestamp>().ok();
        Ok((Some(at) != zero).then_some(at))
    }
}

// Existing state files write an unknown login or requester as an empty string, so it stands for `None` both ways.
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
    const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";
    const LATER: &str = "fedcba9876543210fedcba9876543210fedcba98";

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
        assert_eq!(added, Vec::<String>::new());
        assert_eq!(status_of(&state, "acme/app#1@10"), Status::Baseline);
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

    #[test]
    fn startable_is_the_latest_queued_or_reported_request_of_the_pr() {
        for (name, statuses, expected) in [
            (
                "latest_reported",
                [Status::Queued, Status::Reported],
                Some("acme/app#1@11"),
            ),
            (
                "earlier_queued",
                [Status::Queued, Status::Launched],
                Some("acme/app#1@10"),
            ),
            ("none_left", [Status::Launched, Status::Declined], None),
        ] {
            let mut state = ReviewState::default();
            state.apply_poll(vec![found("acme/app", 1, 10)], at(NOW), false);
            state.apply_poll(
                vec![found("acme/app", 1, 10), found("acme/app", 1, 11)],
                at("2026-11-12T04:00:00Z"),
                false,
            );
            for (key, status) in ["acme/app#1@10", "acme/app#1@11"].into_iter().zip(statuses) {
                state.requests.get_mut(key).expect("request").status = status;
            }

            let startable = state.startable("Acme/App", 1);

            assert_eq!(
                startable.map(|request| request.found.key.as_str()),
                expected,
                "case: {name}"
            );
        }
    }

    #[test]
    fn tracked_prs_are_the_requests_of_the_last_thirty_days_but_the_baseline() {
        let mut state = ReviewState::default();
        state.apply_poll(vec![found("acme/app", 1, 10)], at(NOW), true);
        state.apply_poll(
            vec![
                found("acme/app", 1, 10),
                found("Acme/App", 2, 20),
                found("acme/app", 3, 30),
            ],
            at(NOW),
            false,
        );
        state
            .requests
            .get_mut("acme/app#3@30")
            .expect("request")
            .updated_at = at("2026-10-12T02:59:59Z");

        let tracked = state.tracked_prs(at(NOW));

        assert_eq!(tracked, HashSet::from(["acme/app#2".to_owned()]));
    }

    #[test]
    fn newer_request_started_at_the_head_covers_an_older_one() {
        for (name, started, newer_head, older_key, expected) in [
            ("started_at_the_head", true, HEAD, "acme/app#12@10", true),
            (
                "started_at_another_head",
                true,
                LATER,
                "acme/app#12@10",
                false,
            ),
            ("only_a_draft", false, HEAD, "acme/app#12@10", false),
            ("unknown_event", true, HEAD, "acme/app#12@unknown", false),
        ] {
            let mut state = ReviewState::default();
            state.apply_poll(
                vec![
                    keyed(older_key),
                    keyed("acme/app#12@20"),
                    keyed("acme/app#13@30"),
                ],
                at(NOW),
                false,
            );
            for key in ["acme/app#12@20", "acme/app#13@30"] {
                let other = state.requests.get_mut(key).expect("request");
                other.review_started = started;
                other.head_sha = Some(newer_head.to_owned());
            }
            // Seen after the newer event, as after the newer one's team was removed, so the order comes from the event ids.
            state
                .requests
                .get_mut(older_key)
                .expect("request")
                .first_seen = at("2026-11-12T04:00:00Z");

            assert_eq!(state.reviewed_at(older_key, HEAD), expected, "case: {name}");
            assert!(
                !state.reviewed_at("acme/app#12@20", HEAD),
                "case: {name}: an older request never covers a newer one"
            );
        }
    }

    mod record {
        use super::*;
        use crate::app::review::batch::{Listed, ReportReason};

        // As the current version writes it: millisecond UTC times, an empty requester, no team, and the zero time for a poll never made.
        const SAVED_STATE: &str = r#"{
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
      "firstSeen": "2026-11-12T03:00:00.123Z",
      "updatedAt": "2026-11-12T03:01:00.000Z",
      "choice": "code",
      "headSha": "0123456789abcdef0123456789abcdef01234567",
      "worktree": "/w/acme/app/pr-1-0123456789ab",
      "reviewStarted": true
    },
    "acme/app#2@20": {
      "key": "acme/app#2@20",
      "repo": "acme/app",
      "number": 2,
      "title": "U",
      "requester": "sato",
      "status": "queued",
      "firstSeen": "2026-11-12T03:00:00.000Z",
      "updatedAt": "2026-11-12T03:00:00.000Z"
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
        fn saved_state_is_read_with_unset_values_as_none() {
            let state = serde_json::from_str::<ReviewState>(SAVED_STATE).expect("read the state");

            let request = &state.requests["acme/app#1@10"];
            assert_eq!(state.login.as_deref(), Some("me"));
            assert_eq!(state.last_poll, None);
            assert_eq!(state.last_batch, None);
            assert_eq!(state.pending_digests, []);
            assert_eq!(request.found.requester, None);
            assert_eq!(request.status, Status::Launched);
            assert_eq!(request.first_seen, at("2026-11-12T03:00:00.123Z"));
            assert!(request.review_started);
            assert!(!state.requests["acme/app#2@20"].review_started);
            let cached = state.prs["acme/app#1"].found.as_ref().expect("cached");
            assert_eq!(cached.team.as_deref(), Some("acme/backend"));
        }

        #[test]
        fn state_is_written_with_its_keys_and_millisecond_utc_times() {
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
            let mut state =
                serde_json::from_str::<ReviewState>(SAVED_STATE).expect("read the state");
            state.last_poll = Some(at(NOW));
            state.last_error = Some("HTTP 403".to_owned());
            state.last_batch = Some(at("2026-11-12T01:50:00Z"));
            state.pending_digests.push(PendingDigest {
                ready: at("2026-11-12T02:00:00Z"),
                digest: Digest {
                    started: vec![Listed {
                        repo: "acme/app".to_owned(),
                        number: 1,
                        title: "T".to_owned(),
                        chat: Some("review-chat".to_owned()),
                        request: Some("acme/app#1@10".to_owned()),
                    }],
                    opened: vec![Listed {
                        repo: "acme/app".to_owned(),
                        number: 3,
                        title: "V".to_owned(),
                        chat: None,
                        request: None,
                    }],
                    reported: vec![(
                        Listed {
                            repo: "acme/app".to_owned(),
                            number: 2,
                            title: "U".to_owned(),
                            chat: None,
                            request: None,
                        },
                        ReportReason::ApprovedByOther,
                    )],
                    failed: 1,
                },
            });

            let again = serde_json::from_str::<ReviewState>(
                &serde_json::to_string(&state).expect("write the state"),
            )
            .expect("read it back");

            assert_eq!(again, state);
        }

        #[test]
        fn state_of_another_version_or_with_an_unreadable_time_is_refused() {
            for (name, content) in [
                (
                    "other_version",
                    SAVED_STATE.replace(r#""version": 1"#, r#""version": 2"#),
                ),
                (
                    "unreadable_time",
                    SAVED_STATE.replace("2026-11-12T03:01:00.000Z", "noon"),
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

    // A request of acme/app under `key`, whose number the key names.
    fn keyed(key: &str) -> Found {
        let number = key
            .split_once('#')
            .and_then(|(_, rest)| rest.split_once('@'))
            .and_then(|(number, _)| number.parse().ok())
            .expect("a key acme/app#number@event");
        Found {
            key: key.to_owned(),
            ..found("acme/app", number, 0)
        }
    }

    fn status_of(state: &ReviewState, key: &str) -> Status {
        state.requests[key].status
    }

    fn at(text: &str) -> Timestamp {
        text.parse().expect("parse the time")
    }
}
