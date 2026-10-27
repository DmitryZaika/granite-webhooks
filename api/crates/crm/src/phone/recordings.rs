//! Copying a call's recording from Telnyx to our private bucket
//! (`PHONE_RECORDINGS_BUCKET`), as soon as Telnyx says it is ready (its
//! download links last 10 minutes).
//!
//! The recording is looked up with our key (`GET /v2/recordings/{id}`;
//! never a link from the callback), and must be the call's: one of the
//! call ids Telnyx gives for it (its Call Control id, leg id or call
//! session) is one of the call's (the TeXML `CallSid`, the app's leg, the
//! session, a leg the call rang), and its connection, when Telnyx names
//! one, is the company's. Otherwise nothing is stored or deleted. Then the
//! attempt is counted (before the download, so a copy that never finishes
//! counts too), the MP3 downloaded, written once to
//! `phone/{company}/{yyyy}/{mm}/{call id}-{kind}.mp3` (the month the call
//! started, UTC) and checked: our object's size must match what we
//! downloaded. Only then is Telnyx's copy deleted; a delete that fails is
//! retried by the phone job (`telnyx_deleted_at`). A copy that fails leaves
//! the call's recording `pending`, and the phone job tries again (5
//! attempts in all).

use super::company::CompanyConfig;
use super::wire_enum;
use crate::state::AppState;
use crate::storage::StorageError;
use crate::telnyx::{Recording, TelnyxError};
use chrono::{DateTime, Datelike, Utc};
use lambda_http::tracing;
use sqlx::MySqlPool;

/// The largest recording we copy (about two hours of dual-channel MP3).
pub const MAX_RECORDING_BYTES: usize = 100 * 1024 * 1024;

/// How long a link to a recording works, in seconds.
pub const LINK_SECS: i64 = 300;

/// A link to a call's recording in our bucket.
#[derive(Debug, Clone)]
pub struct Link {
    /// Presigned; works [`LINK_SECS`].
    pub url: String,
    /// `audio/mpeg`, or `audio/wav` for a `.wav` key.
    pub content_type: &'static str,
    /// When it stops working.
    pub expires_at: DateTime<Utc>,
}

/// A link (5 minutes) to the recording at `key` of the recordings bucket;
/// `None` when the server has no recordings bucket. The app's call details
/// and the CRM's call panels play recordings through it.
pub async fn link(state: &AppState, key: &str) -> Result<Option<Link>, StorageError> {
    let Some(bucket) = state.phone.recordings_bucket.as_deref() else {
        return Ok(None);
    };
    let wav = std::path::Path::new(key)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("wav"));
    let content_type = if wav { "audio/wav" } else { "audio/mpeg" };
    let url = state
        .storage
        .presign_get_in_for(
            Some(bucket),
            key,
            content_type,
            None,
            std::time::Duration::from_secs(LINK_SECS.unsigned_abs()),
        )
        .await?;
    Ok(Some(Link {
        url,
        content_type,
        expires_at: Utc::now() + chrono::Duration::seconds(LINK_SECS),
    }))
}

wire_enum! {
    /// What a recording is of: `call` (a conversation) or `voicemail`.
    pub enum RecordingKind {
        /// The call itself.
        Call = "call",
        /// A voicemail the caller left.
        Voicemail = "voicemail",
    }
}

/// Where a call's recording is kept in our bucket.
pub fn key(
    company_id: i32,
    call_id: i64,
    started_at: DateTime<Utc>,
    kind: RecordingKind,
) -> String {
    format!(
        "phone/{company_id}/{:04}/{:02}/{call_id}-{}.mp3",
        started_at.year(),
        started_at.month(),
        kind.as_str()
    )
}

/// How a copy went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyOutcome {
    /// In our bucket now (and Telnyx's copy deleted, or that is retried).
    Copied,
    /// The call's recording was copied before.
    AlreadyCopied,
    /// Not copied this time (the reason, for the log); the job retries.
    Failed(&'static str),
    /// Not the call's recording (Telnyx says it is of another call or
    /// company): nothing was stored, deleted or retried.
    NotTheCalls,
}

/// The call a recording is copied for.
#[derive(sqlx::FromRow)]
struct CallRow {
    started_at: DateTime<Utc>,
    recording_status: String,
    attempts: i64,
    telnyx_call_sid: Option<String>,
    telnyx_call_control_id: Option<String>,
    telnyx_call_session_id: Option<String>,
}

/// Whether Telnyx's `recording` is of the call with these `ids` in the
/// company of `config`: one of its call ids is one of the call's, and its
/// connection (when Telnyx names one) is one of the company's (when it set
/// any).
fn belongs(recording: &Recording, ids: &[String], config: Option<&CompanyConfig>) -> bool {
    let ours: Vec<&str> = config
        .map(|config| {
            [&config.texml_app_id, &config.credential_connection_id]
                .into_iter()
                .filter_map(|id| id.as_deref())
                .collect()
        })
        .unwrap_or_default();
    let connection_ok = recording
        .connection_id
        .as_deref()
        .is_none_or(|theirs| ours.is_empty() || ours.contains(&theirs));
    connection_ok
        && recording
            .call_ids()
            .any(|id| ids.iter().any(|ours| ours == id))
}

/// The ids Telnyx knows the call by: its TeXML `CallSid`, the app's leg and
/// call session, and the legs it rang.
async fn call_ids(pool: &MySqlPool, call_id: i64, call: &CallRow) -> sqlx::Result<Vec<String>> {
    let mut ids: Vec<String> = [
        &call.telnyx_call_sid,
        &call.telnyx_call_control_id,
        &call.telnyx_call_session_id,
    ]
    .into_iter()
    .flatten()
    .cloned()
    .collect();
    let legs: Vec<String> = sqlx::query_scalar(
        "SELECT leg_call_sid FROM phone_call_legs WHERE call_id = ? AND leg_call_sid IS NOT NULL",
    )
    .bind(call_id)
    .fetch_all(pool)
    .await?;
    ids.extend(legs);
    Ok(ids)
}

/// Copies Telnyx's recording `recording_id` of a call of the company.
/// `duration_secs` is the length Telnyx reported, if any. Database errors
/// are the caller's; everything else is a [`CopyOutcome`].
pub async fn copy(
    state: &AppState,
    company_id: i32,
    call_id: i64,
    recording_id: &str,
    kind: RecordingKind,
    duration_secs: Option<i64>,
) -> sqlx::Result<CopyOutcome> {
    let pool = &state.pool;
    let call: Option<CallRow> = sqlx::query_as(
        "SELECT started_at, CAST(recording_status AS CHAR) AS recording_status,
                CAST(recording_attempts AS SIGNED) AS attempts, telnyx_call_sid,
                telnyx_call_control_id, telnyx_call_session_id
           FROM phone_calls WHERE id = ? AND company_id = ?",
    )
    .bind(call_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    let Some(call) = call else {
        return Ok(CopyOutcome::Failed("no such call"));
    };
    if call.recording_status == "copied" {
        return Ok(CopyOutcome::AlreadyCopied);
    }
    let recording_id: String = recording_id.trim().chars().take(128).collect();
    let duration = duration_secs.and_then(|secs| i32::try_from(secs).ok());
    let looked_up = match state.phone.recordings_bucket {
        Some(_) => Some(state.telnyx.get_recording(&recording_id).await),
        None => None,
    };
    if let Some(Ok(recording)) = &looked_up {
        let config = super::company::load(pool, company_id).await?;
        let ids = call_ids(pool, call_id, &call).await?;
        if !belongs(recording, &ids, config.as_ref()) {
            tracing::warn!(call_id, "a recording of another call was named; not copied");
            return Ok(CopyOutcome::NotTheCalls);
        }
    }
    // The attempt counts now (so a copy that never finishes counts too),
    // and claims the copy: another one started meanwhile wins.
    let claimed = sqlx::query(
        "UPDATE phone_calls
            SET recording_status = 'pending',
                recording_telnyx_id = IF(? OR recording_telnyx_id IS NULL, ?, recording_telnyx_id),
                recording_secs = COALESCE(?, recording_secs),
                recording_attempts = LEAST(recording_attempts + 1, 255),
                updated_at = UTC_TIMESTAMP()
          WHERE id = ? AND company_id = ? AND recording_status <> 'copied'
            AND recording_attempts = ?",
    )
    .bind(matches!(looked_up, Some(Ok(_))))
    .bind(&recording_id)
    .bind(duration)
    .bind(call_id)
    .bind(company_id)
    .bind(call.attempts)
    .execute(pool)
    .await?
    .rows_affected();
    if claimed == 0 {
        return Ok(CopyOutcome::Failed("another copy is under way"));
    }
    let recording = match looked_up {
        Some(Ok(recording)) => recording,
        Some(Err(err)) => {
            let reason = if matches!(err, TelnyxError::NotConfigured) {
                "TELNYX_API_KEY is not set"
            } else {
                "Telnyx did not give the recording"
            };
            tracing::warn!(call_id, reason, "recording not copied; the job retries");
            return Ok(CopyOutcome::Failed(reason));
        }
        None => {
            tracing::warn!(
                call_id,
                "PHONE_RECORDINGS_BUCKET is not set; the job retries"
            );
            return Ok(CopyOutcome::Failed("PHONE_RECORDINGS_BUCKET is not set"));
        }
    };
    let key = key(company_id, call_id, call.started_at, kind);
    match transfer(state, &recording, &key).await {
        Ok(()) => {
            // Transcribed for its AI note (the user's choice; voicemails
            // always), and an answered outgoing call for its call points and
            // call check whatever the AI notes say.
            sqlx::query(
                "UPDATE phone_calls
                    SET recording_status = 'copied', recording_key = ?, recording_telnyx_id = ?,
                        recording_secs = COALESCE(?, recording_secs),
                        transcript_status = CASE
                            WHEN transcript_status = 'none'
                             AND (? OR ai_notes <> 0
                                  OR (direction = 'outbound' AND answered_at IS NOT NULL))
                            THEN 'pending'
                            ELSE transcript_status END
                  WHERE id = ? AND company_id = ?",
            )
            .bind(&key)
            .bind(&recording_id)
            .bind(
                recording
                    .duration_secs
                    .and_then(|secs| i32::try_from(secs).ok()),
            )
            .bind(kind == RecordingKind::Voicemail)
            .bind(call_id)
            .bind(company_id)
            .execute(pool)
            .await?;
            delete_telnyx_copy(state, call_id, &recording_id).await?;
            Ok(CopyOutcome::Copied)
        }
        Err(reason) => {
            tracing::warn!(call_id, reason, "recording not copied; the job retries");
            Ok(CopyOutcome::Failed(reason))
        }
    }
}

/// Deletes Telnyx's copy of the call's recording (ours is in the bucket):
/// done, or the try counted for the phone job's retry. Whether it is gone.
pub async fn delete_telnyx_copy(
    state: &AppState,
    call_id: i64,
    recording_id: &str,
) -> sqlx::Result<bool> {
    match state.telnyx.delete_recording(recording_id).await {
        Ok(()) => {
            sqlx::query("UPDATE phone_calls SET telnyx_deleted_at = UTC_TIMESTAMP() WHERE id = ?")
                .bind(call_id)
                .execute(&state.pool)
                .await?;
            Ok(true)
        }
        Err(err) => {
            tracing::warn!(call_id, error = %err, "Telnyx's copy of a recording not deleted; the job retries");
            sqlx::query(
                "UPDATE phone_calls
                    SET telnyx_delete_attempts = LEAST(telnyx_delete_attempts + 1, 65535),
                        telnyx_delete_tried_at = UTC_TIMESTAMP()
                  WHERE id = ?",
            )
            .bind(call_id)
            .execute(&state.pool)
            .await?;
            Ok(false)
        }
    }
}

/// Telnyx's recording into our bucket at `key`; else why not.
async fn transfer(state: &AppState, recording: &Recording, key: &str) -> Result<(), &'static str> {
    let bucket = state
        .phone
        .recordings_bucket
        .as_deref()
        .ok_or("PHONE_RECORDINGS_BUCKET is not set")?;
    let url = recording
        .mp3_url
        .as_deref()
        .ok_or("Telnyx has no MP3 of the recording")?;
    let bytes = state
        .telnyx
        .download(url, MAX_RECORDING_BYTES)
        .await
        .map_err(|_| "the recording did not download whole")?;
    let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    state
        .storage
        .put_private(bucket, key, bytes, "audio/mpeg")
        .await
        .map_err(|_| "the recording was not stored")?;
    let stored = state
        .storage
        .size_in(Some(bucket), key)
        .await
        .map_err(|_| "the stored recording was not checked")?;
    if stored != Some(size) {
        return Err("the stored recording's size does not match");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_name_the_company_month_call_and_kind() {
        let started = DateTime::parse_from_rfc3339("2026-03-31T23:59:59Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            key(100, 980_001, started, RecordingKind::Voicemail),
            "phone/100/2026/03/980001-voicemail.mp3"
        );
        assert_eq!(
            key(7, 1, started, RecordingKind::Call),
            "phone/7/2026/03/1-call.mp3"
        );
    }

    #[test]
    fn a_recording_must_name_one_of_the_calls_ids() {
        let recording = |ids: [Option<&str>; 3], connection: Option<&str>| Recording {
            call_control_id: ids[0].map(str::to_string),
            call_leg_id: ids[1].map(str::to_string),
            call_session_id: ids[2].map(str::to_string),
            connection_id: connection.map(str::to_string),
            ..Recording::default()
        };
        let ids = vec!["v3:parent".to_string(), "leg-2".to_string()];
        assert!(belongs(
            &recording([Some("v3:parent"), None, None], None),
            &ids,
            None
        ));
        assert!(belongs(
            &recording([Some("v3:x"), Some("leg-2"), None], None),
            &ids,
            None
        ));
        // Another call's, or naming no call at all.
        assert!(!belongs(
            &recording([Some("v3:other"), None, Some("s-9")], None),
            &ids,
            None
        ));
        assert!(!belongs(&recording([None, None, None], None), &ids, None));
        assert!(!belongs(
            &recording([Some("v3:parent"), None, None], None),
            &[],
            None
        ));
    }
}
