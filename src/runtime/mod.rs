use std::ffi::OsString;

pub(crate) mod agent;
pub(crate) mod output;
pub(crate) mod review;
pub(crate) mod termination;
pub(crate) mod watch;

pub(crate) type Env<'env> = &'env dyn Fn(&str) -> Option<OsString>;
