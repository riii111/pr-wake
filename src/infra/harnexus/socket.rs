use std::{
    io::{self, Read, Write},
    os::unix::net::UnixStream,
    path::Path,
    time::{Duration, Instant},
};

use crate::infra::stop::StopRequest;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SocketFailure {
    // False when the connection itself failed, so the line was never written.
    pub(crate) connected: bool,
    // The errno name, as the TypeScript watch reported it.
    pub(crate) code: Option<&'static str>,
    pub(crate) message: String,
}

// Each read and write waits at most this long, well under the one second within which every
// wait must notice a stop request.
const STOP_CHECK: Duration = Duration::from_millis(200);

// Sends one line and reads the answer until the bridge closes the connection; the timeout covers
// the whole call, from connecting to the last byte, however the answer trickles in.
pub(crate) fn request_line(
    path: &Path,
    line: &str,
    timeout: Duration,
    stop: &StopRequest,
) -> Result<String, SocketFailure> {
    let deadline = Instant::now() + timeout;
    if stop.is_requested() {
        return Err(stopped(false));
    }
    let stream = UnixStream::connect(path).map_err(|error| failure(false, &error))?;
    if Instant::now() >= deadline {
        return Err(timed_out());
    }
    // An empty line is only a probe: the bridge answers nothing to it, but the connection proves
    // it is alive.
    if line.is_empty() {
        return Ok(String::new());
    }
    exchange(stream, line, deadline, stop)
}

// Everything here happens after the connection, so any failure may follow a delivered request.
fn exchange(
    mut stream: UnixStream,
    line: &str,
    deadline: Instant,
    stop: &StopRequest,
) -> Result<String, SocketFailure> {
    let request = format!("{line}\n");
    let mut written = 0;
    while written < request.len() {
        let wait = next_wait(deadline, stop)?;
        settle(stream.set_write_timeout(Some(wait)))?;
        match stream.write(&request.as_bytes()[written..]) {
            Ok(0) => return Err(failure(true, &io::Error::from(io::ErrorKind::WriteZero))),
            Ok(count) => written += count,
            Err(error) if is_wait(&error) => {}
            Err(error) => return Err(failure(true, &error)),
        }
    }

    let mut answer = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        let wait = next_wait(deadline, stop)?;
        settle(stream.set_read_timeout(Some(wait)))?;
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => answer.extend_from_slice(&buffer[..count]),
            Err(error) if is_wait(&error) => {}
            Err(error) => return Err(failure(true, &error)),
        }
    }
    Ok(String::from_utf8_lossy(&answer).trim_end().to_owned())
}

// `set_read_timeout` bounds only one read, so each wait is cut to what is left of the call.
fn next_wait(deadline: Instant, stop: &StopRequest) -> Result<Duration, SocketFailure> {
    if stop.is_requested() {
        return Err(stopped(true));
    }
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(timed_out());
    }
    Ok(left.min(STOP_CHECK))
}

// macOS refuses socket options with EINVAL once the peer has shut the connection, and the read or
// write that follows then returns at once, so the wait needs no bound of its own.
fn settle(set: io::Result<()>) -> Result<(), SocketFailure> {
    match set {
        Err(error) if error.raw_os_error() == Some(libc::EINVAL) => Ok(()),
        set => set.map_err(|error| failure(true, &error)),
    }
}

// macOS reports a read timeout as `WouldBlock` rather than `TimedOut`.
fn is_wait(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
    )
}

fn failure(connected: bool, error: &io::Error) -> SocketFailure {
    SocketFailure {
        connected,
        code: error.raw_os_error().and_then(errno_name),
        message: error.to_string(),
    }
}

fn timed_out() -> SocketFailure {
    SocketFailure {
        connected: true,
        code: Some("ETIMEDOUT"),
        message: "timed out".to_owned(),
    }
}

fn stopped(connected: bool) -> SocketFailure {
    SocketFailure {
        connected,
        code: None,
        message: "stopped by a stop request".to_owned(),
    }
}

// The names the hints and the log were written for.
const fn errno_name(errno: i32) -> Option<&'static str> {
    Some(match errno {
        libc::ENOENT => "ENOENT",
        libc::ECONNREFUSED => "ECONNREFUSED",
        libc::EPERM => "EPERM",
        libc::EACCES => "EACCES",
        libc::ECONNRESET => "ECONNRESET",
        libc::EPIPE => "EPIPE",
        libc::ENOTSOCK => "ENOTSOCK",
        libc::ENOTDIR => "ENOTDIR",
        libc::ETIMEDOUT => "ETIMEDOUT",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use std::{io::BufRead, io::BufReader, thread};

    use super::*;

    #[test]
    fn line_goes_out_and_the_whole_answer_comes_back() {
        let (client, server) = UnixStream::pair().expect("create a socket pair");
        let bridge = thread::spawn(move || {
            let mut reader = BufReader::new(server);
            let mut request = String::new();
            reader.read_line(&mut request).expect("read the request");
            let mut server = reader.into_inner();
            server.write_all(b"{\"outcome\":").expect("answer");
            server.write_all(b"\"done\"}\n").expect("answer");
            request
        });

        let answer = exchange(
            client,
            r#"{"version":1}"#,
            Instant::now() + Duration::from_secs(10),
            &StopRequest::default(),
        );

        assert_eq!(answer.as_deref(), Ok(r#"{"outcome":"done"}"#));
        assert_eq!(bridge.join().expect("join the bridge"), "{\"version\":1}\n");
    }

    #[test]
    fn answer_trickling_in_is_cut_off_at_the_call_deadline() {
        let (client, mut server) = UnixStream::pair().expect("create a socket pair");
        let timeout = Duration::from_millis(600);
        let bridge = thread::spawn(move || {
            // Answers one byte at a time, faster than any single read would time out, for longer
            // than the call may take.
            for _ in 0..40 {
                if server.write_all(b" ").is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
        });
        let started = Instant::now();

        let answer = exchange(client, "{}", started + timeout, &StopRequest::default());

        let waited = started.elapsed();
        assert_eq!(
            answer.map_err(|failure| failure.code),
            Err(Some("ETIMEDOUT"))
        );
        assert!(
            (timeout..timeout + Duration::from_millis(400)).contains(&waited),
            "ended {waited:?} after the start"
        );
        bridge.join().expect("join the bridge");
    }

    #[test]
    fn stop_request_cuts_off_a_waiting_call_within_a_second() {
        let (client, server) = UnixStream::pair().expect("create a socket pair");
        let stop = StopRequest::default();

        let (answer, requested_at) = thread::scope(|scope| {
            let requester = scope.spawn(|| {
                thread::sleep(Duration::from_millis(300));
                let requested_at = Instant::now();
                stop.request();
                requested_at
            });
            let answer = exchange(
                client,
                "{}",
                Instant::now() + Duration::from_secs(60),
                &stop,
            );
            (answer, requester.join().expect("join the requester"))
        });

        let waited = requested_at.elapsed();
        let failure = answer.expect_err("stopped");
        assert!(failure.connected);
        assert_eq!(failure.message, "stopped by a stop request");
        assert!(
            waited < Duration::from_secs(1),
            "stopped {waited:?} after the request"
        );
        drop(server);
    }

    #[test]
    fn missing_socket_fails_before_connecting() {
        let directory = tempfile::tempdir().expect("create a directory");

        let answer = request_line(
            &directory.path().join("call.sock"),
            "{}",
            Duration::from_secs(1),
            &StopRequest::default(),
        );

        assert_eq!(
            answer.map_err(|failure| (failure.connected, failure.code)),
            Err((false, Some("ENOENT")))
        );
    }

    #[test]
    fn stop_requested_beforehand_never_connects() {
        let directory = tempfile::tempdir().expect("create a directory");
        let stop = StopRequest::default();
        stop.request();

        let answer = request_line(
            &directory.path().join("call.sock"),
            "{}",
            Duration::from_secs(1),
            &stop,
        );

        assert_eq!(answer.map_err(|failure| failure.connected), Err(false));
    }
}
