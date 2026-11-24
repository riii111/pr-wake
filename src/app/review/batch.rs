use std::collections::{BTreeMap, BTreeSet};

use jiff::{Timestamp, Zoned, civil::Weekday};
use serde::{Deserialize, Serialize};

use super::{config::Batches, one_line, state::Found};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Slot {
    pub(crate) start: Timestamp,
    pub(crate) ready: Timestamp,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Reviewers {
    pub(crate) author: Option<String>,
    pub(crate) requested: Vec<String>,
    pub(crate) reviews: Vec<SubmittedReview>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubmittedReview {
    pub(crate) author: String,
    pub(crate) state: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Plan {
    Start,
    Report(ReportReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ReportReason {
    ManyReviewers,
    ApprovedByOther,
    // A newer request of the PR already started its review at the same head, which may still be running.
    AlreadyReviewed,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Digest {
    pub(crate) started: Vec<Listed>,
    pub(crate) reported: Vec<(Listed, ReportReason)>,
    pub(crate) failed: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Listed {
    pub(crate) number: u64,
    pub(crate) title: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NowRequest {
    // `owner/repo#number` of the one PR to start; every queued request when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) pr: Option<String>,
}

// With the owner, this many reviewers means others are expected to review first.
const MANY_REVIEWERS: usize = 3;
const APPROVED: &str = "APPROVED";
const MAX_LISTED: usize = 5;
const MAX_TITLE_CHARS: usize = 30;

// The latest slot of today that has started and not run yet, so a slot missed while asleep runs once, late.
#[must_use]
pub(crate) fn due_slot(batches: &Batches, now: &Zoned, last: Option<Timestamp>) -> Option<Slot> {
    if matches!(now.weekday(), Weekday::Saturday | Weekday::Sunday) {
        return None;
    }
    batches
        .ready_at
        .iter()
        .rev()
        .find_map(|ready| {
            let ready = now
                .date()
                .to_datetime(*ready)
                .to_zoned(now.time_zone().clone())
                .ok()?
                .timestamp();
            let start = ready.checked_sub(batches.lead).ok()?;
            (start <= now.timestamp()).then_some(Slot { start, ready })
        })
        .filter(|slot| last.is_none_or(|last| last < slot.start))
}

#[must_use]
pub(crate) fn plan(reviewers: &Reviewers, login: &str) -> Plan {
    let author = reviewers.author.as_deref().map(str::to_lowercase);
    let login = login.to_lowercase();
    // The author answering a review comment submits a review too, which does not make them a reviewer.
    let others = reviewers
        .requested
        .iter()
        .chain(reviewers.reviews.iter().map(|review| &review.author))
        .map(|reviewer| reviewer.to_lowercase())
        .filter(|reviewer| *reviewer != login && Some(reviewer) != author.as_ref())
        .collect::<BTreeSet<_>>();

    if others.len() + 1 >= MANY_REVIEWERS {
        return Plan::Report(ReportReason::ManyReviewers);
    }
    if approved_by_other(reviewers, &login) {
        return Plan::Report(ReportReason::ApprovedByOther);
    }
    Plan::Start
}

impl ReportReason {
    #[must_use]
    pub(crate) const fn text(self) -> &'static str {
        match self {
            Self::ManyReviewers => "3人以上を指名",
            Self::ApprovedByOther => "他の人が承認済み",
            Self::AlreadyReviewed => "同じheadのレビューを開始済み",
        }
    }
}

impl Digest {
    #[must_use]
    pub(crate) fn subtitle(&self) -> String {
        format!(
            "レビュー {}件",
            self.started.len() + self.reported.len() + self.failed
        )
    }

    // A notification shows a few lines, so the PRs past them are only counted.
    #[must_use]
    pub(crate) fn message(&self) -> String {
        let mut listed = self.started.iter().map(Listed::line).chain(
            self.reported
                .iter()
                .map(|(pr, reason)| format!("報告のみ: {}（{}）", pr.line(), reason.text())),
        );
        let mut lines = listed.by_ref().take(MAX_LISTED).collect::<Vec<_>>();
        let rest = listed.count();
        if rest > 0 {
            lines.push(format!("ほか{rest}件"));
        }
        if self.failed > 0 {
            lines.push(format!("開始できず: {}件", self.failed));
        }
        lines.join("\n")
    }
}

impl Listed {
    #[must_use]
    pub(crate) fn of(found: &Found) -> Self {
        Self {
            number: found.number,
            title: found.title.clone(),
        }
    }

    fn line(&self) -> String {
        let title = one_line(&self.title);
        let title = if title.chars().count() > MAX_TITLE_CHARS {
            let mut short = title.chars().take(MAX_TITLE_CHARS - 1).collect::<String>();
            short.push('…');
            short
        } else {
            title
        };
        format!("#{} {title}", self.number)
    }
}

// A comment after an approval leaves the approval standing, so only approvals, change requests and dismissals count.
fn approved_by_other(reviewers: &Reviewers, login: &str) -> bool {
    let author = reviewers.author.as_deref().map(str::to_lowercase);
    let mut latest = BTreeMap::new();
    for review in &reviewers.reviews {
        let reviewer = review.author.to_lowercase();
        if reviewer == login || Some(&reviewer) == author.as_ref() {
            continue;
        }
        if matches!(
            review.state.as_str(),
            "APPROVED" | "CHANGES_REQUESTED" | "DISMISSED"
        ) {
            latest.insert(reviewer, review.state.as_str());
        }
    }
    latest.values().any(|state| *state == APPROVED)
}

#[cfg(test)]
mod tests {
    use jiff::{SignedDuration, civil::Time, tz::TimeZone};

    use super::*;

    #[test]
    fn slot_is_due_from_its_start_on_weekdays_until_it_has_run() {
        // 2026-11-18 is a Wednesday.
        for (name, now, last, expected) in [
            ("before_the_first", "2026-11-18T10:49:59", None, None),
            (
                "at_the_first_start",
                "2026-11-18T10:50:00",
                None,
                Some("2026-11-18T10:50:00"),
            ),
            (
                "first_already_run",
                "2026-11-18T10:55:00",
                Some("2026-11-18T10:50:00"),
                None,
            ),
            (
                "missed_while_asleep",
                "2026-11-18T13:00:00",
                Some("2026-11-17T15:50:00"),
                Some("2026-11-18T10:50:00"),
            ),
            (
                "only_the_latest_of_two_missed",
                "2026-11-18T17:00:00",
                Some("2026-11-17T15:50:00"),
                Some("2026-11-18T15:50:00"),
            ),
            (
                "second_after_the_first_ran",
                "2026-11-18T15:50:00",
                Some("2026-11-18T10:50:00"),
                Some("2026-11-18T15:50:00"),
            ),
            ("saturday", "2026-11-21T10:55:00", None, None),
            ("sunday", "2026-11-22T16:00:00", None, None),
            (
                "monday",
                "2026-11-23T10:51:00",
                Some("2026-11-20T15:50:00"),
                Some("2026-11-23T10:50:00"),
            ),
        ] {
            let slot = due_slot(
                &batches(),
                &local(now),
                last.map(|last| local(last).timestamp()),
            );

            let expected = expected.map(|start| {
                let start = local(start).timestamp();
                Slot {
                    start,
                    ready: start
                        .checked_add(SignedDuration::from_mins(10))
                        .expect("a later time"),
                }
            });
            assert_eq!(slot, expected, "case: {name}");
        }
    }

    #[test]
    fn plan_reports_what_others_are_expected_to_review() {
        for (name, requested, reviews, expected) in [
            ("only_me", &[][..], &[][..], Plan::Start),
            ("me_requested_too", &["Me"][..], &[][..], Plan::Start),
            ("one_other", &["sato"][..], &[][..], Plan::Start),
            (
                "one_other_commented",
                &[][..],
                &[("sato", "COMMENTED")][..],
                Plan::Start,
            ),
            (
                "one_other_approved",
                &[][..],
                &[("sato", "APPROVED")][..],
                Plan::Report(ReportReason::ApprovedByOther),
            ),
            (
                "approval_then_comment",
                &[][..],
                &[("sato", "APPROVED"), ("sato", "COMMENTED")][..],
                Plan::Report(ReportReason::ApprovedByOther),
            ),
            (
                "approval_dismissed",
                &["sato"][..],
                &[("sato", "APPROVED"), ("sato", "DISMISSED")][..],
                Plan::Start,
            ),
            (
                "my_own_approval",
                &["sato"][..],
                &[("me", "APPROVED")][..],
                Plan::Start,
            ),
            (
                "author_replies",
                &["sato"][..],
                &[("tanaka", "COMMENTED")][..],
                Plan::Start,
            ),
            (
                "two_others",
                &["sato", "suzuki"][..],
                &[][..],
                Plan::Report(ReportReason::ManyReviewers),
            ),
            (
                "one_requested_one_reviewed",
                &["sato"][..],
                &[("Suzuki", "COMMENTED")][..],
                Plan::Report(ReportReason::ManyReviewers),
            ),
        ] {
            let reviewers = Reviewers {
                author: Some("tanaka".to_owned()),
                requested: requested.iter().map(|login| (*login).to_owned()).collect(),
                reviews: reviews
                    .iter()
                    .map(|(author, state)| SubmittedReview {
                        author: (*author).to_owned(),
                        state: (*state).to_owned(),
                    })
                    .collect(),
            };

            assert_eq!(plan(&reviewers, "me"), expected, "case: {name}");
        }
    }

    #[test]
    fn digest_lists_the_started_then_the_reported_prs() {
        for (name, digest, subtitle, message) in [
            (
                "started_and_reported",
                Digest {
                    started: vec![listed(12, "Fix\nthe bug")],
                    reported: vec![
                        (listed(33344, "Add a job"), ReportReason::ManyReviewers),
                        (listed(7, "Rename"), ReportReason::ApprovedByOther),
                    ],
                    failed: 0,
                },
                "レビュー 3件",
                "#12 Fix the bug\n報告のみ: #33344 Add a job（3人以上を指名）\n報告のみ: #7 Rename（他の人が承認済み）",
            ),
            (
                "long_title_and_failures",
                Digest {
                    started: vec![listed(1, &"a".repeat(31))],
                    reported: Vec::new(),
                    failed: 2,
                },
                "レビュー 3件",
                &format!("#1 {}…\n開始できず: 2件", "a".repeat(29)),
            ),
            (
                "more_than_fit",
                Digest {
                    started: (1..=7).map(|number| listed(number, "T")).collect(),
                    reported: Vec::new(),
                    failed: 0,
                },
                "レビュー 7件",
                "#1 T\n#2 T\n#3 T\n#4 T\n#5 T\nほか2件",
            ),
        ] {
            assert_eq!(digest.subtitle(), subtitle, "case: {name}");
            assert_eq!(digest.message(), message, "case: {name}");
        }
    }

    fn batches() -> Batches {
        Batches {
            ready_at: vec![Time::constant(11, 0, 0, 0), Time::constant(16, 0, 0, 0)],
            lead: SignedDuration::from_mins(10),
        }
    }

    fn local(text: &str) -> Zoned {
        text.parse::<jiff::civil::DateTime>()
            .expect("parse the time")
            .to_zoned(TimeZone::fixed(jiff::tz::offset(9)))
            .expect("a local time")
    }

    fn listed(number: u64, title: &str) -> Listed {
        Listed {
            number,
            title: title.to_owned(),
        }
    }
}
