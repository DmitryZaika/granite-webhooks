//! Calls saved from `CloudTalk` and `RingCentral` (`provider_calls`), so a
//! company's call history stays after it changes provider. The call-history
//! job saves them; `GET /v1/calls/{provider}` lists them next to the live
//! provider's.

use super::id_text;
use chrono::{DateTime, NaiveDateTime, Utc};
use serde_json::Value;
use sqlx::MySqlPool;

/// Most saved calls one list answers (newest first).
const LIMIT: i64 = 500;

/// `Cdr.started_at`: RFC 3339 (`RingCentral`) or `YYYY-MM-DD HH:MM:SS` in
/// UTC (`CloudTalk`).
fn started_at(text: &str) -> Option<NaiveDateTime> {
    DateTime::parse_from_rfc3339(text)
        .map(|at| at.naive_utc())
        .ok()
        .or_else(|| NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S").ok())
}

/// Saves `items` (calls in the `CloudTalk` shape) of `provider`, refreshing a
/// call saved before. A call without an id, a 10-digit outside number or a
/// start time is skipped. How many were saved.
pub async fn save(
    pool: &MySqlPool,
    company_id: i32,
    provider: &str,
    items: &[Value],
) -> Result<usize, sqlx::Error> {
    let rows: Vec<(String, String, NaiveDateTime, String)> = items
        .iter()
        .filter_map(|item| {
            let cdr = &item["Cdr"];
            let call_id = id_text(&cdr["id"]);
            let digits =
                crate::phone::numbers::last10(cdr["public_external"].as_str().unwrap_or(""))?;
            let started = started_at(cdr["started_at"].as_str()?)?;
            (!call_id.is_empty()).then(|| (call_id, digits, started, item.to_string()))
        })
        .collect();
    for chunk in rows.chunks(200) {
        let mut builder = sqlx::QueryBuilder::<sqlx::MySql>::new(
            "INSERT INTO provider_calls (company_id, provider, call_id, customer_digits, started_at, item) ",
        );
        builder.push_values(chunk, |mut row, (call_id, digits, started, item)| {
            row.push_bind(company_id)
                .push_bind(provider)
                .push_bind(call_id.as_str())
                .push_bind(digits.as_str())
                .push_bind(*started)
                .push_bind(item.as_str());
        });
        builder.push(
            " AS new ON DUPLICATE KEY UPDATE customer_digits = new.customer_digits,
                started_at = new.started_at, item = new.item",
        );
        builder.build().execute(pool).await?;
    }
    Ok(rows.len())
}

/// A saved call: its provider, its id and the call.
pub struct SavedCall {
    pub provider: String,
    pub call_id: String,
    pub item: Value,
}

/// The company's saved calls with any of `numbers` (last 10 digits) from
/// `from` until before `to`, newest first.
pub async fn list(
    pool: &MySqlPool,
    company_id: i32,
    numbers: &[String],
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
) -> Result<Vec<SavedCall>, sqlx::Error> {
    if numbers.is_empty() {
        return Ok(Vec::new());
    }
    let mut builder = sqlx::QueryBuilder::<sqlx::MySql>::new(
        "SELECT CAST(provider AS CHAR), call_id, CAST(item AS CHAR) FROM provider_calls
          WHERE company_id = ",
    );
    builder
        .push_bind(company_id)
        .push(" AND customer_digits IN (");
    let mut each = builder.separated(", ");
    for number in numbers {
        each.push_bind(number.as_str());
    }
    each.push_unseparated(")");
    if let Some(from) = from {
        builder
            .push(" AND started_at >= ")
            .push_bind(from.naive_utc());
    }
    if let Some(to) = to {
        builder.push(" AND started_at < ").push_bind(to.naive_utc());
    }
    builder
        .push(" ORDER BY started_at DESC LIMIT ")
        .push_bind(LIMIT);
    let rows: Vec<(String, String, String)> = builder.build_query_as().fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .filter_map(|(provider, call_id, item)| {
            serde_json::from_str(&item).ok().map(|item| SavedCall {
                provider,
                call_id,
                item,
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_providers_start_times_read() {
        assert_eq!(
            started_at("2026-10-23T16:16:04.000Z").map(|at| at.to_string()),
            Some("2026-10-23 16:16:04".into())
        );
        assert_eq!(
            started_at("2026-10-12 20:42:37").map(|at| at.to_string()),
            Some("2026-10-12 20:42:37".into())
        );
        assert_eq!(started_at(""), None);
    }
}
