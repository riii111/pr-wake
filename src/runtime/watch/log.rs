use jiff::Timestamp;
use serde_json::Value;

use crate::{app::time, runtime::output};

// One line of the watch log, read by people and by searches over the event names.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Entry {
    pub(crate) event: &'static str,
    pub(crate) fields: Vec<(&'static str, Value)>,
}

pub(crate) trait Log {
    fn write(&self, entry: Entry);
}

// launchd sends the agent's standard output to its log file.
pub(crate) struct StdoutLog;

impl Entry {
    #[must_use]
    pub(crate) const fn new(event: &'static str) -> Self {
        Self {
            event,
            fields: Vec::new(),
        }
    }

    #[must_use]
    pub(crate) fn with(mut self, key: &'static str, value: impl Into<Value>) -> Self {
        self.fields.push((key, value.into()));
        self
    }

    // The time and the event come first, as the TypeScript watch wrote them, and the fields keep
    // the order they were given in.
    #[must_use]
    pub(crate) fn line(&self, at: Timestamp) -> String {
        let fields = [
            ("time", Value::from(time::format_timestamp(at))),
            ("event", Value::from(self.event)),
        ];
        let members = fields
            .iter()
            .map(|(key, value)| (*key, value))
            .chain(self.fields.iter().map(|(key, value)| (*key, value)))
            .map(|(key, value)| format!("{}:{value}", Value::from(key)))
            .collect::<Vec<_>>();
        format!("{{{}}}", members.join(","))
    }
}

impl Log for StdoutLog {
    fn write(&self, entry: Entry) {
        output::write_stdout(&format!("{}\n", entry.line(Timestamp::now())));
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn line_is_one_json_object_with_the_time_and_event_first() {
        let entry = Entry::new("queued")
            .with("pr", "https://github.com/org/pay/pull/7")
            .with("kinds", json!(["finding", "conflict"]))
            .with("note", "a \"quoted\" word");
        let at = "2026-10-23T12:00:00Z".parse().expect("parse the time");

        let line = entry.line(at);

        assert_eq!(
            line,
            r#"{"time":"2026-10-23T12:00:00.000Z","event":"queued","pr":"https://github.com/org/pay/pull/7","kinds":["finding","conflict"],"note":"a \"quoted\" word"}"#
        );
        assert_eq!(
            serde_json::from_str::<Value>(&line).expect("one JSON object")["note"],
            "a \"quoted\" word"
        );
    }
}
