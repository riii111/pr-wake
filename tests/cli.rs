use std::process::{Command, Output};

use rstest::rstest;

const ENGINE: &str = env!("CARGO_BIN_EXE_pr-wake-engine");

#[test]
fn help_shows_the_usage_and_succeeds() {
    let output = run(&["--help"]);

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 help");
    assert!(stdout.contains("Usage: pr-wake-engine"), "{stdout}");
    assert!(output.stderr.is_empty());
}

#[test]
fn version_names_the_engine() {
    let output = run(&["--version"]);

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 version");
    assert_eq!(
        stdout,
        format!("pr-wake-engine {}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[rstest]
#[case::no_arguments(&[])]
#[case::watch_command(&["watch"])]
fn anything_but_help_or_version_is_a_usage_error(#[case] arguments: &[&str]) {
    let output = run(arguments);

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 error");
    assert!(stderr.contains("Usage: pr-wake-engine"), "{stderr}");
}

fn run(arguments: &[&str]) -> Output {
    Command::new(ENGINE)
        .args(arguments)
        .output()
        .expect("run pr-wake-engine")
}
