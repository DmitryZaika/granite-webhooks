use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serializer;

/// Serializes like JavaScript's `Date.prototype.toJSON`
/// (`2025-01-02T03:04:05.000Z`), which is what Remix loaders produced.
pub fn js_date<S: Serializer>(
    value: &Option<DateTime<Utc>>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match value {
        Some(date) => serializer.serialize_str(&date.to_rfc3339_opts(SecondsFormat::Millis, true)),
        None => serializer.serialize_none(),
    }
}

/// [`js_date`] for a date that is always set.
pub fn js_date_required<S: Serializer>(
    value: &DateTime<Utc>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&value.to_rfc3339_opts(SecondsFormat::Millis, true))
}

/// For PATCH bodies: a field left out is `None`, a field sent as `null` is
/// `Some(None)`, a value is `Some(Some(value))`. Use with `#[serde(default)]`.
pub fn double_option<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: serde::Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    serde::Deserialize::deserialize(deserializer).map(Some)
}
