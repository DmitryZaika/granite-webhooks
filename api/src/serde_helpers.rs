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
