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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threads_of_tracked_prs_are_cleared_but_mentions() {
        let tracked = HashSet::from(["acme/app#12".to_owned()]);
        for (name, reason, pr, cleared) in [
            (
                "review_requested",
                "review_requested",
                Some(("Acme/App", 12)),
                true,
            ),
            ("comment", "comment", Some(("acme/app", 12)), true),
            ("mention", "mention", Some(("acme/app", 12)), false),
            (
                "team_mention",
                "team_mention",
                Some(("acme/app", 12)),
                false,
            ),
            (
                "untracked_pr",
                "review_requested",
                Some(("acme/app", 13)),
                false,
            ),
            ("not_a_pr", "subscribed", None, false),
        ] {
            let threads = [InboxThread {
                id: "1".to_owned(),
                reason: reason.to_owned(),
                pr: pr.map(|(repo, number)| (repo.to_owned(), number)),
            }];

            let to_clear = threads_to_clear(&threads, &tracked);

            assert_eq!(to_clear.len(), usize::from(cleared), "case: {name}");
        }
    }
}
