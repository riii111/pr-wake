mod api;
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the review commands reach it once pr-wake runs them"
    )
)]
pub(crate) mod rest;

pub(crate) use api::GhApi;
