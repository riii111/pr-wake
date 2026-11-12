use std::collections::HashSet;

use super::{
    classify::{self, Verdict},
    config::WatchConfig,
    pr::{ItemKind, PrDetail},
    reviewers::{BotProgress, ReviewBot},
    state::{WatchEvent, WatchState, bot_key},
};
use crate::app::time;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Found {
    pub(crate) events: Vec<WatchEvent>,
    pub(crate) seen: Vec<String>,
    pub(crate) conflict_head: Option<String>,
}

const PENDING: &str = "PENDING";
const SUCCESS: &str = "SUCCESS";
const CONFLICTING: &str = "CONFLICTING";
const MERGEABLE: &str = "MERGEABLE";

pub(crate) fn next_events(
    detail: &PrDetail,
    state: &WatchState,
    linked_at: &str,
    // The owner's own account is fixed per repository, so its comments are known without another
    // request.
    owner: &str,
    config: &WatchConfig,
) -> Found {
    // The first read of a PR takes what is already on it as seen, so starting the watch never
    // sends what the owner has handled.
    if state.summary.is_none() {
        return baseline(detail, &config.review_bots);
    }

    let mut seen = Seen::new(&state.seen);
    let findings = new_findings(detail, &mut seen, linked_at, owner, config);
    let done = config
        .review_bots
        .iter()
        .filter_map(|bot| review_done(detail, bot, &mut seen, linked_at, &findings))
        .collect::<Vec<_>>();
    let (conflict, conflict_head) = conflict_of(detail, state.conflict_head.as_deref());
    let failed = failed_checks(detail, &mut seen);

    Found {
        events: done
            .into_iter()
            .chain(findings)
            .chain(conflict)
            .chain(failed)
            .collect(),
        seen: seen.keys,
        conflict_head,
    }
}

fn baseline(detail: &PrDetail, bots: &[ReviewBot]) -> Found {
    let head = &detail.summary.head;
    Found {
        events: Vec::new(),
        // Every comment is taken, findings or not, so one whose verdict changes later is still
        // not new.
        seen: detail
            .comments
            .iter()
            .map(|comment| comment.id.clone())
            .chain(bots.iter().flat_map(|bot| bot_baseline(detail, bot)))
            .chain(
                detail
                    .failed_required
                    .iter()
                    .map(|check| check_key(head, &check.name)),
            )
            .collect(),
        conflict_head: (detail.summary.mergeable == CONFLICTING).then(|| head.clone()),
    }
}

fn new_findings(
    detail: &PrDetail,
    seen: &mut Seen,
    linked_at: &str,
    owner: &str,
    config: &WatchConfig,
) -> Vec<WatchEvent> {
    // A bot's findings wait for its review to complete, so they arrive with it in one message.
    let reviewing = detail
        .summary
        .bots
        .iter()
        .filter(|(_, status)| {
            status
                .as_ref()
                .is_some_and(|status| status.state == PENDING)
        })
        .map(|(id, _)| id.as_str())
        .collect::<HashSet<_>>();
    classify::classify(&detail.comments, owner, &config.ignore, &config.review_bots)
        .into_iter()
        .filter_map(|verdict| match verdict {
            Verdict::Notified(finding) => Some(finding),
            Verdict::Ignored(_) => None,
        })
        .filter(|finding| {
            finding
                .bot
                .as_deref()
                .is_none_or(|bot| !reviewing.contains(bot))
                && time::is_at_or_after(&finding.created_at, linked_at)
                && seen.insert(&finding.id)
        })
        .map(|finding| WatchEvent::Finding { finding })
        .collect()
}

fn review_done(
    detail: &PrDetail,
    bot: &ReviewBot,
    seen: &mut Seen,
    linked_at: &str,
    findings: &[WatchEvent],
) -> Option<WatchEvent> {
    // A bot added after the PR's first read starts from its reviews as they stand, as the first
    // read does.
    if !seen.contains(&bot_key(&bot.id)) {
        for key in bot_baseline(detail, bot) {
            seen.insert(&key);
        }
        return None;
    }

    let status = detail.summary.bots.get(&bot.id)?.as_ref()?;
    if !review_completed(detail, bot)
        || !time::is_at_or_after(&status.created_at, linked_at)
        || !seen.insert(&review_key(bot, &detail.summary.head))
    {
        return None;
    }

    // Whether the review found anything is decided by its new inline findings; the summary
    // comment only confirms it.
    let inline = findings
        .iter()
        .filter(|event| {
            matches!(event, WatchEvent::Finding { finding }
                if finding.bot.as_deref() == Some(bot.id.as_str())
                    && finding.kind == ItemKind::Inline)
        })
        .count();
    // A bot without a summary to check against always agrees.
    let summary_agrees =
        bot.no_actionable.is_none() || (inline == 0) == detail.no_actionable.contains(&bot.id);
    Some(WatchEvent::ReviewDone {
        bot: bot.id.clone(),
        name: bot.name.clone(),
        head: detail.summary.head.clone(),
        findings: inline,
        summary_agrees,
    })
}

fn review_completed(detail: &PrDetail, bot: &ReviewBot) -> bool {
    let (Some(progress), Some(Some(status))) = (&bot.status, detail.summary.bots.get(&bot.id))
    else {
        return false;
    };
    status.state == SUCCESS
        && match progress {
            BotProgress::CommitStatus { completed, .. } => {
                status.description.contains(completed.as_str())
            }
            BotProgress::CheckRun(_) => true,
        }
}

fn bot_baseline(detail: &PrDetail, bot: &ReviewBot) -> Vec<String> {
    let mut keys = vec![bot_key(&bot.id)];
    if review_completed(detail, bot) {
        keys.push(review_key(bot, &detail.summary.head));
    }
    keys
}

// The id leads, so the keys CodeRabbit's reviews were recorded under before bots were
// configurable still match.
fn review_key(bot: &ReviewBot, head: &str) -> String {
    format!("{}:{head}", bot.id)
}

fn conflict_of(detail: &PrDetail, notified: Option<&str>) -> (Option<WatchEvent>, Option<String>) {
    let summary = &detail.summary;
    // Told once per head while the PR conflicts, so a base that keeps moving does not repeat it.
    if summary.mergeable == CONFLICTING && notified != Some(summary.head.as_str()) {
        return (
            Some(WatchEvent::Conflict {
                head: summary.head.clone(),
            }),
            Some(summary.head.clone()),
        );
    }
    let head = if summary.mergeable == MERGEABLE {
        None
    } else {
        notified.map(str::to_owned)
    };
    (None, head)
}

fn failed_checks(detail: &PrDetail, seen: &mut Seen) -> Vec<WatchEvent> {
    let head = &detail.summary.head;
    detail
        .failed_required
        .iter()
        .filter(|check| seen.insert(&check_key(head, &check.name)))
        .map(|check| WatchEvent::CiFailed {
            head: head.clone(),
            check: check.clone(),
        })
        .collect()
}

fn check_key(head: &str, name: &str) -> String {
    format!("ci:{head}:{name}")
}

// Keys keep the order they were first seen in, as the record lists them.
struct Seen {
    keys: Vec<String>,
    known: HashSet<String>,
}

impl Seen {
    fn new(keys: &[String]) -> Self {
        let mut seen = Self {
            keys: Vec::with_capacity(keys.len()),
            known: HashSet::with_capacity(keys.len()),
        };
        for key in keys {
            seen.insert(key);
        }
        seen
    }

    fn contains(&self, key: &str) -> bool {
        self.known.contains(key)
    }

    // True when the key is new.
    fn insert(&mut self, key: &str) -> bool {
        if !self.known.insert(key.to_owned()) {
            return false;
        }
        self.keys.push(key.to_owned());
        true
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::app::watch::{
        config::Repository,
        pr::PrComment,
        reviewers,
        state::{BotStatus, FailedCheck, Finding, PrSummary},
    };

    const PR: &str = "https://github.com/org/pay/pull/7";
    const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const LINKED_AT: &str = "2026-10-23T10:00:00.000Z";

    mod first_read {
        use super::*;

        #[test]
        fn everything_on_the_pr_is_taken_as_seen() {
            let detail = PrDetail {
                summary: PrSummary {
                    mergeable: CONFLICTING.to_owned(),
                    ..summary(Some(completed()))
                },
                failed_required: vec![build()],
                comments: vec![comment(1, "2026-10-23T11:00:00Z")],
                no_actionable: Vec::new(),
            };

            let found = next_events(
                &detail,
                &WatchState::default(),
                LINKED_AT,
                "owner",
                &config(),
            );

            assert_eq!(
                found,
                Found {
                    events: Vec::new(),
                    seen: vec![
                        "inline:1".to_owned(),
                        "bot:coderabbit".to_owned(),
                        format!("coderabbit:{HEAD}"),
                        format!("ci:{HEAD}:build"),
                    ],
                    conflict_head: Some(HEAD.to_owned()),
                }
            );
        }
    }

    mod findings {
        use super::*;

        #[test]
        fn only_findings_made_since_the_link_and_not_yet_seen_are_new() {
            let detail = detail(vec![
                comment(1, "2026-10-23T09:59:59Z"),
                comment(2, "2026-10-23T11:00:00Z"),
                comment(3, "2026-10-23T11:00:00Z"),
            ]);
            let state = read_state(&["bot:coderabbit", "inline:3"]);

            let found = next_events(&detail, &state, LINKED_AT, "owner", &config());

            assert_eq!(finding_ids(&found.events), ["inline:2"]);
            assert_eq!(found.seen, ["bot:coderabbit", "inline:3", "inline:2"]);
        }

        #[test]
        fn bot_findings_wait_while_its_review_runs() {
            let pending = BotStatus {
                state: PENDING.to_owned(),
                ..completed()
            };
            let detail = PrDetail {
                summary: summary(Some(pending)),
                ..detail(vec![
                    bot_comment(1, "**🟠 Major**"),
                    comment(2, "2026-10-23T11:00:00Z"),
                ])
            };

            let found = next_events(
                &detail,
                &read_state(&["bot:coderabbit"]),
                LINKED_AT,
                "owner",
                &config(),
            );

            assert_eq!(finding_ids(&found.events), ["inline:2"]);
        }
    }

    mod review_done {
        use super::*;

        #[test]
        fn completed_review_is_told_once_with_its_new_inline_findings() {
            let detail = PrDetail {
                summary: summary(Some(completed())),
                ..detail(vec![bot_comment(1, "**🟠 Major**")])
            };
            let state = read_state(&["bot:coderabbit"]);

            let first = next_events(&detail, &state, LINKED_AT, "owner", &config());
            let again = next_events(
                &detail,
                &WatchState {
                    seen: first.seen.clone(),
                    ..state
                },
                LINKED_AT,
                "owner",
                &config(),
            );

            assert_eq!(
                first.events.first(),
                Some(&WatchEvent::ReviewDone {
                    bot: "coderabbit".to_owned(),
                    name: "CodeRabbit".to_owned(),
                    head: HEAD.to_owned(),
                    findings: 1,
                    summary_agrees: true,
                })
            );
            assert_eq!(finding_ids(&first.events), ["inline:1"]);
            assert!(again.events.is_empty());
        }

        #[test]
        fn summary_agrees_when_it_matches_the_inline_findings() {
            for (name, comments, no_actionable, expected) in [
                (
                    "findings_without_summary",
                    vec![bot_comment(1, "x")],
                    vec![],
                    true,
                ),
                ("no_findings_with_summary", vec![], vec!["coderabbit"], true),
                ("no_findings_without_summary", vec![], vec![], false),
                (
                    "findings_with_summary",
                    vec![bot_comment(1, "x")],
                    vec!["coderabbit"],
                    false,
                ),
            ] {
                let detail = PrDetail {
                    summary: summary(Some(completed())),
                    no_actionable: no_actionable.into_iter().map(str::to_owned).collect(),
                    ..detail(comments)
                };

                let found = next_events(
                    &detail,
                    &read_state(&["bot:coderabbit"]),
                    LINKED_AT,
                    "owner",
                    &config(),
                );

                let agrees = found.events.iter().find_map(|event| match event {
                    WatchEvent::ReviewDone { summary_agrees, .. } => Some(*summary_agrees),
                    _ => None,
                });
                assert_eq!(agrees, Some(expected), "case: {name}");
            }
        }

        #[test]
        fn review_is_not_told_until_it_completes_after_the_link() {
            for (name, status) in [
                ("no_status", None),
                (
                    "still_running",
                    Some(BotStatus {
                        state: PENDING.to_owned(),
                        ..completed()
                    }),
                ),
                (
                    "other_description",
                    Some(BotStatus {
                        description: "Review in progress".to_owned(),
                        ..completed()
                    }),
                ),
                (
                    "completed_before_the_link",
                    Some(BotStatus {
                        created_at: "2026-10-23T09:00:00Z".to_owned(),
                        ..completed()
                    }),
                ),
            ] {
                let detail = PrDetail {
                    summary: summary(status),
                    ..detail(Vec::new())
                };

                let found = next_events(
                    &detail,
                    &read_state(&["bot:coderabbit"]),
                    LINKED_AT,
                    "owner",
                    &config(),
                );

                assert!(found.events.is_empty(), "case: {name}");
            }
        }

        #[test]
        fn bot_added_later_starts_from_its_finished_reviews() {
            let detail = PrDetail {
                summary: summary(Some(completed())),
                ..detail(vec![bot_comment(1, "**🟠 Major**")])
            };

            let found = next_events(&detail, &read_state(&[]), LINKED_AT, "owner", &config());

            assert_eq!(finding_ids(&found.events), ["inline:1"]);
            assert!(
                !found
                    .events
                    .iter()
                    .any(|event| matches!(event, WatchEvent::ReviewDone { .. }))
            );
            assert_eq!(
                found.seen,
                [
                    "inline:1".to_owned(),
                    "bot:coderabbit".to_owned(),
                    format!("coderabbit:{HEAD}")
                ]
            );
        }

        #[test]
        fn check_run_success_alone_completes_the_review() {
            let config = WatchConfig {
                review_bots: vec![reviewers::preset("claude").expect("the preset")],
                ..config()
            };
            let detail = PrDetail {
                summary: PrSummary {
                    bots: BTreeMap::from([(
                        "claude".to_owned(),
                        Some(BotStatus {
                            description: String::new(),
                            ..completed()
                        }),
                    )]),
                    ..summary(None)
                },
                ..detail(Vec::new())
            };

            let found = next_events(
                &detail,
                &read_state(&["bot:claude"]),
                LINKED_AT,
                "owner",
                &config,
            );

            assert!(matches!(
                found.events.as_slice(),
                [WatchEvent::ReviewDone { bot, findings: 0, summary_agrees: true, .. }] if bot == "claude"
            ));
        }
    }

    mod pr_state {
        use super::*;

        #[test]
        fn conflict_is_told_once_per_head_and_forgotten_once_mergeable() {
            for (name, mergeable, notified, told, kept) in [
                ("new_conflict", CONFLICTING, None, true, Some(HEAD)),
                ("same_head", CONFLICTING, Some(HEAD), false, Some(HEAD)),
                ("new_head", CONFLICTING, Some("bbbbbbb"), true, Some(HEAD)),
                ("resolved", MERGEABLE, Some(HEAD), false, None),
                ("not_known_yet", "UNKNOWN", Some(HEAD), false, Some(HEAD)),
            ] {
                let detail = PrDetail {
                    summary: PrSummary {
                        mergeable: mergeable.to_owned(),
                        ..summary(None)
                    },
                    ..detail(Vec::new())
                };
                let state = WatchState {
                    conflict_head: notified.map(str::to_owned),
                    ..read_state(&["bot:coderabbit"])
                };

                let found = next_events(&detail, &state, LINKED_AT, "owner", &config());

                assert_eq!(
                    (
                        found.events.contains(&WatchEvent::Conflict {
                            head: HEAD.to_owned()
                        }),
                        found.conflict_head.as_deref(),
                    ),
                    (told, kept),
                    "case: {name}"
                );
            }
        }

        #[test]
        fn failed_required_check_is_told_once_per_head() {
            let detail = PrDetail {
                failed_required: vec![build()],
                ..detail(Vec::new())
            };
            let state = read_state(&["bot:coderabbit"]);

            let first = next_events(&detail, &state, LINKED_AT, "owner", &config());
            let again = next_events(
                &detail,
                &WatchState {
                    seen: first.seen.clone(),
                    ..state
                },
                LINKED_AT,
                "owner",
                &config(),
            );

            assert_eq!(
                first.events,
                [WatchEvent::CiFailed {
                    head: HEAD.to_owned(),
                    check: build(),
                }]
            );
            assert!(again.events.is_empty());
        }

        #[test]
        fn events_are_ordered_review_findings_conflict_then_checks() {
            let detail = PrDetail {
                summary: PrSummary {
                    mergeable: CONFLICTING.to_owned(),
                    ..summary(Some(completed()))
                },
                failed_required: vec![build()],
                comments: vec![comment(1, "2026-10-23T11:00:00Z")],
                no_actionable: vec!["coderabbit".to_owned()],
            };

            let found = next_events(
                &detail,
                &read_state(&["bot:coderabbit"]),
                LINKED_AT,
                "owner",
                &config(),
            );

            let kinds = found
                .events
                .iter()
                .map(|event| match event {
                    WatchEvent::ReviewDone { .. } => "review_done",
                    WatchEvent::Finding { .. } => "finding",
                    WatchEvent::Conflict { .. } => "conflict",
                    WatchEvent::CiFailed { .. } => "ci_failed",
                })
                .collect::<Vec<_>>();
            assert_eq!(kinds, ["review_done", "finding", "conflict", "ci_failed"]);
        }
    }

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
            quiet_hours: None,
            instructions: None,
        }
    }

    // A PR the watch has read before.
    fn read_state(seen: &[&str]) -> WatchState {
        WatchState {
            summary: Some(summary(None)),
            seen: seen.iter().map(|key| (*key).to_owned()).collect(),
            ..WatchState::default()
        }
    }

    fn summary(coderabbit: Option<BotStatus>) -> PrSummary {
        PrSummary {
            state: "OPEN".to_owned(),
            is_draft: false,
            head: HEAD.to_owned(),
            updated_at: "2026-10-23T11:00:00Z".to_owned(),
            mergeable: MERGEABLE.to_owned(),
            bots: BTreeMap::from([("coderabbit".to_owned(), coderabbit)]),
            rollup: Some("SUCCESS".to_owned()),
        }
    }

    fn completed() -> BotStatus {
        BotStatus {
            state: SUCCESS.to_owned(),
            description: "Review completed".to_owned(),
            created_at: "2026-10-23T11:00:00Z".to_owned(),
        }
    }

    fn detail(comments: Vec<PrComment>) -> PrDetail {
        PrDetail {
            summary: summary(None),
            failed_required: Vec::new(),
            comments,
            no_actionable: Vec::new(),
        }
    }

    fn build() -> FailedCheck {
        FailedCheck {
            name: "build".to_owned(),
            url: None,
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

    fn bot_comment(id: u64, body: &str) -> PrComment {
        PrComment {
            author: "coderabbitai[bot]".to_owned(),
            body: body.to_owned(),
            ..comment(id, "2026-10-23T11:00:00Z")
        }
    }

    fn finding_ids(events: &[WatchEvent]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|event| match event {
                WatchEvent::Finding {
                    finding: Finding { id, .. },
                } => Some(id.as_str()),
                _ => None,
            })
            .collect()
    }
}
