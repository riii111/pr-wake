use std::path::PathBuf;

use clap::{Args, Subcommand};

use crate::runtime::{
    agent,
    review::{self, ReviewEnv, Smoke},
};

use super::LogFlags;

#[derive(Debug, PartialEq, Eq, Subcommand)]
pub(super) enum ReviewCommand {
    #[command(about = "Watch for review requests, as the LaunchAgent does")]
    Run(ConfigFlag),
    #[command(about = "Show the LaunchAgent and the latest requests")]
    Status,
    #[command(about = "Check the config, the commands and GitHub")]
    Check(ConfigFlag),
    #[command(about = "Take one PR through the notification and the launch")]
    Smoke(SmokeFlags),
    #[command(about = "Link the calling review chat to a PR, which then gets the PR's re-requests")]
    Link {
        #[arg(value_name = "OWNER/REPO#N|URL")]
        pr: String,
    },
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

#[derive(Debug, PartialEq, Eq, Args)]
pub(super) struct ConfigFlag {
    #[arg(
        long,
        value_name = "PATH",
        help = "The config file instead of the usual one"
    )]
    config: Option<PathBuf>,
}

#[derive(Debug, PartialEq, Eq, Args)]
pub(super) struct SmokeFlags {
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

pub(super) fn run(command: ReviewCommand) -> Result<(), String> {
    // Resolved per command, so stopping, which needs only the label, still works when the home is unknown.
    let env = || ReviewEnv::from_process().ok_or("the home directory is unknown");
    let config =
        |flag: Option<PathBuf>, env: &ReviewEnv| flag.unwrap_or_else(|| env.paths.config.clone());

    match command {
        ReviewCommand::Run(flag) => {
            let env = env()?;
            review::run(&config(flag.config, &env), &env)
        }
        ReviewCommand::Status => review::show_status(&env()?),
        ReviewCommand::Check(flag) => {
            let env = env()?;
            review::check_from_terminal(&config(flag.config, &env), &env)
        }
        ReviewCommand::Smoke(flags) => {
            let env = env()?;
            review::smoke_from_terminal(
                &Smoke {
                    config: &config(flags.config, &env),
                    pr: &flags.pr,
                    requester: flags.requester.as_deref(),
                    no_open: flags.no_open,
                },
                &env,
            )
        }
        ReviewCommand::Link { pr } => review::link_from_chat(&env()?, &pr),
        ReviewCommand::Install => review::install(&env()?),
        ReviewCommand::Uninstall => agent::uninstall(agent::REVIEW, &env()?.home),
        ReviewCommand::Start => agent::start(agent::REVIEW, &env()?.home),
        ReviewCommand::Stop => agent::stop(agent::REVIEW),
        ReviewCommand::Logs(flags) => agent::logs(agent::REVIEW, &env()?.home, flags.follow),
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;
    use crate::cli::{
        Cli, Feature,
        tests::{PR, command_line},
    };

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
                "link",
                &["review", "link", PR],
                ReviewCommand::Link { pr: PR.to_owned() },
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
}
