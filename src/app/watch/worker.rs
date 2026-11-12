use jiff::Timestamp;

use super::{SendOutcome, WorkerView, state::Delivered};

// What the watch needs from whoever works on a PR; each app a worker runs in supplies its own.
pub(crate) trait Workers {
    // The chat to send from, or why nothing can send to the worker yet.
    fn sender_of(&mut self, worker: &str) -> Result<String, String>;

    // `None` while the worker cannot be asked, as while its app is closed; the next cycle asks
    // again.
    fn look(
        &mut self,
        sender: &str,
        worker: &str,
        awaited: Option<&Delivered>,
        now: Timestamp,
    ) -> Option<WorkerView>;

    fn send(&mut self, sender: &str, worker: &str, prompt: &str) -> Sent;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Sent {
    pub(crate) outcome: SendOutcome,
    // The worker's own word for the outcome, which the log keeps.
    pub(crate) reported: String,
}

// A worker in T3 Code is linked with this before its thread, so a link tells which app reaches it.
pub(crate) const T3_WORKER: &str = "t3:";

// The t3 CLI sends as the person using T3 Code, so every message to a T3 Code worker is recorded
// as from it.
pub(crate) const T3_SENDER: &str = "t3";

const NO_SENDER: &str =
    "no chat is registered to send from: run pr-wake watch sender in a chat kept for it";

#[must_use]
pub(crate) fn is_t3_worker(worker: &str) -> bool {
    worker.starts_with(T3_WORKER)
}

// Every message to a Codex worker comes from the one chat registered as the sender, so the watch
// depends on no other command's records.
pub(crate) fn sender_for(
    registered: Result<Option<String>, String>,
    worker: &str,
) -> Result<String, String> {
    match registered {
        Err(error) => Err(format!("cannot read the registered sender: {error}")),
        Ok(None) => Err(NO_SENDER.to_owned()),
        // A thread cannot message itself, so the registered chat is never sent to from itself.
        Ok(Some(sender)) if sender == worker => Err(
            "the registered sender is this chat itself: run pr-wake watch sender in another chat"
                .to_owned(),
        ),
        Ok(Some(sender)) => Ok(sender),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sender_is_the_registered_chat_unless_it_is_missing_or_the_worker() {
        for (name, registered, expected) in [
            (
                "registered",
                Ok(Some("sender-chat")),
                Ok("sender-chat".to_owned()),
            ),
            ("none_registered", Ok(None), Err(NO_SENDER.to_owned())),
            (
                "unreadable",
                Err("bad JSON"),
                Err("cannot read the registered sender: bad JSON".to_owned()),
            ),
            (
                "worker_itself",
                Ok(Some("worker")),
                Err("the registered sender is this chat itself: run pr-wake watch sender in another chat".to_owned()),
            ),
        ] {
            let registered = registered
                .map(|sender| sender.map(str::to_owned))
                .map_err(str::to_owned);

            assert_eq!(sender_for(registered, "worker"), expected, "case: {name}");
        }
    }

    #[test]
    fn t3_worker_is_told_apart_by_its_prefix() {
        assert!(is_t3_worker("t3:mcp:w1"));
        assert!(!is_t3_worker("019a-codex"));
    }
}
