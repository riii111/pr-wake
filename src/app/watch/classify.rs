use std::fmt::{self, Display, Formatter};

use super::{
    config::IgnoreRule,
    pr::{ItemKind, PrComment},
    reviewers::{self, ReviewBot},
    state::{Approval, Finding},
};

// One per comment, in the order of the comments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict<'bot> {
    Notified(Finding),
    Ignored(Ignored<'bot>),
}

// Each carries the name of the review bot it is about, as the dry run shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ignored<'bot> {
    Owner,
    IgnoreRule,
    IssueComment,
    BotReply(&'bot str),
    BotReviewWithoutFindings(&'bot str),
    ResolvedBotThread(&'bot str),
    EmptyReview,
}

pub(crate) fn classify<'bot>(
    comments: &[PrComment],
    owner: &str,
    rules: &[IgnoreRule],
    bots: &'bot [ReviewBot],
) -> Vec<Verdict<'bot>> {
    comments
        .iter()
        .map(|comment| {
            let bot = reviewers::bot_of(bots, &comment.author);
            ignored_reason(comment, owner, rules, bot).map_or_else(
                || Verdict::Notified(finding_of(comment, bot)),
                Verdict::Ignored,
            )
        })
        .collect()
}

fn ignored_reason<'bot>(
    comment: &PrComment,
    owner: &str,
    rules: &[IgnoreRule],
    bot: Option<&'bot ReviewBot>,
) -> Option<Ignored<'bot>> {
    if comment.author.eq_ignore_ascii_case(owner) {
        return Some(Ignored::Owner);
    }
    if rules.iter().any(|rule| matches(rule, comment)) {
        return Some(Ignored::IgnoreRule);
    }
    // Findings are left on the code or in a review; the conversation carries requests and replies
    // such as "will look".
    if comment.kind == ItemKind::IssueComment {
        return Some(Ignored::IssueComment);
    }

    if let Some(bot) = bot {
        // A bot's replies answer the owner in a thread already reported when it opened.
        if comment.kind == ItemKind::Inline && comment.reply {
            return Some(Ignored::BotReply(&bot.name));
        }
        if comment.kind == ItemKind::Review
            && bot
                .actionable_review
                .as_ref()
                .is_some_and(|actionable| !comment.body.contains(actionable.as_str()))
        {
            return Some(Ignored::BotReviewWithoutFindings(&bot.name));
        }
        // A bot's finding resolved before the watch read it was already handled; a person's reply
        // there still reopens the talk.
        if comment.kind == ItemKind::Inline && comment.resolved {
            return Some(Ignored::ResolvedBotThread(&bot.name));
        }
    }

    if comment.kind == ItemKind::Review
        && !is_approval(comment, bot)
        && comment.body.trim().is_empty()
    {
        return Some(Ignored::EmptyReview);
    }
    None
}

fn matches(rule: &IgnoreRule, comment: &PrComment) -> bool {
    rule.author.eq_ignore_ascii_case(&comment.author)
        && rule.kind.is_none_or(|kind| kind == comment.kind)
        && rule
            .body_includes
            .as_ref()
            .is_none_or(|text| comment.body.contains(text.as_str()))
}

fn finding_of(comment: &PrComment, bot: Option<&ReviewBot>) -> Finding {
    Finding {
        id: comment.id.clone(),
        kind: comment.kind,
        bot: bot.map(|bot| bot.id.clone()),
        label: label_of(comment, bot),
        path: comment.path.clone(),
        line: comment.line,
        url: comment.url.clone(),
        created_at: comment.created_at.clone(),
        approval: approval_of(comment, bot),
    }
}

// A review bot's approval is read like its other reviews.
fn is_approval(comment: &PrComment, bot: Option<&ReviewBot>) -> bool {
    bot.is_none() && comment.kind == ItemKind::Review && comment.approved
}

fn approval_of(comment: &PrComment, bot: Option<&ReviewBot>) -> Option<Approval> {
    if !is_approval(comment, bot) {
        return None;
    }
    Some(if comment.body.trim().is_empty() {
        Approval::WithoutBody
    } else {
        Approval::WithBody
    })
}

fn label_of(comment: &PrComment, bot: Option<&ReviewBot>) -> String {
    if is_approval(comment, bot) {
        return format!("{}・Approve", comment.author);
    }
    let Some(bot) = bot else {
        return comment.author.clone();
    };
    if comment.kind == ItemKind::Review {
        return format!("{}・レビュー", bot.name);
    }
    reviewers::severity_of(bot, &comment.body).map_or_else(
        || bot.name.clone(),
        |severity| format!("{}・{severity}", bot.name),
    )
}

impl Display for Ignored<'_> {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Owner => formatter.write_str("owner"),
            Self::IgnoreRule => formatter.write_str("ignore rule"),
            Self::IssueComment => formatter.write_str("issue comment"),
            Self::BotReply(bot) => write!(formatter, "{bot} reply"),
            Self::BotReviewWithoutFindings(bot) => {
                write!(formatter, "{bot} review without actionable comments")
            }
            Self::ResolvedBotThread(bot) => write!(formatter, "resolved {bot} thread"),
            Self::EmptyReview => formatter.write_str("empty review"),
        }
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    const PR: &str = "https://github.com/org/pay/pull/7";

    mod verdict {
        use super::*;

        #[rstest]
        #[case::reviewer_inline_comment(comment())]
        #[case::reviewer_reply_in_a_thread(PrComment { reply: true, ..comment() })]
        #[case::reviewer_reply_in_a_resolved_thread(PrComment {
            reply: true,
            resolved: true,
            ..comment()
        })]
        #[case::reviewer_review_with_a_body(PrComment {
            kind: ItemKind::Review,
            body: "LGTM with one question".to_owned(),
            ..comment()
        })]
        #[case::new_coderabbit_thread(PrComment { author: CODERABBIT.to_owned(), ..comment() })]
        #[case::coderabbit_review_posting_actionable_comments(PrComment {
            author: CODERABBIT.to_owned(),
            kind: ItemKind::Review,
            body: "**Actionable comments posted: 2**".to_owned(),
            ..comment()
        })]
        #[case::github_actions_annotation_outside_the_rule(PrComment {
            author: "github-actions[bot]".to_owned(),
            body: "lint: unused import".to_owned(),
            ..comment()
        })]
        fn comment_that_may_need_work_is_notified(#[case] comment: PrComment) {
            let verdicts = verdicts_of(comment);

            assert_eq!(verdicts, ["notified"]);
        }

        #[test]
        fn comment_is_ignored_for_who_wrote_it_or_where_it_is() {
            for (name, comment, expected) in [
                (
                    "owner_comment",
                    PrComment {
                        author: "Owner".to_owned(),
                        ..comment()
                    },
                    "owner",
                ),
                (
                    "coderabbit_walkthrough",
                    PrComment {
                        author: CODERABBIT.to_owned(),
                        kind: ItemKind::IssueComment,
                        ..comment()
                    },
                    "ignore rule",
                ),
                (
                    "phase_timing_annotation",
                    PrComment {
                        author: "github-actions[bot]".to_owned(),
                        body: "📝 **[phase-timing]** <hook-duration>".to_owned(),
                        ..comment()
                    },
                    "ignore rule",
                ),
                (
                    "blacksmith_ci_notice",
                    PrComment {
                        author: "blacksmith-sh[bot]".to_owned(),
                        kind: ItemKind::IssueComment,
                        ..comment()
                    },
                    "ignore rule",
                ),
                (
                    "reviewer_issue_comment",
                    PrComment {
                        kind: ItemKind::IssueComment,
                        ..comment()
                    },
                    "issue comment",
                ),
                (
                    "review_without_a_body",
                    PrComment {
                        kind: ItemKind::Review,
                        body: " \n".to_owned(),
                        ..comment()
                    },
                    "empty review",
                ),
            ] {
                assert_eq!(verdicts_of(comment), [expected], "case: {name}");
            }
        }

        #[test]
        fn bot_comment_without_a_new_finding_is_ignored() {
            for (name, comment, expected) in [
                (
                    "reply_to_the_owner",
                    PrComment {
                        author: CODERABBIT.to_owned(),
                        reply: true,
                        ..comment()
                    },
                    "CodeRabbit reply",
                ),
                (
                    "review_without_actionable_comments",
                    PrComment {
                        author: CODERABBIT.to_owned(),
                        kind: ItemKind::Review,
                        body: "nitpicks".to_owned(),
                        ..comment()
                    },
                    "CodeRabbit review without actionable comments",
                ),
                (
                    "approval_without_actionable_comments",
                    PrComment {
                        author: CODERABBIT.to_owned(),
                        kind: ItemKind::Review,
                        approved: true,
                        body: String::new(),
                        ..comment()
                    },
                    "CodeRabbit review without actionable comments",
                ),
                (
                    "finding_in_a_resolved_thread",
                    PrComment {
                        author: CODERABBIT.to_owned(),
                        resolved: true,
                        ..comment()
                    },
                    "resolved CodeRabbit thread",
                ),
            ] {
                assert_eq!(verdicts_of(comment), [expected], "case: {name}");
            }
        }

        fn verdicts_of(comment: PrComment) -> Vec<String> {
            classify(&[comment], "owner", &rules(), &[reviewers::coderabbit()])
                .into_iter()
                .map(|verdict| match verdict {
                    Verdict::Notified(_) => "notified".to_owned(),
                    Verdict::Ignored(reason) => reason.to_string(),
                })
                .collect()
        }
    }

    mod finding {
        use super::*;

        #[test]
        fn notified_finding_keeps_the_place_and_link_but_not_the_body() {
            let verdicts = classify(&[comment()], "owner", &rules(), &[]);

            assert_eq!(
                verdicts,
                [Verdict::Notified(Finding {
                    id: "inline:1".to_owned(),
                    kind: ItemKind::Inline,
                    bot: None,
                    label: "reviewer".to_owned(),
                    path: Some("src/pay.kt".to_owned()),
                    line: Some(42),
                    url: format!("{PR}#discussion_r1"),
                    created_at: "2026-10-23T00:00:00Z".to_owned(),
                    approval: None,
                })]
            );
        }

        #[test]
        fn coderabbit_findings_are_labeled_by_severity_and_people_by_name() {
            let comments = [
                PrComment {
                    id: "inline:1".to_owned(),
                    author: CODERABBIT.to_owned(),
                    body: "**🔒 Security & Privacy** | **🟠 Major** | **⚡ Quick win**".to_owned(),
                    ..comment()
                },
                PrComment {
                    id: "inline:2".to_owned(),
                    author: CODERABBIT.to_owned(),
                    body: "x".to_owned(),
                    ..comment()
                },
                PrComment {
                    id: "review:3".to_owned(),
                    kind: ItemKind::Review,
                    author: CODERABBIT.to_owned(),
                    body: "**Actionable comments posted: 1**".to_owned(),
                    ..comment()
                },
                PrComment {
                    id: "inline:4".to_owned(),
                    author: "tanaka".to_owned(),
                    ..comment()
                },
            ];
            let bots = [reviewers::coderabbit()];

            let verdicts = classify(&comments, "owner", &rules(), &bots);

            assert_eq!(
                labels(&verdicts),
                [
                    ("CodeRabbit・Major", None),
                    ("CodeRabbit", None),
                    ("CodeRabbit・レビュー", None),
                    ("tanaka", None),
                ]
            );
        }

        #[test]
        fn person_approval_is_labeled_and_records_whether_it_has_a_body() {
            let comments = [
                PrComment {
                    id: "review:1".to_owned(),
                    kind: ItemKind::Review,
                    approved: true,
                    body: "ok".to_owned(),
                    ..comment()
                },
                PrComment {
                    id: "review:2".to_owned(),
                    kind: ItemKind::Review,
                    approved: true,
                    body: " ".to_owned(),
                    ..comment()
                },
                PrComment {
                    id: "review:3".to_owned(),
                    kind: ItemKind::Review,
                    body: "why?".to_owned(),
                    ..comment()
                },
                PrComment {
                    id: "review:4".to_owned(),
                    kind: ItemKind::Review,
                    author: CODERABBIT.to_owned(),
                    approved: true,
                    body: "**Actionable comments posted: 1**".to_owned(),
                    ..comment()
                },
            ];
            let bots = [reviewers::coderabbit()];

            let verdicts = classify(&comments, "owner", &rules(), &bots);

            assert_eq!(
                labels(&verdicts),
                [
                    ("reviewer・Approve", Some(Approval::WithBody)),
                    ("reviewer・Approve", Some(Approval::WithoutBody)),
                    ("reviewer", None),
                    ("CodeRabbit・レビュー", None),
                ]
            );
        }

        #[test]
        fn configured_bot_is_read_like_coderabbit_and_an_unknown_bot_like_a_person() {
            let bots = [ReviewBot {
                id: "other".to_owned(),
                name: "OtherBot".to_owned(),
                login: "other-reviewer[bot]".to_owned(),
                status: None,
                actionable_review: None,
                no_actionable: None,
                severity: Some(reviewers::Severity::new(r"^\[(P[0-3])\]").expect("compile")),
            }];
            let comments = [
                PrComment {
                    id: "inline:1".to_owned(),
                    author: "other-reviewer[bot]".to_owned(),
                    body: "[P1] x".to_owned(),
                    ..comment()
                },
                PrComment {
                    id: "inline:2".to_owned(),
                    author: "other-reviewer[bot]".to_owned(),
                    reply: true,
                    ..comment()
                },
                PrComment {
                    id: "inline:3".to_owned(),
                    author: CODERABBIT.to_owned(),
                    reply: true,
                    ..comment()
                },
            ];

            let verdicts = classify(&comments, "owner", &rules(), &bots)
                .into_iter()
                .map(|verdict| match verdict {
                    Verdict::Notified(finding) => format!("{} {:?}", finding.label, finding.bot),
                    Verdict::Ignored(reason) => reason.to_string(),
                })
                .collect::<Vec<_>>();

            assert_eq!(
                verdicts,
                [
                    r#"OtherBot・P1 Some("other")"#,
                    "OtherBot reply",
                    "coderabbitai[bot] None",
                ]
            );
        }

        fn labels<'verdicts>(
            verdicts: &'verdicts [Verdict<'_>],
        ) -> Vec<(&'verdicts str, Option<Approval>)> {
            verdicts
                .iter()
                .map(|verdict| match verdict {
                    Verdict::Notified(finding) => (finding.label.as_str(), finding.approval),
                    Verdict::Ignored(_) => ("ignored", None),
                })
                .collect()
        }
    }

    const CODERABBIT: &str = "coderabbitai[bot]";

    fn rules() -> [IgnoreRule; 3] {
        [
            IgnoreRule {
                author: CODERABBIT.to_owned(),
                kind: Some(ItemKind::IssueComment),
                body_includes: None,
            },
            IgnoreRule {
                author: "github-actions[bot]".to_owned(),
                kind: Some(ItemKind::Inline),
                body_includes: Some("[phase-timing]".to_owned()),
            },
            IgnoreRule {
                author: "blacksmith-sh[bot]".to_owned(),
                kind: None,
                body_includes: None,
            },
        ]
    }

    fn comment() -> PrComment {
        PrComment {
            id: "inline:1".to_owned(),
            kind: ItemKind::Inline,
            author: "reviewer".to_owned(),
            url: format!("{PR}#discussion_r1"),
            created_at: "2026-10-23T00:00:00Z".to_owned(),
            body: "please rename".to_owned(),
            path: Some("src/pay.kt".to_owned()),
            line: Some(42),
            reply: false,
            resolved: false,
            approved: false,
        }
    }
}
