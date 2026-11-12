use std::process::ExitCode;

use clap::Parser;

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "its callers arrive with the watch commands")
)]
mod app;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "its callers arrive with the watch commands")
)]
mod infra;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "its callers arrive with the watch commands")
)]
mod runtime;

#[derive(Parser)]
#[command(name = "pr-wake-engine", version, about, arg_required_else_help = true)]
struct Cli {}

fn main() -> ExitCode {
    match Cli::try_parse() {
        Ok(Cli {}) => ExitCode::SUCCESS,
        // Parsed without clap's own exit, so the process ends through `main` like every command.
        Err(error) => {
            let _ = error.print();
            ExitCode::from(u8::try_from(error.exit_code()).unwrap_or(1))
        }
    }
}
