use serde::{Deserialize, Serialize};

use super::state::{FailedCheck, PrSummary};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PrRef {
    // `owner/name`, which GitHub matches without case.
    pub(crate) repo: String,
    pub(crate) number: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ItemKind {
    Inline,
    Review,
    IssueComment,
}

// What the full read of a PR gives the watch.
#[derive(Debug, Clone)]
pub(crate) struct PrDetail {
    pub(crate) summary: PrSummary,
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
