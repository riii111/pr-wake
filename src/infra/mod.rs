#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the review commands reach it once pr-wake runs them"
    )
)]
pub(crate) mod alerter;
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the review commands reach it once pr-wake runs them"
    )
)]
pub(crate) mod codex_app;
pub(crate) mod command;
pub(crate) mod gh;
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the review commands reach it once pr-wake runs them"
    )
)]
pub(crate) mod git;
pub(crate) mod harnexus;
pub(crate) mod record;
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the review commands reach it once pr-wake runs them"
    )
)]
pub(crate) mod review_record;
pub(crate) mod stop;
pub(crate) mod t3;
pub(crate) mod watch_record;
