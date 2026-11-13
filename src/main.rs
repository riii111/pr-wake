use std::process::ExitCode;

mod app;
mod cli;
mod infra;
mod runtime;

fn main() -> ExitCode {
    cli::run()
}
