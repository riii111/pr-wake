use jiff::{Timestamp, fmt::temporal::DateTimePrinter};

// The records hold `Date.toISOString()` times with exactly three fraction digits, while jiff drops trailing zeros.
const RECORD_TIME: DateTimePrinter = DateTimePrinter::new().precision(Some(3));

#[must_use]
pub(crate) fn format_timestamp(timestamp: Timestamp) -> String {
    RECORD_TIME.timestamp_to_string(&timestamp)
}

#[must_use]
pub(crate) fn is_at_or_after(time: &str, reference: Timestamp) -> bool {
    // A time GitHub wrote in a form jiff cannot read is taken as earlier, so its comment is never sent as new.
    time.parse::<Timestamp>()
        .is_ok_and(|time| time >= reference)
}

// A record's time for `#[serde(with)]`, written as `format_timestamp` writes it.
pub(crate) mod record {
    use jiff::Timestamp;
    use serde::{Deserialize, Deserializer, Serializer, de::Error as _};

    pub(crate) fn serialize<S: Serializer>(
        at: &Timestamp,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&super::format_timestamp(*at))
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Timestamp, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(D::Error::custom)
    }
}

// As `record`, for a time that may be null.
// Through `with`, serde refuses a record that leaves the field out, so a field older records lack also takes `default`.
pub(crate) mod optional_record {
    use jiff::Timestamp;
    use serde::{Deserialize, Deserializer, Serializer};

    #[expect(clippy::ref_option, reason = "serde passes the field by reference")]
    pub(crate) fn serialize<S: Serializer>(
        at: &Option<Timestamp>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match at {
            Some(at) => super::record::serialize(at, serializer),
            None => serializer.serialize_none(),
        }
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Timestamp>, D::Error> {
        #[derive(Deserialize)]
        struct Recorded(#[serde(with = "super::record")] Timestamp);

        Option::<Recorded>::deserialize(deserializer).map(|at| at.map(|Recorded(at)| at))
    }
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
                "sub_millisecond_truncated",
                "2026-11-12T00:47:00.123999Z",
                "2026-11-12T00:47:00.123Z",
            ),
        ] {
            let timestamp: Timestamp = input.parse().expect("parse the input");

            assert_eq!(format_timestamp(timestamp), expected, "case: {name}");
        }
    }

    #[test]
    fn time_is_at_or_after_only_when_it_parses() {
        let reference = "2026-11-12T00:47:00Z".parse().expect("parse the reference");
        for (name, time, expected) in [
            ("later", "2026-11-12T00:47:01Z", true),
            ("same_instant", "2026-11-12T09:47:00+09:00", true),
            ("earlier", "2026-11-12T00:46:59Z", false),
            ("unreadable_time", "soon", false),
        ] {
            assert_eq!(is_at_or_after(time, reference), expected, "case: {name}");
        }
    }
}
