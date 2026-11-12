use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[derive(Debug, Clone, Default)]
pub(crate) struct StopRequest {
    // A signal handler can set an atomic but cannot wake a condition variable, so waits poll this.
    requested: Arc<AtomicBool>,
}

impl StopRequest {
    // Called from a signal handler, so this must stay a single atomic store.
    pub(crate) fn request(&self) {
        self.requested.store(true, Ordering::Relaxed);
    }

    #[must_use]
    pub(crate) fn is_requested(&self) -> bool {
        self.requested.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_reaches_every_clone() {
        let stop = StopRequest::default();
        let waiter = stop.clone();
        assert!(!waiter.is_requested());

        stop.request();

        assert!(waiter.is_requested());
    }
}
