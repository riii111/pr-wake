use std::{env, ffi::OsStr, path::Path, process::ExitCode};

use clap::{Args, Parser, Subcommand};

use crate::runtime::output;

mod review;
mod watch;

use review::ReviewCommand;
use watch::WatchCommand;

#[derive(Debug, Parser)]
#[command(name = NAME, version, about, arg_required_else_help = true)]
struct Cli {
    #[command(subcommand)]
    feature: Feature,
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
enum Feature {
    #[command(
        subcommand,
        about = "Notify the review requests for you and start an AI review"
    )]
    Review(ReviewCommand),
    #[command(subcommand, about = "Tell AI workers what happens on their PRs")]
    Watch(WatchCommand),
}

#[derive(Debug, PartialEq, Eq, Args)]
struct LogFlags {
    #[arg(short = 'f', long, help = "Keep printing what is written to the log")]
    follow: bool,
}

// harnexus-crew runs `pr-wake-engine watch` for an old harnexus-task watch agent, so that name runs only the watch.
#[derive(Debug, Parser)]
#[command(
    name = ENGINE,
    version,
    about = "pr-wake's PR watch under the name harnexus-crew starts",
    arg_required_else_help = true
)]
struct EngineCli {
    #[command(subcommand)]
    command: EngineCommand,
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
enum EngineCommand {
    #[command(about = "Watch the linked PRs, as `pr-wake watch run` does")]
    Watch,
}

const NAME: &str = "pr-wake";
const ENGINE: &str = "pr-wake-engine";

// Exit 0 is a success, with a watch command's JSON as one line on stdout.
// Exit 1 is a failure with its reason on stderr, and 2 a usage error.
pub(crate) fn run() -> ExitCode {
    if invoked_as_engine() {
        return match EngineCli::try_parse() {
            Ok(EngineCli {
                command: EngineCommand::Watch,
            }) => finish(ENGINE, watch::run(WatchCommand::Run)),
            Err(error) => usage_error(&error),
        };
    }

    match Cli::try_parse() {
        Ok(Cli { feature }) => finish(
            NAME,
            match feature {
                Feature::Review(command) => review::run(command).map(|()| None),
                Feature::Watch(command) => watch::run(command),
            },
        ),
        // Parsed without clap's own exit, so the process ends through `main` like every command.
        Err(error) => usage_error(&error),
    }
}

fn finish(name: &str, result: Result<Option<String>, String>) -> ExitCode {
    match result {
        Ok(Some(printed)) => {
            output::write_stdout(&format!("{printed}\n"));
            ExitCode::SUCCESS
        }
        Ok(None) => ExitCode::SUCCESS,
        Err(failure) => {
            output::write_stderr(&format!("{name}: {failure}\n"));
            ExitCode::FAILURE
        }
    }
}

fn invoked_as_engine() -> bool {
    env::args_os()
        .next()
        .is_some_and(|invoked| Path::new(&invoked).file_name() == Some(OsStr::new(ENGINE)))
}

fn usage_error(error: &clap::Error) -> ExitCode {
    let _ = error.print();
    ExitCode::from(u8::try_from(error.exit_code()).unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use std::iter;

    use super::*;

    pub(super) const PR: &str = "https://github.com/org/pay/pull/7";

    #[test]
    fn wrong_command_lines_are_usage_errors() {
        for (name, arguments) in [
            ("sender_with_an_argument", &["watch", "sender", "extra"][..]),
            ("link_of_two_prs", &["watch", "link", PR, PR]),
            ("release_without_a_pr", &["watch", "release"]),
            ("unknown_watch_command", &["watch", "unknown"]),
            ("smoke_without_a_pr", &["review", "smoke"]),
        ] {
            let parsed = Cli::try_parse_from(command_line(arguments));

            assert_eq!(
                parsed.map_err(|error| error.exit_code()).err(),
                Some(2),
                "case: {name}"
            );
        }
    }

    #[test]
    fn engine_name_runs_only_the_watch() {
        let watch = EngineCli::try_parse_from([ENGINE, "watch"]);
        let flag = EngineCli::try_parse_from([ENGINE, "watch", "--status"]);

        assert_eq!(
            watch.map(|cli| cli.command).ok(),
            Some(EngineCommand::Watch)
        );
        assert_eq!(flag.map_err(|error| error.exit_code()).err(), Some(2));
    }

    pub(super) fn command_line<'arguments>(
        arguments: &[&'arguments str],
    ) -> impl Iterator<Item = &'arguments str> {
        iter::once(NAME).chain(arguments.iter().copied())
    }
}
