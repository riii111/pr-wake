use jiff::{Timestamp, fmt::temporal::DateTimePrinter};

// The records hold `Date.toISOString()` times with exactly three fraction digits, while jiff's
// own display drops trailing zeros.
const RECORD_TIME: DateTimePrinter = DateTimePrinter::new().precision(Some(3));

#[must_use]
pub(crate) fn format_timestamp(timestamp: Timestamp) -> String {
    RECORD_TIME.timestamp_to_string(&timestamp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_is_written_like_to_iso_string() {
        for (name, input, expected) in [
            (
                "whole_second",
                "2026-11-12T00:47:00Z",
                "2026-11-12T00:47:00.000Z",
            ),
            (
                "milliseconds",
                "2026-11-12T00:47:00.120Z",
                "2026-11-12T00:47:00.120Z",
            ),
            (
                "sub_millisecond_truncated",
                "2026-11-12T00:47:00.123999Z",
                "2026-11-12T00:47:00.123Z",
            ),
            (
                "offset_converted_to_utc",
                "2026-11-12T09:47:00.5+09:00",
                "2026-11-12T00:47:00.500Z",
            ),
        ] {
            let timestamp: Timestamp = input.parse().expect("parse the input");

            assert_eq!(format_timestamp(timestamp), expected, "case: {name}");
        }
    }

    // GitHub writes whole seconds in UTC, the TypeScript watch milliseconds in UTC, and the Go
    // review nanoseconds with the local offset.
    #[test]
    fn stored_and_github_times_parse_to_the_same_instant() {
        let parsed = [
            "2026-11-12T00:47:00Z",
            "2026-11-12T00:47:00.000Z",
            "2026-11-12T09:47:00.000000000+09:00",
        ]
        .map(|text| text.parse::<Timestamp>().expect("parse the time"));

        assert!(
            parsed.iter().all(|timestamp| *timestamp == parsed[0]),
            "{parsed:?}"
        );
    }
}
