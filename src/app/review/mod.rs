pub(crate) mod batch;
pub(crate) mod config;
pub(crate) mod inbox;
pub(crate) mod notice;
pub(crate) mod prompt;
pub(crate) mod request;
pub(crate) mod selection;
pub(crate) mod state;

use std::{
    path::{Path, PathBuf},
    sync::LazyLock,
};

use regex::Regex;

use self::{notice::Size, request::LiveRequest};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PullRequest {
    pub(crate) repo: String,
    pub(crate) number: u64,
    pub(crate) title: String,
    pub(crate) url: String,
    pub(crate) head_sha: String,
    pub(crate) base_ref: String,
}

// What one read of a pull request tells, so no later step reads it again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PullDetails {
    pub(crate) pr: PullRequest,
    pub(crate) author: Option<String>,
    pub(crate) live: LiveRequest,
    // The requested users who are not bots, as bots never count as reviewers.
    pub(crate) requested: Vec<String>,
    pub(crate) size: Size,
}

static REPOSITORY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$").expect("the repository pattern is valid")
});

#[must_use]
pub(crate) fn is_repo_name(text: &str) -> bool {
    REPOSITORY.is_match(text)
}

// Accepts `owner/repo#123` or a pull request URL on github.com, whatever tab it shows.
pub(crate) fn parse_pr_ref(text: &str) -> Result<(String, u64), String> {
    if let Some(path) = github_path(text) {
        let parts = path.trim_matches('/').split('/').collect::<Vec<_>>();
        return match parts.as_slice() {
            [owner, name, "pull", number, ..] => checked_ref(&format!("{owner}/{name}"), number),
            _ => Err(format!("not a pull request URL: {text}")),
        };
    }
    let Some((repo, number)) = text.split_once('#') else {
        return Err(format!(
            "expected owner/repo#number or a pull request URL: {text}"
        ));
    };
    checked_ref(repo, number)
}

#[must_use]
pub(crate) fn worktree_path(root: &Path, pr: &PullRequest) -> PathBuf {
    let (owner, name) = pr.repo.split_once('/').unwrap_or((&pr.repo, ""));
    let head = pr.head_sha.get(..12).unwrap_or(&pr.head_sha);
    root.join(owner)
        .join(name)
        .join(format!("pr-{}-{head}", pr.number))
}

#[must_use]
pub(crate) fn with_worktree(prompt: &str, worktree: &Path, pr: &PullRequest) -> String {
    format!(
        "{prompt}\n\nコードは{}（{}に固定した専用worktree）を読んでください。branchやcheckoutは変更しないでください。",
        worktree.display(),
        pr.head_sha
    )
}

#[must_use]
pub(crate) fn with_request(prompt: &str, key: &str) -> String {
    // One chat holds a PR's every review, so the report finds this request's turn by the key, put first to survive a cut-short read.
    format!(
        "pr-wakeの依頼ID: {key}\n\n{prompt}\n\nレビュー結果はこのレビューチャットで回答してください。調査を分担する場合は、このチャットのサブエージェントを使ってください。"
    )
}

#[must_use]
pub(crate) fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn github_path(text: &str) -> Option<&str> {
    let scheme = text.get(..8)?;
    if !scheme.eq_ignore_ascii_case("https://") {
        return None;
    }
    let rest = &text[8..];
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, rest) = rest.split_at(end);
    let host = authority.rsplit('@').next().unwrap_or(authority);
    if !host.eq_ignore_ascii_case("github.com") {
        return None;
    }
    Some(&rest[..rest.find(['?', '#']).unwrap_or(rest.len())])
}

fn checked_ref(repo: &str, number: &str) -> Result<(String, u64), String> {
    if !is_repo_name(repo) {
        return Err(format!("not owner/repo: {repo:?}"));
    }
    match number.parse::<u64>() {
        Ok(parsed) if parsed > 0 => Ok((repo.to_owned(), parsed)),
        _ => Err(format!("not a pull request number: {number:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pr_ref_is_read_from_a_url_or_a_short_name() {
        for (name, text, expected) in [
            ("short", "acme/app#12", Some(("acme/app", 12))),
            (
                "url",
                "https://github.com/acme/app/pull/12",
                Some(("acme/app", 12)),
            ),
            (
                "url_with_tab_and_fragment",
                "https://GitHub.com/acme/app/pull/12/files#diff",
                Some(("acme/app", 12)),
            ),
            ("issue_url", "https://github.com/acme/app/issues/12", None),
            ("other_host", "https://example.com/acme/app/pull/12", None),
            ("zero", "acme/app#0", None),
            ("no_number", "acme/app", None),
            ("bad_repo", "acme#12", None),
        ] {
            let parsed = parse_pr_ref(text);

            let expected = expected.map(|(repo, number)| (repo.to_owned(), number));
            assert_eq!(parsed.ok(), expected, "case: {name}");
        }
    }
}
