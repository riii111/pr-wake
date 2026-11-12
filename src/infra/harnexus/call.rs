use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    time::Duration,
};

use serde::Serialize;
use serde_json::{Map, Value};

use super::socket::{self, SocketFailure};
use crate::infra::stop::StopRequest;

// harnexus's public call socket: one JSON line out, one JSON line back.
pub(crate) struct Bridge {
    // Why the socket cannot be named, when it cannot.
    socket: Result<PathBuf, String>,
    timeouts: CallTimeouts,
    stop: StopRequest,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct CallTimeouts {
    pub(crate) query: Duration,
    pub(crate) call: Duration,
}

// What the bridge offers, or why it cannot be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Capabilities {
    Offered(Vec<String>),
    // It could not be asked.
    NotSent(String),
    // It answered without protocol 1.
    Unsupported(String),
}

pub(crate) const PROTOCOL_VERSION: u64 = 1;

// The tools the watch reads workers and sends to them with.
pub(crate) const WATCH_OPERATIONS: [&str; 2] = ["read_thread", "send_message_to_thread"];

#[derive(Serialize)]
struct Query {
    version: u64,
    query: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Call<'call> {
    version: u64,
    thread_id: &'call str,
    tool: &'call str,
    arguments: &'call Map<String, Value>,
}

const SOCKET_HINT: &str =
    "open the harnexus app with its call socket on and run pr-wake outside the sandbox";

impl CallTimeouts {
    // The call outlasts the longest wait the bridge allows, so the bridge always answers first.
    pub(crate) const STANDARD: Self = Self {
        query: Duration::from_secs(10),
        call: Duration::from_mins(15),
    };
}

impl Bridge {
    #[must_use]
    pub(crate) const fn new(
        socket: Result<PathBuf, String>,
        timeouts: CallTimeouts,
        stop: StopRequest,
    ) -> Self {
        Self {
            socket,
            timeouts,
            stop,
        }
    }

    // The query changes nothing, so any failure of it leaves a call unsent.
    pub(crate) fn capabilities(&self) -> Capabilities {
        let path = match &self.socket {
            Ok(path) => path,
            Err(error) => return Capabilities::NotSent(format!("{error}; {SOCKET_HINT}")),
        };
        let line = to_line(&Query {
            version: PROTOCOL_VERSION,
            query: "capabilities",
        });
        let answer = match socket::request_line(path, &line, self.timeouts.query, &self.stop) {
            Ok(answer) => answer,
            Err(failure) => {
                return Capabilities::NotSent(format!(
                    "{}: {}; {}",
                    path.display(),
                    reason_of(&failure),
                    socket_hint(failure.code)
                ));
            }
        };
        // An older bridge answers the query as an invalid call; it is never sent the versionless
        // form instead.
        let offered = serde_json::from_str::<Value>(&answer).ok();
        let operations = offered
            .as_ref()
            .filter(|offered| {
                offered["outcome"] == "done" && offered["version"] == PROTOCOL_VERSION
            })
            .and_then(|offered| offered["operations"].as_array());
        operations.map_or_else(
            || {
                Capabilities::Unsupported(format!(
                    "the harnexus bridge does not offer call protocol {PROTOCOL_VERSION}; update harnexus and restart the app"
                ))
            },
            |operations| {
                Capabilities::Offered(
                    operations
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect(),
                )
            },
        )
    }

    // A call goes out only after the bridge said it offers protocol 1 and the tool, so an older
    // bridge never receives it. The answer always carries an `outcome`.
    pub(crate) fn call(
        &self,
        caller: &str,
        tool: &str,
        arguments: &Map<String, Value>,
    ) -> Map<String, Value> {
        let operations = match self.capabilities() {
            Capabilities::Offered(operations) => operations,
            Capabilities::NotSent(message) => return outcome("not_sent", &message),
            Capabilities::Unsupported(message) => return outcome("unsupported", &message),
        };
        if !operations.iter().any(|operation| operation == tool) {
            return outcome(
                "unsupported",
                &format!("the harnexus bridge does not offer {tool}"),
            );
        }
        let path = match &self.socket {
            Ok(path) => path,
            Err(error) => return outcome("not_sent", error),
        };
        let line = to_line(&Call {
            version: PROTOCOL_VERSION,
            thread_id: caller,
            tool,
            arguments,
        });
        match socket::request_line(path, &line, self.timeouts.call, &self.stop) {
            Ok(answer) => answer_of(&answer),
            // The request may already have reached the app once connected.
            Err(failure) if failure.connected => {
                outcome("unknown", &format!("no answer: {}", reason_of(&failure)))
            }
            Err(failure) => outcome(
                "not_sent",
                &format!(
                    "{}: {}; {}",
                    path.display(),
                    reason_of(&failure),
                    socket_hint(failure.code)
                ),
            ),
        }
    }

    // The watch starts only once the bridge answers protocol 1 with every tool it uses; it never
    // falls back to the versionless protocol.
    pub(crate) fn check_v1(&self) -> Result<(), String> {
        let operations = match self.capabilities() {
            Capabilities::Offered(operations) => operations,
            Capabilities::NotSent(message) | Capabilities::Unsupported(message) => {
                return Err(format!("cannot use the harnexus call socket: {message}"));
            }
        };
        let missing = WATCH_OPERATIONS
            .iter()
            .filter(|tool| !operations.iter().any(|operation| operation == *tool))
            .copied()
            .collect::<Vec<_>>();
        if missing.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "the harnexus bridge does not offer {}; update harnexus and restart the app",
                missing.join(", ")
            ))
        }
    }
}

// Where harnexus serves the call socket, read by harnexus's documented settings so both sides
// agree.
pub(crate) fn call_socket_path(
    call_socket: Option<&OsStr>,
    state_path: Option<&OsStr>,
    home: Option<&Path>,
) -> Result<PathBuf, String> {
    let call_socket = call_socket.filter(|value| !value.is_empty());
    if call_socket.is_some_and(|value| value == "off") {
        return Err("HARNEXUS_CALL_SOCKET is off".to_owned());
    }
    if let Some(value) = call_socket.filter(|value| *value != "on") {
        let path = Path::new(value);
        return if path.is_absolute() {
            Ok(path.to_owned())
        } else {
            Err("HARNEXUS_CALL_SOCKET must be on, off or an absolute path".to_owned())
        };
    }
    let threads = match state_path.filter(|value| !value.is_empty()).map(Path::new) {
        Some(state) if !state.is_absolute() => {
            return Err("HARNEXUS_STATE_PATH must be an absolute path".to_owned());
        }
        Some(state) => state.to_owned(),
        None => home
            .ok_or_else(|| "the home directory is unknown".to_owned())?
            .join(".local/state/harnexus/threads.json"),
    };
    Ok(threads.with_file_name("call.sock"))
}

fn answer_of(answer: &str) -> Map<String, Value> {
    match serde_json::from_str::<Value>(answer) {
        Err(_) => outcome("unknown", "no answer: invalid JSON"),
        Ok(Value::Object(answer)) if answer.get("outcome").is_some_and(Value::is_string) => answer,
        Ok(_) => outcome("unknown", "no answer: answer without outcome"),
    }
}

fn outcome(outcome: &str, message: &str) -> Map<String, Value> {
    Map::from_iter([
        ("outcome".to_owned(), Value::from(outcome)),
        ("message".to_owned(), Value::from(message)),
    ])
}

fn to_line(request: &impl Serialize) -> String {
    serde_json::to_string(request).expect("a request of strings and numbers serializes")
}

fn reason_of(failure: &SocketFailure) -> &str {
    failure.code.unwrap_or(&failure.message)
}

fn socket_hint(code: Option<&str>) -> String {
    match code {
        Some("ENOENT") => format!("no bridge has opened the socket; {SOCKET_HINT}"),
        Some("ECONNREFUSED") => "a bridge that has exited left the socket; the running bridge takes it over within seconds, so the next cycle tries again".to_owned(),
        Some("EPERM" | "EACCES") => {
            "the sandbox blocked the connection; run pr-wake outside the sandbox".to_owned()
        }
        _ => SOCKET_HINT.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{BufRead, BufReader, Read, Write},
        os::unix::net::UnixListener,
        sync::{Arc, Mutex},
        thread,
    };

    use rstest::rstest;
    use serde_json::json;

    use super::*;

    // A bridge on a socket that answers each request line as the test says, keeping the lines it
    // was sent. Listening on a Unix socket needs a sandbox that allows it.
    struct FakeBridge {
        path: PathBuf,
        lines: Arc<Mutex<Vec<Value>>>,
        _directory: tempfile::TempDir,
    }

    const QUICK: CallTimeouts = CallTimeouts {
        query: Duration::from_secs(5),
        call: Duration::from_secs(5),
    };

    #[test]
    fn call_asks_for_protocol_1_and_sends_the_call_in_its_form() {
        let bridge = v1_bridge(|_| Some(json!({ "version": 1, "outcome": "done", "result": 1 })));

        let answer = client(&bridge, QUICK).call(
            "caller",
            "read_thread",
            &arguments(json!({ "threadId": "w" })),
        );

        assert_eq!(
            Value::Object(answer),
            json!({ "version": 1, "outcome": "done", "result": 1 })
        );
        assert_eq!(
            bridge.lines(),
            [
                json!({ "version": 1, "query": "capabilities" }),
                json!({
                    "version": 1,
                    "threadId": "caller",
                    "tool": "read_thread",
                    "arguments": { "threadId": "w" },
                }),
            ]
        );
    }

    #[test]
    fn bridge_without_protocol_1_is_sent_nothing_and_never_the_versionless_form() {
        let bridge = fake_bridge(|_| Some(old_bridge()));

        let answer = client(&bridge, QUICK).call(
            "caller",
            "send_message_to_thread",
            &arguments(json!({ "threadId": "w", "prompt": "hi" })),
        );

        assert_eq!(answer["outcome"], "unsupported");
        assert_eq!(
            bridge.lines(),
            [json!({ "version": 1, "query": "capabilities" })]
        );
    }

    #[test]
    fn tool_the_bridge_does_not_offer_is_not_sent() {
        let bridge = v1_bridge(|_| Some(json!({ "outcome": "done" })));

        let answer = client(&bridge, QUICK).call("caller", "create_thread", &Map::new());

        assert_eq!(
            (answer["outcome"].clone(), bridge.lines().len()),
            (json!("unsupported"), 1)
        );
    }

    #[test]
    fn connected_call_without_an_answer_in_time_is_unknown() {
        let bridge = v1_bridge(|_| None);
        let timeouts = CallTimeouts {
            query: Duration::from_secs(5),
            call: Duration::from_millis(50),
        };

        let answer =
            client(&bridge, timeouts).call("caller", "send_message_to_thread", &Map::new());

        assert_eq!(answer["outcome"], "unknown");
    }

    #[test]
    fn missing_socket_is_not_sent() {
        let directory = tempfile::tempdir().expect("create a directory");
        let bridge = Bridge::new(
            Ok(directory.path().join("missing.sock")),
            QUICK,
            StopRequest::default(),
        );

        let answer = bridge.call("caller", "send_message_to_thread", &Map::new());

        assert_eq!(answer["outcome"], "not_sent");
        assert!(
            answer["message"]
                .as_str()
                .is_some_and(|message| message.contains("ENOENT; no bridge has opened the socket")),
            "{answer:?}"
        );
    }

    #[test]
    fn answer_that_is_not_an_outcome_is_unknown() {
        for (name, answer, message) in [
            ("not_json", "{", "no answer: invalid JSON"),
            (
                "no_outcome",
                r#"{"result":1}"#,
                "no answer: answer without outcome",
            ),
            (
                "outcome_not_text",
                r#"{"outcome":1}"#,
                "no answer: answer without outcome",
            ),
        ] {
            let read = answer_of(answer);

            assert_eq!(
                Value::Object(read),
                json!({ "outcome": "unknown", "message": message }),
                "case: {name}"
            );
        }
    }

    #[test]
    fn check_v1_accepts_a_bridge_offering_the_watch_tools() {
        let bridge = v1_bridge(|_| None);

        assert_eq!(client(&bridge, QUICK).check_v1(), Ok(()));
    }

    #[rstest]
    #[case::before_protocol_1(old_bridge(), "does not offer call protocol 1")]
    #[case::without_send(
        json!({ "version": 1, "outcome": "done", "operations": ["read_thread"] }),
        "does not offer send_message_to_thread"
    )]
    #[case::another_version(
        json!({ "version": 2, "outcome": "done", "operations": WATCH_OPERATIONS }),
        "does not offer call protocol 1"
    )]
    fn check_v1_refuses_a_bridge_that_cannot_serve_the_watch(
        #[case] offered: Value,
        #[case] says: &str,
    ) {
        let bridge = fake_bridge(move |_| Some(offered.clone()));

        let checked = client(&bridge, QUICK).check_v1();

        assert!(
            checked.as_ref().is_err_and(|error| error.contains(says)),
            "{checked:?}"
        );
    }

    #[test]
    fn check_v1_refuses_when_the_bridge_cannot_be_asked() {
        let directory = tempfile::tempdir().expect("create a directory");
        let bridge = Bridge::new(
            Ok(directory.path().join("missing.sock")),
            QUICK,
            StopRequest::default(),
        );

        let checked = bridge.check_v1();

        assert!(
            checked
                .as_ref()
                .is_err_and(|error| error.contains("cannot use the harnexus call socket")),
            "{checked:?}"
        );
    }

    #[test]
    fn socket_path_follows_the_settings_harnexus_reads() {
        for (name, call_socket, state_path, home, expected) in [
            (
                "home",
                None,
                None,
                Some("/h"),
                Ok("/h/.local/state/harnexus/call.sock"),
            ),
            (
                "state_path",
                None,
                Some("/s/threads.json"),
                Some("/h"),
                Ok("/s/call.sock"),
            ),
            ("explicit", Some("/x.sock"), None, None, Ok("/x.sock")),
            (
                "on",
                Some("on"),
                None,
                Some("/h"),
                Ok("/h/.local/state/harnexus/call.sock"),
            ),
            (
                "off",
                Some("off"),
                None,
                None,
                Err("HARNEXUS_CALL_SOCKET is off"),
            ),
            (
                "relative",
                Some("x.sock"),
                None,
                None,
                Err("HARNEXUS_CALL_SOCKET must be on, off or an absolute path"),
            ),
            (
                "relative_state",
                None,
                Some("threads.json"),
                Some("/h"),
                Err("HARNEXUS_STATE_PATH must be an absolute path"),
            ),
        ] {
            let path = call_socket_path(
                call_socket.map(OsStr::new),
                state_path.map(OsStr::new),
                home.map(Path::new),
            );

            let expected = expected.map(PathBuf::from).map_err(str::to_owned);
            assert_eq!(path, expected, "case: {name}");
        }
    }

    fn client(bridge: &FakeBridge, timeouts: CallTimeouts) -> Bridge {
        Bridge::new(Ok(bridge.path.clone()), timeouts, StopRequest::default())
    }

    fn v1_bridge(call: impl Fn(&Value) -> Option<Value> + Send + 'static) -> FakeBridge {
        fake_bridge(move |request| {
            if request["query"] == "capabilities" {
                Some(json!({
                    "version": 1,
                    "outcome": "done",
                    "operations": ["read_thread", "send_message_to_thread", "name_thread"],
                }))
            } else {
                call(request)
            }
        })
    }

    // `None` keeps the connection open without an answer, as a bridge stuck mid-call does.
    fn fake_bridge(answer: impl Fn(&Value) -> Option<Value> + Send + 'static) -> FakeBridge {
        let directory = tempfile::tempdir().expect("create a directory");
        let path = directory.path().join("call.sock");
        let listener = UnixListener::bind(&path).expect("listen on the socket");
        let lines = Arc::new(Mutex::new(Vec::new()));
        let kept = Arc::clone(&lines);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    continue;
                }
                let request = serde_json::from_str::<Value>(&line).expect("a JSON request line");
                kept.lock().expect("lock the lines").push(request.clone());
                let mut stream = reader.into_inner();
                match answer(&request) {
                    Some(answered) => {
                        let _ = writeln!(stream, "{answered}");
                    }
                    // Held until the client gives up, which closes its end.
                    None => {
                        let _ = stream.read_to_end(&mut Vec::new());
                    }
                }
            }
        });
        FakeBridge {
            path,
            lines,
            _directory: directory,
        }
    }

    // What a bridge from before protocol 1 answers any line naming a version.
    fn old_bridge() -> Value {
        json!({ "outcome": "invalid", "message": "expected a call" })
    }

    fn arguments(value: Value) -> Map<String, Value> {
        let Value::Object(arguments) = value else {
            panic!("expected an object, got {value}");
        };
        arguments
    }

    impl FakeBridge {
        fn lines(&self) -> Vec<Value> {
            self.lines.lock().expect("lock the lines").clone()
        }
    }
}
