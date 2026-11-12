use std::{
    error::Error,
    ffi::OsString,
    fmt::{self, Display, Formatter},
    io::{self, Read, Write},
    os::unix::process::{CommandExt, ExitStatusExt},
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use super::stop::StopRequest;

pub(crate) struct ExternalCommand {
    program: OsString,
    arguments: Vec<OsString>,
    environment: Vec<(OsString, OsString)>,
    input: Option<Vec<u8>>,
    timeout: Duration,
}

// A command that ran to its end succeeds whatever its exit code, so the caller decides what the code means.
#[derive(Debug)]
pub(crate) struct CommandOutput {
    pub(crate) code: i32,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

#[derive(Debug)]
pub(crate) struct CommandError {
    program: String,
    pub(crate) kind: CommandErrorKind,
}

#[derive(Debug)]
pub(crate) enum CommandErrorKind {
    Launch(io::Error),
    StoppedBeforeStart,
    // The process and every thread reading or writing its streams have ended.
    Terminated {
        cause: Termination,
        status: ExitStatus,
    },
    // A process outside the group still holds the output, so its readers keep running rather than block the caller.
    OutputHeld {
        cause: Termination,
    },
    Signaled(ExitStatus),
    Io(io::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Termination {
    Timeout(Duration),
    StopRequest,
}

pub(crate) struct ToolCall<'call> {
    pub(crate) program: &'call Path,
    pub(crate) arguments: Vec<OsString>,
    pub(crate) timeout: Duration,
    pub(crate) stop: &'call StopRequest,
}

// The tool's standard output, or why it did not exit with 0.
pub(crate) type RunTool<'run> = dyn Fn(&ToolCall<'_>) -> Result<Vec<u8>, String> + Sync + 'run;

// alerter removes its notification only on SIGTERM, so SIGKILL waits this long after it.
const TERMINATION_GRACE: Duration = Duration::from_secs(5);
// A pipe still open this long after SIGKILL reached the group is held by a process outside it.
const OUTPUT_GRACE: Duration = Duration::from_secs(1);
// Well under the one second within which every wait must notice a stop request.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

impl ExternalCommand {
    #[must_use]
    pub(crate) fn new(program: impl Into<OsString>, timeout: Duration) -> Self {
        Self {
            program: program.into(),
            arguments: Vec::new(),
            environment: Vec::new(),
            input: None,
            timeout,
        }
    }

    #[must_use]
    pub(crate) fn arguments<I, S>(mut self, arguments: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.arguments.extend(arguments.into_iter().map(Into::into));
        self
    }

    #[must_use]
    pub(crate) fn env(mut self, name: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.environment.push((name.into(), value.into()));
        self
    }

    #[must_use]
    pub(crate) fn input(mut self, input: impl Into<Vec<u8>>) -> Self {
        self.input = Some(input.into());
        self
    }

    pub(crate) fn run(&self, stop: &StopRequest) -> Result<CommandOutput, CommandError> {
        if stop.is_requested() {
            return Err(self.error(CommandErrorKind::StoppedBeforeStart));
        }
        let mut child = self
            .spawn()
            .map_err(|error| self.error(CommandErrorKind::Launch(error)))?;
        let deadline = Instant::now() + self.timeout;
        let streams = Streams::start(&mut child, self.input.clone());
        let ending = wait_for_end(&mut child, &streams, deadline, self.timeout, stop)
            .map_err(|error| self.error(CommandErrorKind::Io(error)))?;

        let kind = match ending {
            Ending::Finished(status) => {
                let (stdout, stderr) = streams
                    .collect()
                    .map_err(|error| self.error(CommandErrorKind::Io(error)))?;
                let Some(code) = status.code() else {
                    return Err(self.error(CommandErrorKind::Signaled(status)));
                };
                return Ok(CommandOutput {
                    code,
                    stdout,
                    stderr,
                });
            }
            Ending::Terminated { cause, status } => {
                // A stopped command's output is incomplete, so it is dropped once its threads end.
                let _ = streams.collect();
                CommandErrorKind::Terminated { cause, status }
            }
            Ending::OutputHeld { cause } => CommandErrorKind::OutputHeld { cause },
        };
        Err(self.error(kind))
    }

    fn spawn(&self) -> io::Result<Child> {
        let mut command = Command::new(&self.program);
        command
            .args(&self.arguments)
            .envs(self.environment.iter().map(|(name, value)| (name, value)))
            .stdin(if self.input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Processes the child starts can hold its pipes open, so its own group lets one signal reach them all.
            .process_group(0);
        command.spawn()
    }

    fn error(&self, kind: CommandErrorKind) -> CommandError {
        CommandError {
            program: self.program.to_string_lossy().into_owned(),
            kind,
        }
    }
}

// As a shell would echo it, so the owner sees what pr-wake ran.
#[must_use]
pub(crate) fn command_line(program: &str, arguments: &[OsString]) -> String {
    let mut line = program.to_owned();
    for argument in arguments {
        line.push(' ');
        line.push_str(&argument.to_string_lossy());
    }
    line
}

pub(crate) fn run_tool(call: &ToolCall<'_>) -> Result<Vec<u8>, String> {
    let output = ExternalCommand::new(call.program, call.timeout)
        .arguments(&call.arguments)
        .run(call.stop)
        .map_err(|error| error.to_string())?;
    if output.code == 0 {
        return Ok(output.stdout);
    }

    let name = call
        .program
        .file_name()
        .unwrap_or(call.program.as_os_str())
        .to_string_lossy();
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(match stderr.trim() {
        "" => format!("{name}: exit status {}", output.code),
        stderr => format!("{name}: exit status {}: {stderr}", output.code),
    })
}

impl CommandError {
    // Whether the command may have acted.
    #[must_use]
    pub(crate) const fn started(&self) -> bool {
        !matches!(
            self.kind,
            CommandErrorKind::Launch(_) | CommandErrorKind::StoppedBeforeStart
        )
    }
}

impl Display for CommandError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        let program = &self.program;
        match &self.kind {
            CommandErrorKind::Launch(error) => write!(formatter, "cannot run {program}: {error}"),
            CommandErrorKind::StoppedBeforeStart => {
                write!(
                    formatter,
                    "{program} was not run because a stop was requested"
                )
            }
            CommandErrorKind::Terminated { cause, status } => match status.signal() {
                Some(signal) => write!(
                    formatter,
                    "{program} was stopped {cause} and ended by signal {signal}"
                ),
                None => write!(
                    formatter,
                    "{program} was stopped {cause} and ended with {status}"
                ),
            },
            CommandErrorKind::OutputHeld { cause } => write!(
                formatter,
                "{program} was stopped {cause}, but a process it started still holds its output"
            ),
            CommandErrorKind::Signaled(status) => match status.signal() {
                Some(signal) => write!(formatter, "{program} was ended by signal {signal}"),
                None => write!(formatter, "{program} ended with {status}"),
            },
            CommandErrorKind::Io(error) => {
                write!(formatter, "failed while running {program}: {error}")
            }
        }
    }
}

impl Error for CommandError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match &self.kind {
            CommandErrorKind::Launch(error) | CommandErrorKind::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl Display for Termination {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout(timeout) => write!(formatter, "after {} ms", timeout.as_millis()),
            Self::StopRequest => formatter.write_str("by a stop request"),
        }
    }
}

enum Ending {
    Finished(ExitStatus),
    Terminated {
        cause: Termination,
        status: ExitStatus,
    },
    OutputHeld {
        cause: Termination,
    },
}

fn wait_for_end(
    child: &mut Child,
    streams: &Streams,
    deadline: Instant,
    timeout: Duration,
    stop: &StopRequest,
) -> io::Result<Ending> {
    let Ok(group) = libc::pid_t::try_from(child.id()) else {
        abandon(child);
        return Err(io::Error::other(
            "the child process ID does not fit a pid_t",
        ));
    };

    let mut termination: Option<(Termination, Instant)> = None;
    let mut killed = false;
    loop {
        // Reaping releases the group ID, so the child is reaped only after its streams close or are given up on.
        if streams.are_closed()
            && let Some(status) = reap(child)?
        {
            return Ok(match termination {
                None => Ending::Finished(status),
                Some((cause, _)) => Ending::Terminated { cause, status },
            });
        }

        let now = Instant::now();
        match termination {
            None => {
                let cause = if stop.is_requested() {
                    Some(Termination::StopRequest)
                } else if now >= deadline {
                    Some(Termination::Timeout(timeout))
                } else {
                    None
                };
                if let Some(cause) = cause {
                    signal_group(group, libc::SIGTERM);
                    termination = Some((cause, now));
                }
            }
            Some((cause, since)) => {
                let waited = now.duration_since(since);
                if !killed && waited >= TERMINATION_GRACE {
                    signal_group(group, libc::SIGKILL);
                    killed = true;
                }
                if killed && waited >= TERMINATION_GRACE + OUTPUT_GRACE && reap(child)?.is_some() {
                    return Ok(Ending::OutputHeld { cause });
                }
            }
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn reap(child: &mut Child) -> io::Result<Option<ExitStatus>> {
    child.try_wait().inspect_err(|_| abandon(child))
}

fn signal_group(group: libc::pid_t, signal: libc::c_int) {
    // SAFETY: kill only sends a signal, and a process that has already exited ignores it.
    // The child leads the group and is not reaped yet, so the group ID cannot belong to anything else.
    unsafe { libc::kill(-group, signal) };
}

fn abandon(child: &mut Child) {
    // The child's state can no longer be read, so it is killed rather than left running unseen.
    let _ = child.kill();
    let _ = child.wait();
}

struct Streams {
    input: Option<JoinHandle<io::Result<()>>>,
    stdout: JoinHandle<io::Result<Vec<u8>>>,
    stderr: JoinHandle<io::Result<Vec<u8>>>,
}

impl Streams {
    fn start(child: &mut Child, input: Option<Vec<u8>>) -> Self {
        let input = child.stdin.take().zip(input).map(|(mut stdin, input)| {
            thread::spawn(move || match stdin.write_all(&input) {
                // A child may end without reading all of its input, which is not a failure.
                Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
                written => written,
            })
        });
        Self {
            input,
            stdout: spawn_reader(child.stdout.take()),
            stderr: spawn_reader(child.stderr.take()),
        }
    }

    fn are_closed(&self) -> bool {
        self.input.as_ref().is_none_or(JoinHandle::is_finished)
            && self.stdout.is_finished()
            && self.stderr.is_finished()
    }

    fn collect(self) -> io::Result<(Vec<u8>, Vec<u8>)> {
        let input = self.input.map_or(Ok(()), join);
        let stdout = join(self.stdout);
        let stderr = join(self.stderr);
        input?;
        Ok((stdout?, stderr?))
    }
}

fn spawn_reader(stream: Option<impl Read + Send + 'static>) -> JoinHandle<io::Result<Vec<u8>>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut stream) = stream {
            stream.read_to_end(&mut bytes)?;
        }
        Ok(bytes)
    })
}

fn join<T>(handle: JoinHandle<io::Result<T>>) -> io::Result<T> {
    handle
        .join()
        .unwrap_or_else(|_| Err(io::Error::other("a command stream thread panicked")))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use rstest::rstest;

    use super::*;

    const SHELL: &str = "/bin/sh";
    const TEN_SECONDS: Duration = Duration::from_secs(10);
    // Renamed into place once the shell is set up, so a reader never sees a partial PID.
    const WRITE_PID: &str = r#"echo $$ > "$1.tmp" && mv "$1.tmp" "$1""#;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Trigger {
        Timeout,
        StopRequest,
    }

    #[test]
    fn exit_code_and_both_outputs_are_returned_for_a_failing_command() {
        let command = shell("printf out; printf err >&2; exit 3", TEN_SECONDS);

        let output = command.run(&StopRequest::default()).expect("run sh");

        assert_eq!(output.code, 3);
        assert_eq!(output.stdout, b"out");
        assert_eq!(output.stderr, b"err");
    }

    #[test]
    fn input_larger_than_a_pipe_buffer_is_echoed_back_whole() {
        let input = (0..1_u32 << 20)
            .map(|index| index.to_le_bytes()[0])
            .collect::<Vec<_>>();
        let command = ExternalCommand::new("cat", TEN_SECONDS).input(input.clone());

        let output = command.run(&StopRequest::default()).expect("run cat");

        assert_eq!(output.code, 0);
        assert!(output.stdout == input, "cat returned different bytes");
    }

    #[test]
    fn environment_is_added_to_the_inherited_one() {
        let command = shell(
            r#"printf '%s ' "$PR_WAKE_TEST"; [ -n "$HOME" ] && printf home"#,
            TEN_SECONDS,
        )
        .env("PR_WAKE_TEST", "set");

        let output = command.run(&StopRequest::default()).expect("run sh");

        assert_eq!(output.stdout, b"set home");
    }

    #[test]
    fn missing_program_fails_without_starting() {
        let command = ExternalCommand::new("/nonexistent/pr-wake-test", TEN_SECONDS);

        let error = command
            .run(&StopRequest::default())
            .expect_err("no such program");

        assert!(
            matches!(error.kind, CommandErrorKind::Launch(_)),
            "{error:?}"
        );
        assert!(!error.started());
    }

    #[test]
    fn stop_requested_beforehand_skips_the_command() {
        let directory = tempfile::tempdir().expect("create a directory");
        let marker = directory.path().join("ran");
        let command = shell(r#"touch "$1""#, TEN_SECONDS).arguments([&marker]);
        let stop = StopRequest::default();
        stop.request();

        let error = command.run(&stop).expect_err("stopped beforehand");

        assert!(
            matches!(error.kind, CommandErrorKind::StoppedBeforeStart),
            "{error:?}"
        );
        assert!(!error.started());
        assert!(!marker.exists());
    }

    #[test]
    fn signal_from_outside_is_reported_with_the_signal() {
        let command = shell("kill -KILL $$", TEN_SECONDS);

        let error = command
            .run(&StopRequest::default())
            .expect_err("killed itself");

        let CommandErrorKind::Signaled(status) = error.kind else {
            panic!("expected a signal, got {error:?}");
        };
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        assert!(error.started());
    }

    #[test]
    fn stop_request_ends_a_running_child_within_a_second() {
        let directory = tempfile::tempdir().expect("create a directory");
        let ready = directory.path().join("ready");
        let command = shell(
            &format!("{WRITE_PID}; exec sleep 60"),
            Duration::from_secs(60),
        )
        .arguments([&ready]);
        let stop = StopRequest::default();

        let (result, requested_at) = thread::scope(|scope| {
            let requester = scope.spawn(|| request_when_ready(&ready, &stop));
            let result = command.run(&stop);
            (result, requester.join().expect("join the requester"))
        });

        let waited = requested_at.elapsed();
        let error = result.expect_err("stopped");
        let CommandErrorKind::Terminated { cause, status } = error.kind else {
            panic!("expected a termination, got {error:?}");
        };
        assert_eq!(cause, Termination::StopRequest);
        assert_eq!(status.signal(), Some(libc::SIGTERM));
        assert!(
            waited < Duration::from_secs(1),
            "stopped {waited:?} after the request"
        );
        assert!(error.started());
    }

    // The child ignores SIGTERM and leaves an input larger than a pipe buffer unread, so its writer blocks until SIGKILL.
    // A Terminated result then means every stream thread was joined.
    #[rstest]
    #[case::timeout(Trigger::Timeout)]
    #[case::stop_request(Trigger::StopRequest)]
    fn child_ignoring_sigterm_is_killed_after_the_grace_and_reaped(#[case] trigger: Trigger) {
        let directory = tempfile::tempdir().expect("create a directory");
        let ready = directory.path().join("ready");
        let timeout = match trigger {
            Trigger::Timeout => Duration::from_secs(1),
            Trigger::StopRequest => Duration::from_secs(60),
        };
        let command = shell(
            &format!("trap '' TERM; {WRITE_PID}; exec sleep 60"),
            timeout,
        )
        .arguments([&ready])
        .input(vec![b'x'; 1 << 20]);
        let stop = StopRequest::default();
        let started_at = Instant::now();

        let (result, terminated_at) = thread::scope(|scope| match trigger {
            Trigger::Timeout => (command.run(&stop), started_at + timeout),
            Trigger::StopRequest => {
                let requester = scope.spawn(|| request_when_ready(&ready, &stop));
                let result = command.run(&stop);
                (result, requester.join().expect("join the requester"))
            }
        });

        let waited = terminated_at.elapsed();
        let error = result.expect_err("terminated");
        let CommandErrorKind::Terminated { cause, status } = error.kind else {
            panic!("expected a termination, got {error:?}");
        };
        let expected_cause = match trigger {
            Trigger::Timeout => Termination::Timeout(timeout),
            Trigger::StopRequest => Termination::StopRequest,
        };
        assert_eq!(cause, expected_cause);
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        assert!(
            (TERMINATION_GRACE..TERMINATION_GRACE + Duration::from_secs(2)).contains(&waited),
            "SIGKILL came {waited:?} after the termination began"
        );
        assert!(!is_running(read_pid(&ready)), "the child was not reaped");
    }

    #[test]
    fn background_process_holding_the_output_is_stopped_with_the_group() {
        let directory = tempfile::tempdir().expect("create a directory");
        let ready = directory.path().join("ready");
        let timeout = Duration::from_secs(1);
        let script = format!(r#"sh -c '{WRITE_PID}; exec sleep 60' sh "$1" & echo started"#);
        let command = shell(&script, timeout).arguments([&ready]);

        let error = command.run(&StopRequest::default()).expect_err("timed out");

        let CommandErrorKind::Terminated { cause, .. } = error.kind else {
            panic!("expected a termination, got {error:?}");
        };
        assert_eq!(cause, Termination::Timeout(timeout));
        wait_until_gone(read_pid(&ready));
    }

    #[test]
    fn output_held_outside_the_group_releases_the_caller() {
        let directory = tempfile::tempdir().expect("create a directory");
        let ready = directory.path().join("ready");
        let timeout = Duration::from_secs(1);
        // perl leaves the child's group but keeps the inherited output open.
        let script = r#"perl -e 'setpgrp(0, 0); open(my $f, ">", "$ARGV[0].tmp") or die; print $f $$; close $f; rename("$ARGV[0].tmp", $ARGV[0]); sleep 60' "$1" &"#;
        let command = shell(script, timeout).arguments([&ready]);
        let started_at = Instant::now();

        let result = command.run(&StopRequest::default());

        let waited = started_at.elapsed();
        let escaped = read_pid(&ready);
        // SAFETY: the process was started by this test and only receives SIGKILL.
        unsafe { libc::kill(escaped, libc::SIGKILL) };
        let error = result.expect_err("timed out");
        assert!(
            matches!(
                error.kind,
                CommandErrorKind::OutputHeld {
                    cause: Termination::Timeout(_)
                }
            ),
            "{error:?}"
        );
        let limit = timeout + TERMINATION_GRACE + OUTPUT_GRACE + Duration::from_secs(1);
        assert!(waited < limit, "released {waited:?} after the start");
    }

    #[test]
    fn tool_gives_its_output_only_when_it_exits_with_zero() {
        let stop = StopRequest::default();
        for (name, script, expected) in [
            ("success", "printf out; printf err >&2", Ok(b"out".to_vec())),
            (
                "failure_with_stderr",
                "printf out; printf ' err\\n' >&2; exit 3",
                Err("sh: exit status 3: err".to_owned()),
            ),
            (
                "silent_failure",
                "exit 1",
                Err("sh: exit status 1".to_owned()),
            ),
        ] {
            let call = ToolCall {
                program: Path::new(SHELL),
                arguments: vec!["-c".into(), script.into()],
                timeout: TEN_SECONDS,
                stop: &stop,
            };

            assert_eq!(run_tool(&call), expected, "case: {name}");
        }
    }

    #[test]
    fn tool_that_cannot_start_is_told_with_its_path() {
        let stop = StopRequest::default();
        let call = ToolCall {
            program: Path::new("/nonexistent/pr-wake-tool"),
            arguments: Vec::new(),
            timeout: TEN_SECONDS,
            stop: &stop,
        };

        let ran = run_tool(&call);

        assert!(
            ran.as_ref()
                .is_err_and(|error| error.contains("/nonexistent/pr-wake-tool")),
            "{ran:?}"
        );
    }

    fn shell(script: &str, timeout: Duration) -> ExternalCommand {
        ExternalCommand::new(SHELL, timeout).arguments(["-c", script, "sh"])
    }

    // The time is taken before the request, so the run cannot have seen the request earlier.
    fn request_when_ready(ready: &Path, stop: &StopRequest) -> Instant {
        wait_for_file(ready);
        let requested_at = Instant::now();
        stop.request();
        requested_at
    }

    fn wait_for_file(path: &Path) {
        let deadline = Instant::now() + TEN_SECONDS;
        while !path.exists() {
            assert!(
                Instant::now() < deadline,
                "{} never appeared",
                path.display()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn read_pid(path: &Path) -> libc::pid_t {
        wait_for_file(path);
        fs::read_to_string(path)
            .expect("read the PID")
            .trim()
            .parse()
            .expect("parse the PID")
    }

    // A process whose parent has exited stays a zombie until init reaps it.
    fn wait_until_gone(pid: libc::pid_t) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while is_running(pid) {
            assert!(Instant::now() < deadline, "process {pid} is still running");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn is_running(pid: libc::pid_t) -> bool {
        // SAFETY: signal 0 only checks whether the process exists.
        unsafe { libc::kill(pid, 0) == 0 }
    }
}
