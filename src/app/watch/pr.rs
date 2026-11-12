use std::{
    fmt::{self, Display, Formatter},
    sync::LazyLock,
};

use regex::Regex;
use serde::{Deserialize, Serialize};

use super::{
    reviewers::ReviewBot,
    state::{FailedCheck, PrSummary},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PrRef {
    // `owner/name`, which GitHub matches without case.
    pub(crate) repo: String,
    pub(crate) number: u64,
}

// A PR named by its GitHub URL, which the watch records in this one form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GithubPr {
    pub(crate) pr: PrRef,
    pub(crate) url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PrLink {
    pub(crate) pr: PrRef,
    pub(crate) url: String,
    pub(crate) worker: String,
    pub(crate) linked_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ItemKind {
    Inline,
    Review,
    IssueComment,
}

#[derive(Debug, Clone)]
pub(crate) struct PrDetail {
    pub(crate) summary: PrSummary,
    pub(crate) url: String,
    pub(crate) title: String,
    pub(crate) failed_required: Vec<FailedCheck>,
    pub(crate) comments: Vec<PrComment>,
    // The ids of the bots whose PR comment says they found nothing.
    pub(crate) no_actionable: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct PrComment {
    pub(crate) id: String,
    pub(crate) kind: ItemKind,
    pub(crate) author: String,
    pub(crate) url: String,
    pub(crate) created_at: String,
    pub(crate) body: String,
    pub(crate) path: Option<String>,
    pub(crate) line: Option<u64>,
    pub(crate) reply: bool,
    pub(crate) resolved: bool,
    pub(crate) approved: bool,
}

// The config is read again each cycle, so the review bots are passed on every read.
pub(crate) trait GitHub {
    // One summary per PR, in their order; `None` for a PR the account cannot see.
    fn summaries(
        &mut self,
        account: &str,
        prs: &[PrRef],
        bots: &[ReviewBot],
    ) -> Result<Vec<Option<PrSummary>>, ReadError>;

    fn detail(
        &mut self,
        account: &str,
        pr: &PrRef,
        bots: &[ReviewBot],
    ) -> Result<PrDetail, ReadError>;

    // The open PRs the account wrote in the repository.
    fn open_pulls(&mut self, account: &str, repo: &str) -> Result<Vec<PrRef>, ReadError>;
}

// The message may name a path or what a command printed, so the log records the kind alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReadError {
    pub(crate) kind: ReadErrorKind,
    pub(crate) message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReadErrorKind {
    AuthFailed,
    Unreachable,
    Rejected,
}

// The watch reaches GitHub only, so a PR on another host is named but never linked.
static GITHUB_PR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^https://github\.com/([A-Za-z0-9_.-]+)/([A-Za-z0-9_.-]+)/pull/([1-9][0-9]*)(?:[/?#]\S*)?$",
    )
    .expect("the GitHub PR pattern is valid")
});

pub(crate) fn github_pr(value: &str) -> Option<GithubPr> {
    let parts = GITHUB_PR.captures(value)?;
    let (owner, name) = (parts.get(1)?.as_str(), parts.get(2)?.as_str());
    let number = parts.get(3)?.as_str().parse().ok()?;
    Some(GithubPr {
        pr: PrRef {
            repo: format!("{owner}/{name}"),
            number,
        },
        url: format!("https://github.com/{owner}/{name}/pull/{number}"),
    })
}

impl PrRef {
    #[must_use]
    pub(crate) fn is_same(&self, other: &Self) -> bool {
        self.number == other.number && self.repo.to_lowercase() == other.repo.to_lowercase()
    }
}

impl ReadErrorKind {
    // The names the TypeScript watch logged, so a search over old and new logs finds both.
    #[must_use]
    pub(crate) const fn tag(self) -> &'static str {
        match self {
            Self::AuthFailed => "GithubAuthFailed",
            Self::Unreachable => "GithubUnreachable",
            Self::Rejected => "GithubRejected",
        }
    }
}

impl Display for ReadError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_pr_url_is_read_into_its_recorded_form() {
        for (name, value, expected) in [
            (
                "plain",
                "https://github.com/org/pay/pull/7",
                Some(("org/pay", 7, "https://github.com/org/pay/pull/7")),
            ),
            (
                "files_tab",
                "https://github.com/Org/Pay/pull/7/files",
                Some(("Org/Pay", 7, "https://github.com/Org/Pay/pull/7")),
            ),
            (
                "anchor",
                "https://github.com/org/pay/pull/7#discussion_r1",
                Some(("org/pay", 7, "https://github.com/org/pay/pull/7")),
            ),
            ("number_only", "9", None),
            ("issue", "https://github.com/org/pay/issues/7", None),
            ("other_host", "https://example.com/org/pay/pull/7", None),
            ("leading_zero", "https://github.com/org/pay/pull/07", None),
            (
                "trailing_space",
                "https://github.com/org/pay/pull/7 x",
                None,
            ),
        ] {
            let parsed = github_pr(value);

            let expected = expected.map(|(repo, number, url)| GithubPr {
                pr: PrRef {
                    repo: repo.to_owned(),
                    number,
                },
                url: url.to_owned(),
            });
            assert_eq!(parsed, expected, "case: {name}");
        }
    }

    #[test]
    fn same_pr_ignores_the_case_of_the_repository() {
        let pr = PrRef {
            repo: "Org/Pay".to_owned(),
            number: 7,
        };

        assert!(pr.is_same(&PrRef {
            repo: "org/pay".to_owned(),
            number: 7,
        }));
        assert!(!pr.is_same(&PrRef {
            repo: "org/pay".to_owned(),
            number: 8,
        }));
    }
}
