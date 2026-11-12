use std::{
    env,
    ffi::OsStr,
    path::{Path, PathBuf},
    process::ExitCode,
};

use clap::{Args, Parser, Subcommand};

mod app;
mod infra;
mod runtime;

use runtime::{
    agent, output,
    review::{self, ReviewEnv, Smoke},
    watch::{self, Mode},
};

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

#[derive(Debug, PartialEq, Eq, Subcommand)]
enum ReviewCommand {
    #[command(about = "Watch for review requests, as the LaunchAgent does")]
    Run(ConfigFlag),
    #[command(about = "Show the LaunchAgent and the latest requests")]
    Status,
    #[command(about = "Check the config, the commands and GitHub")]
    Check(ConfigFlag),
    #[command(about = "Take one PR through the notification and the launch")]
    Smoke(SmokeFlags),
    #[command(about = "Check the config, then start the LaunchAgent")]
    Install,
    #[command(about = "Stop and remove the LaunchAgent")]
    Uninstall,
    #[command(about = "Start the installed LaunchAgent")]
    Start,
    #[command(about = "Stop the LaunchAgent")]
    Stop,
    #[command(about = "Show the LaunchAgent's log")]
    Logs(LogFlags),
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
enum WatchCommand {
    #[command(about = "Watch the linked PRs, as the LaunchAgent does")]
    Run,
    #[command(about = "Check the config with a dry run, then start the LaunchAgent")]
    Install,
    #[command(about = "Stop and remove the LaunchAgent")]
    Uninstall,
    #[command(about = "Start the installed LaunchAgent")]
    Start,
    #[command(about = "Stop the LaunchAgent")]
    Stop,
    #[command(about = "Show what each linked PR's watch is doing")]
    Status {
        #[arg(long, help = "Print the JSON a program reads, without the LaunchAgent")]
        json: bool,
    },
    #[command(about = "Show the LaunchAgent's log")]
    Logs(LogFlags),
    #[command(about = "Register the calling chat as the one every message is sent from")]
    Sender,
    #[command(about = "Link the calling chat to a PR")]
    Link {
        #[arg(value_name = "PR")]
        pr: String,
        #[arg(long, help = "Take over a PR another chat is watched for")]
        take_over: bool,
    },
    #[command(about = "Let a halted PR's watch go on, taking its halted message as delivered")]
    Release {
        #[arg(value_name = "PR")]
        pr: String,
    },
    #[command(about = "Show what the watch would send, writing nothing")]
    DryRun {
        #[arg(
            long = "pr",
            value_name = "PR",
            help = "A PR to check; the configured repositories' open PRs when none"
        )]
        prs: Vec<String>,
    },
}

#[derive(Debug, PartialEq, Eq, Args)]
struct ConfigFlag {
    #[arg(
        long,
        value_name = "PATH",
        help = "The config file instead of the usual one"
    )]
    config: Option<PathBuf>,
}

#[derive(Debug, PartialEq, Eq, Args)]
struct SmokeFlags {
    #[arg(long, value_name = "OWNER/REPO#N|URL")]
    pr: String,
    #[arg(long, value_name = "LOGIN", help = "The login shown as the requester")]
    requester: Option<String>,
    #[arg(long, help = "Print the deep link instead of opening it")]
    no_open: bool,
    #[arg(
        long,
        value_name = "PATH",
        help = "The config file instead of the usual one"
    )]
    config: Option<PathBuf>,
}

#[derive(Debug, PartialEq, Eq, Args)]
struct LogFlags {
    #[arg(short = 'f', long, help = "Keep printing what is written to the log")]
    follow: bool,
}

// harnexus-crew starts `pr-wake-engine watch` beside pr-wake for a LaunchAgent that still runs
// harnexus-task watch, so that name runs the watch alone.
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

// Exit 0 is a success, with a watch command's JSON as one line on stdout; 1 is a failure with its
// reason on stderr, and 2 a usage error.
fn main() -> ExitCode {
    if invoked_as_engine() {
        return match EngineCli::try_parse() {
            Ok(EngineCli {
                command: EngineCommand::Watch,
            }) => finish(ENGINE, watch_command(WatchCommand::Run)),
            Err(error) => usage_error(&error),
        };
    }
    match Cli::try_parse() {
        Ok(Cli { feature }) => finish(
            NAME,
            match feature {
                Feature::Review(command) => review_command(command).map(|()| None),
                Feature::Watch(command) => watch_command(command),
            },
        ),
        // Parsed without clap's own exit, so the process ends through `main` like every command.
        Err(error) => usage_error(&error),
    }
}

fn review_command(command: ReviewCommand) -> Result<(), String> {
    let env = ReviewEnv::from_process().ok_or("the home directory is unknown")?;
    let config = |flag: Option<PathBuf>| flag.unwrap_or_else(|| env.paths.config.clone());
    match command {
        ReviewCommand::Run(flag) => review::run(&config(flag.config), &env),
        ReviewCommand::Status => review::show_status(&env),
        ReviewCommand::Check(flag) => review::check_from_terminal(&config(flag.config), &env),
        ReviewCommand::Smoke(flags) => review::smoke_from_terminal(
            &Smoke {
                config: &config(flags.config),
                pr: &flags.pr,
                requester: flags.requester.as_deref(),
                no_open: flags.no_open,
            },
            &env,
        ),
        ReviewCommand::Install => review::install(&env),
        ReviewCommand::Uninstall => agent::uninstall(agent::REVIEW, &env.home),
        ReviewCommand::Start => agent::start(agent::REVIEW, &env.home),
        ReviewCommand::Stop => agent::stop(agent::REVIEW),
        ReviewCommand::Logs(flags) => agent::logs(agent::REVIEW, &env.home, flags.follow),
    }
}

fn watch_command(command: WatchCommand) -> Result<Option<String>, String> {
    let env = |name: &str| env::var_os(name);
    let home = || env::home_dir().ok_or("the home directory is unknown");
    let mode = match command {
        WatchCommand::Install => return watch::install(&env).map(|()| None),
        WatchCommand::Uninstall => return agent::uninstall(agent::WATCH, &home()?).map(|()| None),
        WatchCommand::Start => return agent::start(agent::WATCH, &home()?).map(|()| None),
        WatchCommand::Stop => return agent::stop(agent::WATCH).map(|()| None),
        WatchCommand::Logs(flags) => {
            return agent::logs(agent::WATCH, &home()?, flags.follow).map(|()| None);
        }
        WatchCommand::Run => Mode::Run,
        WatchCommand::Status { json } => Mode::Status { json },
        WatchCommand::Sender => Mode::Sender,
        WatchCommand::Link { pr, take_over } => Mode::Link { pr, take_over },
        WatchCommand::Release { pr } => Mode::Release(pr),
        WatchCommand::DryRun { prs } => Mode::DryRun(prs),
    };
    watch::run(mode, &env).map_err(|failure| failure.to_string())
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

    const PR: &str = "https://github.com/org/pay/pull/7";

    #[test]
    fn watch_commands_read_the_pr_before_or_after_their_flags() {
        for (name, arguments, expected) in [
            ("sender", &["watch", "sender"][..], WatchCommand::Sender),
            (
                "link",
                &["watch", "link", PR],
                WatchCommand::Link {
                    pr: PR.to_owned(),
                    take_over: false,
                },
            ),
            (
                "take_over_after_the_pr",
                &["watch", "link", PR, "--take-over"],
                WatchCommand::Link {
                    pr: PR.to_owned(),
                    take_over: true,
                },
            ),
            (
                "take_over_before_the_pr",
                &["watch", "link", "--take-over", PR],
                WatchCommand::Link {
                    pr: PR.to_owned(),
                    take_over: true,
                },
            ),
            (
                "release",
                &["watch", "release", PR],
                WatchCommand::Release { pr: PR.to_owned() },
            ),
            (
                "dry_run",
                &["watch", "dry-run"],
                WatchCommand::DryRun { prs: Vec::new() },
            ),
            (
                "dry_run_of_prs",
                &["watch", "dry-run", "--pr", PR, "--pr", "x"],
                WatchCommand::DryRun {
                    prs: vec![PR.to_owned(), "x".to_owned()],
                },
            ),
            (
                "status_json",
                &["watch", "status", "--json"],
                WatchCommand::Status { json: true },
            ),
            ("run", &["watch", "run"], WatchCommand::Run),
        ] {
            let parsed = Cli::try_parse_from(command_line(arguments));

            assert_eq!(
                parsed
                    .map(|cli| cli.feature)
                    .map_err(|error| error.to_string()),
                Ok(Feature::Watch(expected)),
                "case: {name}"
            );
        }
    }

    #[test]
    fn review_commands_read_their_flags() {
        for (name, arguments, expected) in [
            (
                "smoke",
                &["review", "smoke", "--pr", "org/pay#7", "--no-open"][..],
                ReviewCommand::Smoke(SmokeFlags {
                    pr: "org/pay#7".to_owned(),
                    requester: None,
                    no_open: true,
                    config: None,
                }),
            ),
            (
                "check_with_a_config",
                &["review", "check", "--config", "/tmp/review.json"],
                ReviewCommand::Check(ConfigFlag {
                    config: Some(PathBuf::from("/tmp/review.json")),
                }),
            ),
            (
                "logs_followed",
                &["review", "logs", "-f"],
                ReviewCommand::Logs(LogFlags { follow: true }),
            ),
        ] {
            let parsed = Cli::try_parse_from(command_line(arguments));

            assert_eq!(
                parsed
                    .map(|cli| cli.feature)
                    .map_err(|error| error.to_string()),
                Ok(Feature::Review(expected)),
                "case: {name}"
            );
        }
    }

    #[test]
    fn wrong_command_lines_are_usage_errors() {
        for (name, arguments) in [
            ("sender_with_an_argument", &["watch", "sender", "extra"][..]),
            ("link_without_a_pr", &["watch", "link"]),
            ("link_of_two_prs", &["watch", "link", PR, PR]),
            ("release_without_a_pr", &["watch", "release"]),
            ("dry_run_of_a_bare_pr", &["watch", "dry-run", PR]),
            ("unknown_watch_command", &["watch", "unknown"]),
            ("smoke_without_a_pr", &["review", "smoke"]),
            ("engine_flag", &["watch", "--status"]),
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

    fn command_line<'arguments>(
        arguments: &[&'arguments str],
    ) -> impl Iterator<Item = &'arguments str> {
        iter::once(NAME).chain(arguments.iter().copied())
    }
}
