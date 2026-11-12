use std::{env, process::ExitCode};

use clap::{Args, CommandFactory, Parser, Subcommand, error::ErrorKind};

mod app;
mod infra;
mod runtime;

use runtime::{
    output,
    watch::{self, Mode, NAME},
};

#[derive(Parser)]
#[command(
    name = NAME,
    version,
    about,
    override_usage = WATCH_USAGE,
    arg_required_else_help = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    #[command(
        about = "Watch the linked PRs, or run one of the watch's commands",
        override_usage = WATCH_USAGE
    )]
    Watch(WatchFlags),
}

#[derive(Args)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each switch is a flag of the command line the Go relay and harnexus-crew pass"
)]
struct WatchFlags {
    #[arg(
        long,
        help = "Show what the watch would send for the open PRs, writing nothing"
    )]
    dry_run: bool,
    #[arg(
        long = "pr",
        value_name = "PR",
        help = "A PR for --dry-run; the configured repositories' open PRs when none"
    )]
    prs: Vec<String>,
    #[arg(
        long,
        value_name = "PR",
        help = "Let a halted PR's watch go on, taking its halted message as delivered"
    )]
    release: Option<String>,
    #[arg(long, help = "Print what each linked PR's watch is doing")]
    status: bool,
    #[arg(
        long,
        help = "Register the calling chat as the one every message is sent from"
    )]
    sender: bool,
    #[arg(long, value_name = "PR", help = "Link the calling chat to a PR")]
    link: Option<String>,
    #[arg(long, help = "With --link, take over a PR another chat is watched for")]
    take_over: bool,
}

const WATCH_USAGE: &str = "pr-wake-engine watch [--dry-run [--pr PR]... | --release PR | --status | --sender | --link PR [--take-over]]";

// Exit 0 prints one JSON line, 1 is a failure on stderr and 2 a usage error, as harnexus-task
// watch answered.
fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        // Parsed without clap's own exit, so the process ends through `main` like every command.
        Err(error) => return usage_error(&error),
    };
    let Command::Watch(flags) = cli.command;
    let mode = match mode_of(flags) {
        Ok(mode) => mode,
        Err(message) => {
            return usage_error(&Cli::command().error(ErrorKind::ArgumentConflict, message));
        }
    };
    match watch::run(mode, &|name| env::var_os(name)) {
        Ok(Some(result)) => {
            output::write_stdout(&format!("{result}\n"));
            ExitCode::SUCCESS
        }
        Ok(None) => ExitCode::SUCCESS,
        Err(failure) => {
            output::write_stderr(&format!("{NAME}: {failure}\n"));
            ExitCode::FAILURE
        }
    }
}

fn mode_of(flags: WatchFlags) -> Result<Mode, &'static str> {
    if !flags.prs.is_empty() && !flags.dry_run {
        return Err("--pr is only for --dry-run");
    }
    if flags.take_over && flags.link.is_none() {
        return Err("--take-over is only for --link");
    }
    let chosen = [
        flags.release.is_some(),
        flags.dry_run,
        flags.status,
        flags.sender,
        flags.link.is_some(),
    ];
    if chosen.into_iter().filter(|chosen| *chosen).count() > 1 {
        return Err("use only one of --dry-run, --release, --status, --sender and --link");
    }
    Ok(if let Some(pr) = flags.release {
        Mode::Release(pr)
    } else if let Some(pr) = flags.link {
        Mode::Link {
            pr,
            take_over: flags.take_over,
        }
    } else if flags.dry_run {
        Mode::DryRun(flags.prs)
    } else if flags.status {
        Mode::Status
    } else if flags.sender {
        Mode::Sender
    } else {
        Mode::Run
    })
}

fn usage_error(error: &clap::Error) -> ExitCode {
    let _ = error.print();
    ExitCode::from(u8::try_from(error.exit_code()).unwrap_or(1))
}
