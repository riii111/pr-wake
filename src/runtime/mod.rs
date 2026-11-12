pub(crate) mod output;
#[expect(
    dead_code,
    reason = "the review commands reach it once pr-wake runs them"
)]
pub(crate) mod review;
pub(crate) mod termination;
pub(crate) mod watch;
