mod plist;

use std::{ffi::OsString, path::Path, time::Duration};

pub(crate) use self::plist::{AgentPlist, ProcessType, agent_path};
use super::{
    command::{self, CommandOutput, ExternalCommand},
    stop::StopRequest,
};

// One launchctl run on an agent of the logged-in user's GUI session.
pub(crate) struct Launchctl {
    arguments: Vec<OsString>,
}

const PROGRAM: &str = "launchctl";
// `bootout` waits for the agent to exit, which the plist's ExitTimeOut bounds well under this.
const TIMEOUT: Duration = Duration::from_secs(60);

impl Launchctl {
    #[must_use]
    pub(crate) fn bootstrap(plist: &Path) -> Self {
        Self::of(["bootstrap".into(), domain().into(), plist.into()])
    }

    // `-k` stops the running agent first, so it starts again on the program at its path.
    #[must_use]
    pub(crate) fn kickstart(label: &str) -> Self {
        Self::of(["kickstart".into(), "-k".into(), service(label).into()])
    }

    #[must_use]
    pub(crate) fn bootout(label: &str) -> Self {
        Self::of(["bootout".into(), service(label).into()])
    }

    #[must_use]
    pub(crate) fn command_line(&self) -> String {
        command::command_line(PROGRAM, &self.arguments)
    }

    pub(crate) fn run(&self) -> Result<CommandOutput, String> {
        ExternalCommand::new(PROGRAM, TIMEOUT)
            .arguments(&self.arguments)
            .run(&StopRequest::default())
            .map_err(|error| error.to_string())
    }

    fn of<const N: usize>(arguments: [OsString; N]) -> Self {
        Self {
            arguments: arguments.into(),
        }
    }
}

#[must_use]
pub(crate) fn is_loaded(label: &str) -> bool {
    Launchctl::of(["print".into(), service(label).into()])
        .run()
        .is_ok_and(|output| output.code == 0)
}

fn service(label: &str) -> String {
    format!("{}/{label}", domain())
}

fn domain() -> String {
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    format!("gui/{uid}")
}
