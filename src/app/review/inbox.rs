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
        let thread = |id: &str, reason: &str, pr: Option<(&str, u64)>| InboxThread {
            id: id.to_owned(),
            reason: reason.to_owned(),
            pr: pr.map(|(repo, number)| (repo.to_owned(), number)),
        };
        let threads = [
            thread("1", "review_requested", Some(("acme/app", 12))),
            thread("2", "mention", Some(("acme/app", 12))),
            thread("3", "review_requested", Some(("acme/app", 13))),
            thread("4", "team_mention", Some(("acme/app", 12))),
            thread("5", "comment", Some(("Acme/App", 12))),
            thread("6", "subscribed", None),
        ];

        let cleared = threads_to_clear(&threads, &HashSet::from(["acme/app#12".to_owned()]));

        let ids = cleared
            .iter()
            .map(|thread| thread.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["1", "5"]);
    }
}
