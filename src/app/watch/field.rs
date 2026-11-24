use serde::{Deserialize, Deserializer};

// serde would read a missing `Option` field as `None`, but the stored schemas require the field to be present.
// Through `deserialize_with` a missing field fails, while a null one is still `None`.
pub(crate) fn nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

// Paired with `#[serde(default)]`, the field may be left out but is never null.
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
