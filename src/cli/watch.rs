use std::env;

use clap::Subcommand;

use crate::runtime::{
    agent,
    watch::{self, Mode},
};

use super::LogFlags;

#[derive(Debug, PartialEq, Eq, Subcommand)]
pub(super) enum WatchCommand {
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

pub(super) fn run(command: WatchCommand) -> Result<Option<String>, String> {
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
