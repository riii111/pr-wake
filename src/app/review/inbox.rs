use std::collections::HashSet;

use super::request::pr_key;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InboxThread {
    pub(crate) id: String,
    pub(crate) reason: String,
    pub(crate) pr: Option<(String, u64)>,
}

// Someone asking the owner directly is worth the interruption, so mentions stay in the inbox.
const KEPT_REASONS: [&str; 2] = ["mention", "team_mention"];

#[must_use]
pub(crate) fn threads_to_clear<'thread>(
    threads: &'thread [InboxThread],
    tracked: &HashSet<String>,
) -> Vec<&'thread InboxThread> {
    threads
        .iter()
        .filter(|thread| {
            !KEPT_REASONS.contains(&thread.reason.as_str())
                && thread
                    .pr
                    .as_ref()
                    .is_some_and(|(repo, number)| tracked.contains(&pr_key(repo, *number)))
        })
        .collect()
}
