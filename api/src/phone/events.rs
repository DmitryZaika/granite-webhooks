//! Telnyx's callbacks, each claimed once.
//!
//! Callbacks are kept 30 days (`phone_call_events`; the phone job deletes
//! older ones). Each one is claimed by its dedupe key before it sets
//! anything off, so a callback Telnyx sends again, or twice at once, runs
//! its side effects once.
//!
//! The key is the SHA-256 (hex) of Call Control's `data.id`, or of the
//! TeXML `CallSid|type|CallStatus|RecordingSid|DialCallStatus`, where `type`
//! names the route (and its step). A callback without the id gets a random
//! key: it is stored, never deduplicated.

use crate::routes::auth::sha256_hex;
use chrono::{DateTime, Utc};
use sqlx::{MySqlExecutor, MySqlPool};

/// Most characters of `event_type` kept.
const EVENT_TYPE_CHARS: usize = 64;

/// Which Telnyx API sent a callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// TeXML (form-encoded).
    Texml,
    /// Call Control v2 (JSON).
    CallControl,
}

impl Source {
    /// As stored.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Texml => "texml",
            Self::CallControl => "call_control",
        }
    }
}

/// A key no other callback has.
fn random_key() -> String {
    sha256_hex(&uuid::Uuid::new_v4().to_string())
}

/// The dedupe key of a TeXML callback of type `kind` (e.g. `dial-result`).
pub fn texml_key(
    call_sid: Option<&str>,
    kind: &str,
    call_status: Option<&str>,
    recording_sid: Option<&str>,
    dial_call_status: Option<&str>,
) -> String {
    let Some(call_sid) = call_sid.map(str::trim).filter(|sid| !sid.is_empty()) else {
        return random_key();
    };
    sha256_hex(&format!(
        "{call_sid}|{kind}|{}|{}|{}",
        call_status.unwrap_or_default(),
        recording_sid.unwrap_or_default(),
        dial_call_status.unwrap_or_default()
    ))
}

/// The dedupe key of a Call Control event (its `data.id`).
pub fn call_control_key(event_id: Option<&str>) -> String {
    event_id
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map_or_else(random_key, sha256_hex)
}

/// One callback to store.
#[derive(Debug, Clone, Copy)]
pub struct Event<'a> {
    pub company_id: i32,
    pub call_id: Option<i64>,
    pub source: Source,
    /// The route (TeXML) or Telnyx's event type (Call Control).
    pub event_type: &'a str,
    pub dedupe_key: &'a str,
    pub occurred_at: Option<DateTime<Utc>>,
    /// The body as Telnyx sent it.
    pub payload: &'a str,
}

/// Stores the event unless one with its key is there already: whether it
/// is new (claimed), so its side effects are this request's to run.
pub async fn claim<'e>(executor: impl MySqlExecutor<'e>, event: &Event<'_>) -> sqlx::Result<bool> {
    let event_type: String = event.event_type.chars().take(EVENT_TYPE_CHARS).collect();
    let inserted = sqlx::query(
        "INSERT INTO phone_call_events
           (company_id, call_id, source, event_type, dedupe_key, occurred_at, payload)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(event.company_id)
    .bind(event.call_id)
    .bind(event.source.as_str())
    .bind(event_type)
    .bind(event.dedupe_key)
    .bind(event.occurred_at)
    .bind(event.payload)
    .execute(executor)
    .await;
    match inserted {
        Ok(_) => Ok(true),
        Err(err)
            if err
                .as_database_error()
                .is_some_and(sqlx::error::DatabaseError::is_unique_violation) =>
        {
            Ok(false)
        }
        Err(err) => Err(err),
    }
}

/// Gives a claim back when its handling failed before it changed anything,
/// so Telnyx's retry runs it.
pub async fn release(pool: &MySqlPool, dedupe_key: &str) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM phone_call_events WHERE dedupe_key = ?")
        .bind(dedupe_key)
        .execute(pool)
        .await?;
    Ok(())
}

/// Links a stored event to the call it turned out to be about.
pub async fn link_call<'e>(
    executor: impl MySqlExecutor<'e>,
    dedupe_key: &str,
    call_id: i64,
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE phone_call_events SET call_id = ? WHERE dedupe_key = ? AND call_id IS NULL",
    )
    .bind(call_id)
    .bind(dedupe_key)
    .execute(executor)
    .await?;
    Ok(())
}

/// `Timestamp` / `occurred_at` as Telnyx writes it (RFC 3339, or RFC 2822
/// as TeXML's Twilio-style callbacks may); `None` when missing or
/// unreadable.
pub fn parse_time(text: Option<&str>) -> Option<DateTime<Utc>> {
    let text = text?.trim();
    DateTime::parse_from_rfc3339(text)
        .or_else(|_| DateTime::parse_from_rfc2822(text))
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_stable_per_callback_and_random_without_an_id() {
        let a = texml_key(
            Some("v3:1"),
            "dial-result",
            Some("in-progress"),
            None,
            Some("no-answer"),
        );
        assert_eq!(
            a,
            texml_key(
                Some(" v3:1 "),
                "dial-result",
                Some("in-progress"),
                None,
                Some("no-answer")
            )
        );
        assert_ne!(
            a,
            texml_key(
                Some("v3:1"),
                "dial-result:cell",
                Some("in-progress"),
                None,
                Some("no-answer")
            )
        );
        assert_eq!(a.len(), 64);
        assert_ne!(
            texml_key(None, "notice", None, None, None),
            texml_key(None, "notice", None, None, None)
        );
        assert_eq!(call_control_key(Some("evt-1")), sha256_hex("evt-1"));
        assert_ne!(call_control_key(Some(" ")), call_control_key(Some(" ")));
        assert_eq!(
            parse_time(Some("2026-10-18T10:00:00.250Z")).map(|at| at.timestamp_millis()),
            Some(1_792_317_600_250)
        );
        assert_eq!(parse_time(Some("soon")), None);
        assert_eq!(
            parse_time(Some("Sun, 18 Oct 2026 10:00:00 +0000")).map(|at| at.timestamp()),
            Some(1_792_317_600)
        );
    }
}
