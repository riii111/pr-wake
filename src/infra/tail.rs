use std::{
    ffi::OsString,
    path::Path,
    process::{Command, Stdio},
};

use super::command;

pub(crate) struct Tail {
    arguments: Vec<OsString>,
}

const PROGRAM: &str = "tail";
const LINES: &str = "200";

impl Tail {
    #[must_use]
    pub(crate) fn of(log: &Path, follow: bool) -> Self {
        let mut arguments = vec![OsString::from("-n"), OsString::from(LINES)];
        if follow {
            arguments.push(OsString::from("-F"));
        }
        arguments.push(log.into());
        Self { arguments }
    }

    #[must_use]
    pub(crate) fn command_line(&self) -> String {
        command::command_line(PROGRAM, &self.arguments)
    }

    // The terminal gets the output and its Ctrl-C ends a follow, so no timeout or own process group is given.
    pub(crate) fn run(&self) -> Result<(), String> {
        let status = Command::new(PROGRAM)
            .args(&self.arguments)
            .stdin(Stdio::null())
            .status()
            .map_err(|error| format!("cannot run tail: {error}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("tail ended with {status}"))
        }
    }
}
