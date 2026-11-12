use serde::{Deserialize, Deserializer};

// serde reads a missing `Option` field as `None`; through `deserialize_with` the field must be
// present, as the stored schemas require, while it may still be null.
pub(crate) fn nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

// For a field that may be left out, with `#[serde(default)]`, but is never null.
pub(crate) fn present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

// A field that may be left out, told apart from one recorded as null.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) enum Recorded<T> {
    #[default]
    Absent,
    Present(T),
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Recorded<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        T::deserialize(deserializer).map(Self::Present)
    }
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    #[derive(Debug, PartialEq, Eq, Deserialize)]
    struct Record {
        #[serde(deserialize_with = "super::nullable")]
        reason: Option<String>,
        #[serde(default, deserialize_with = "super::present")]
        kind: Option<String>,
    }

    #[test]
    fn nullable_field_takes_null_but_not_absence() {
        for (name, input, expected) in [
            (
                "null",
                r#"{"reason":null}"#,
                Some(Record {
                    reason: None,
                    kind: None,
                }),
            ),
            (
                "text",
                r#"{"reason":"why"}"#,
                Some(Record {
                    reason: Some("why".to_owned()),
                    kind: None,
                }),
            ),
            ("missing", "{}", None),
        ] {
            let parsed = serde_json::from_str::<Record>(input).ok();

            assert_eq!(parsed, expected, "case: {name}");
        }
    }

    #[test]
    fn present_field_takes_absence_but_not_null() {
        for (name, input, expected) in [
            (
                "missing",
                r#"{"reason":null}"#,
                Some(Record {
                    reason: None,
                    kind: None,
                }),
            ),
            (
                "text",
                r#"{"reason":null,"kind":"inline"}"#,
                Some(Record {
                    reason: None,
                    kind: Some("inline".to_owned()),
                }),
            ),
            ("null", r#"{"reason":null,"kind":null}"#, None),
        ] {
            let parsed = serde_json::from_str::<Record>(input).ok();

            assert_eq!(parsed, expected, "case: {name}");
        }
    }
}
