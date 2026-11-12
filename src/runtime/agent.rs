use std::{
    env, fs, io,
    path::{self, Path, PathBuf},
};

use crate::{
    infra::{
        launchctl::{self, AgentPlist, Launchctl, ProcessType},
        tail::Tail,
    },
    runtime::{Env, output},
};

// A pr-wake feature kept running as a per-user LaunchAgent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Agent {
    label: &'static str,
    // The pr-wake subcommand launchd runs, named in hints and in the log's file name.
    command: &'static str,
    process_type: ProcessType,
    // Said at uninstall, so the owner knows what is left to remove by hand.
    kept: &'static str,
}

pub(crate) const REVIEW: Agent = Agent {
    label: "io.github.riii111.pr-wake.review",
    command: "review",
    process_type: ProcessType::Interactive,
    kept: "config, state, logs and review worktrees are kept",
};

pub(crate) const WATCH: Agent = Agent {
    label: "io.github.riii111.pr-wake.watch",
    command: "watch",
    process_type: ProcessType::Background,
    kept: "config, state and logs are kept",
};

// The agents of the tools pr-wake replaced, removed at install so two never send the same news.
pub(crate) const LEGACY_REVIEW: &str = "io.github.riii111.review-notifier";
pub(crate) const LEGACY_WATCH: &str = "io.github.riii111.pr-watch";

// launchd gives an agent none of the installing shell's environment, so these are written into the
// plist as install found them, sorted by name.
const PASSED_ON: [(&str, Passed); 6] = [
    ("HARNEXUS_CALL_SOCKET", Passed::AsSet),
    ("HARNEXUS_STATE_PATH", Passed::AsSet),
    ("HARNEXUS_T3_BIN", Passed::AsSet),
    ("T3CODE_HOME", Passed::WhenAbsolute),
    ("XDG_CONFIG_HOME", Passed::WhenAbsolute),
    ("XDG_STATE_HOME", Passed::WhenAbsolute),
];

// pr-wake ignores a relative home directory, and the commands an agent runs would read it against
// another working directory, so a home is passed only when absolute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Passed {
    AsSet,
    WhenAbsolute,
}

// Replaces a running agent, so it starts on the program and environment of this install.
pub(crate) fn install(
    agent: Agent,
    home: &Path,
    user: Option<&str>,
    env: Env<'_>,
) -> Result<(), String> {
    let plist = plist_of(agent, home, user, &program()?, env)?;
    stop_service(agent.label)?;
    let path = plist_path(agent.label, home);
    for directory in [path.parent(), agent.log(home).parent()]
        .into_iter()
        .flatten()
    {
        fs::create_dir_all(directory)
            .map_err(|error| format!("cannot create {}: {error}", directory.display()))?;
    }
    fs::write(&path, plist).map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    start(agent, home)
}

pub(crate) fn uninstall(agent: Agent, home: &Path) -> Result<(), String> {
    remove(agent.label, home)?;
    output::write_stdout(&format!("{}\n", agent.kept));
    Ok(())
}

// Only an agent whose plist is still installed is stopped and removed.
pub(crate) fn remove_legacy(label: &str, home: &Path) -> Result<(), String> {
    if plist_path(label, home).exists() {
        remove(label, home)
    } else {
        Ok(())
    }
}

pub(crate) fn start(agent: Agent, home: &Path) -> Result<(), String> {
    let plist = plist_path(agent.label, home);
    if !plist.exists() {
        return Err(format!(
            "{} not found; run `pr-wake {} install`",
            plist.display(),
            agent.command
        ));
    }
    if launchctl::is_loaded(agent.label) {
        run(&Launchctl::kickstart(agent.label))
    } else {
        run(&Launchctl::bootstrap(&plist))
    }
}

pub(crate) fn stop(agent: Agent) -> Result<(), String> {
    stop_service(agent.label)
}

pub(crate) fn print_loaded(agent: Agent) {
    let state = if launchctl::is_loaded(agent.label) {
        "loaded"
    } else {
        "not loaded"
    };
    output::write_stdout(&format!("{}: {state}\n", agent.label));
}

pub(crate) fn logs(agent: Agent, home: &Path, follow: bool) -> Result<(), String> {
    let log = agent.log(home);
    if !log.exists() {
        return Err(format!("{} not found", log.display()));
    }
    let tail = Tail::of(&log, follow);
    output::write_stdout(&format!("$ {}\n", tail.command_line()));
    tail.run()
}

// The user that names the nix-darwin profile on the agent's PATH.
#[must_use]
pub(crate) fn user() -> Option<String> {
    env::var("USER").ok().filter(|user| !user.is_empty())
}

impl Agent {
    fn log(self, home: &Path) -> PathBuf {
        home.join("Library/Logs/pr-wake")
            .join(format!("{}.log", self.command))
    }
}

fn plist_of(
    agent: Agent,
    home: &Path,
    user: Option<&str>,
    program: &str,
    env: Env<'_>,
) -> Result<String, String> {
    let environment = PASSED_ON
        .into_iter()
        .filter_map(|(name, passed)| {
            let value = env(name).filter(|value| {
                !value.is_empty() && (passed == Passed::AsSet || Path::new(value).is_absolute())
            })?;
            Some(
                value
                    .into_string()
                    .map(|value| (name, value))
                    .map_err(|_| format!("{name} is not UTF-8, so it cannot go into the plist")),
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(AgentPlist {
        label: agent.label,
        program: &[
            program.to_owned(),
            agent.command.to_owned(),
            "run".to_owned(),
        ],
        home,
        user,
        environment: &environment,
        log: &agent.log(home),
        process_type: agent.process_type,
    }
    .render())
}

// The path this run was started at, without resolving links, so a reinstall at that path is what
// the agent runs next.
fn program() -> Result<String, String> {
    let program = env::current_exe()
        .and_then(path::absolute)
        .map_err(|error| format!("cannot find this program's path: {error}"))?;
    program
        .into_os_string()
        .into_string()
        .map_err(|_| "this program's path is not UTF-8, so it cannot go into the plist".to_owned())
}

fn remove(label: &str, home: &Path) -> Result<(), String> {
    stop_service(label)?;
    let path = plist_path(label, home);
    match fs::remove_file(&path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            Err(format!("cannot remove {}: {error}", path.display()))
        }
        _ => Ok(()),
    }
}

fn stop_service(label: &str) -> Result<(), String> {
    if !launchctl::is_loaded(label) {
        output::write_stdout(&format!("{label} is not running\n"));
        return Ok(());
    }
    run(&Launchctl::bootout(label))
}

fn run(launchctl: &Launchctl) -> Result<(), String> {
    output::write_stdout(&format!("$ {}\n", launchctl.command_line()));
    let ran = launchctl.run()?;
    output::write_stdout(&String::from_utf8_lossy(&ran.stdout));
    output::write_stderr(&String::from_utf8_lossy(&ran.stderr));
    if ran.code == 0 {
        Ok(())
    } else {
        Err(format!("launchctl exited with {}", ran.code))
    }
}

fn plist_path(label: &str, home: &Path) -> PathBuf {
    home.join("Library/LaunchAgents")
        .join(format!("{label}.plist"))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;

    const PROGRAM: &str = "/Users/me/.local/bin/pr-wake";

    #[test]
    fn agents_run_their_feature_with_their_own_plist_log_and_process_type() {
        for (agent, plist, log, run, process) in [
            (
                REVIEW,
                "/Users/me/Library/LaunchAgents/io.github.riii111.pr-wake.review.plist",
                "/Users/me/Library/Logs/pr-wake/review.log",
                "<string>/Users/me/.local/bin/pr-wake</string>\n\t\t<string>review</string>\n\t\t<string>run</string>\n",
                "<key>ProcessType</key>\n\t<string>Interactive</string>",
            ),
            (
                WATCH,
                "/Users/me/Library/LaunchAgents/io.github.riii111.pr-wake.watch.plist",
                "/Users/me/Library/Logs/pr-wake/watch.log",
                "<string>/Users/me/.local/bin/pr-wake</string>\n\t\t<string>watch</string>\n\t\t<string>run</string>\n",
                "<key>ProcessType</key>\n\t<string>Background</string>",
            ),
        ] {
            let home = Path::new("/Users/me");

            let rendered =
                plist_of(agent, home, Some("me"), PROGRAM, &|_| None).expect("render the plist");

            assert_eq!(
                plist_path(agent.label, home),
                Path::new(plist),
                "case: {}",
                agent.command
            );
            assert_eq!(agent.log(home), Path::new(log), "case: {}", agent.command);
            for want in [run, process, &format!("<string>{log}</string>")] {
                assert!(
                    rendered.contains(want),
                    "case: {}: no {want:?} in\n{rendered}",
                    agent.command
                );
            }
        }
    }

    #[test]
    fn installed_agent_runs_with_the_paths_and_sockets_install_had() {
        let set = |name: &str| {
            let value = match name {
                "XDG_CONFIG_HOME" => "/Users/me/conf",
                "XDG_STATE_HOME" => "/Users/me/st",
                "HARNEXUS_CALL_SOCKET" => "on",
                "HARNEXUS_STATE_PATH" => "/Users/me/harnexus/threads.json",
                "HARNEXUS_T3_BIN" => "/opt/t3/bin/t3",
                "T3CODE_HOME" => "/Users/me/.t3",
                _ => return None,
            };
            Some(OsString::from(value))
        };

        let rendered = plist_of(WATCH, Path::new("/Users/me"), Some("me"), PROGRAM, &set)
            .expect("render the plist");

        assert!(
            rendered.contains(concat!(
                "\t\t<key>LANG</key>\n\t\t<string>en_US.UTF-8</string>\n",
                "\t\t<key>HARNEXUS_CALL_SOCKET</key>\n\t\t<string>on</string>\n",
                "\t\t<key>HARNEXUS_STATE_PATH</key>\n\t\t<string>/Users/me/harnexus/threads.json</string>\n",
                "\t\t<key>HARNEXUS_T3_BIN</key>\n\t\t<string>/opt/t3/bin/t3</string>\n",
                "\t\t<key>T3CODE_HOME</key>\n\t\t<string>/Users/me/.t3</string>\n",
                "\t\t<key>XDG_CONFIG_HOME</key>\n\t\t<string>/Users/me/conf</string>\n",
                "\t\t<key>XDG_STATE_HOME</key>\n\t\t<string>/Users/me/st</string>\n",
                "\t</dict>\n",
            )),
            "{rendered}"
        );
    }

    #[test]
    fn variable_unset_empty_or_a_relative_home_is_not_passed_on() {
        let set = |name: &str| match name {
            "XDG_CONFIG_HOME" => Some(OsString::from("relative")),
            "XDG_STATE_HOME" => Some(OsString::new()),
            _ => None,
        };

        let rendered = plist_of(REVIEW, Path::new("/Users/me"), Some("me"), PROGRAM, &set)
            .expect("render the plist");

        assert!(
            rendered.contains("<string>en_US.UTF-8</string>\n\t</dict>\n"),
            "{rendered}"
        );
    }
}
