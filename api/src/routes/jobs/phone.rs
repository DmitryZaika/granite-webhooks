//! `POST /v1/jobs/phone`: Granite Phone's upkeep.
//!
//! A bounded amount each run (the `time-triggered` Lambda calls it every
//! minute). A run starts no new work after [`RUN_BUDGET`], so it ends
//! inside the 30 s its caller waits; every step claims its rows, so an
//! overlapping run never repeats work.
//!
//! In order:
//! 1. signs out the phones of users who may no longer use Granite Phone
//!    (deleted, moved to another company, no longer an employee or admin,
//!    or their company deleted), and the sign-ins that expired (12 months)
//!    or idled 30 days;
//! 2. deletes the Telnyx credentials of signed-out phones that are still on
//!    Telnyx (a delete that failed before; each failure waits longer before
//!    the next try, up to a day);
//! 3. deletes Telnyx's callbacks older than 30 days (`phone_call_events`);
//! 4. closes calls Telnyx never reported the end of (no end after 6 hours):
//!    answered ones `answered`, else incoming ones `missed` and outgoing ones
//!    `failed`, without a talk time;
//! 5. the customer phone index calls are matched by (`phone::customer_index`):
//!    rewrites the rows of customers changed or added since the last run,
//!    then fills the index of companies that have none yet (a batch a run);
//! 6. AI notes: claims up to 3 calls waiting for one (a claim lasts 15
//!    minutes; 5 attempts in all), transcribes the recording from our
//!    bucket (speaker labels on), writes the note with the CRM's call-note
//!    prompts (the voicemail prompt for voicemails), tidies it the way the
//!    CRM does (`call_notes::sanitize`), adds it to the call's deal (by
//!    "Granite Phone"; the deal's people are notified as for a CRM note)
//!    and keeps it on the call (`aiNote`); a rep's answered call of 31 to 89
//!    seconds with a customer also gets the CRM's call check on that
//!    transcript (a conversation stops their flows and moves the deal on);
//!    a voicemail left for us is noted with the inbound voicemail prompt.
//!    An answered outgoing call of a user with AI notes off is transcribed
//!    for its call points and call check only (over 30 seconds of talk; no
//!    note, `transcriptStatus` back to `none`);
//! 7. call points for answered outbound calls that ended in the last 3 days
//!    (`phone_call_points`, provider `telnyx`; emergency calls left out):
//!    30 seconds of talk or less is an attempt, else the transcript tells a
//!    voicemail from a conversation; a call whose transcript is still coming
//!    waits as `pending`, and is looked at again each run;
//! 8. deletes Telnyx's copies of recordings we copied whose delete failed
//!    (backing off like the credentials);
//! 9. retries the recording copies that failed or never finished (5
//!    attempts in all, each counted before it starts; 5 minutes apart);
//! 10. port-in orders (`phone::porting::run_job`): sets up the numbers
//!    of ported orders still to do, then reads open orders from Telnyx again
//!    (a few a run; Telnyx's porting webhooks are the fast path);
//! 11. last, 10DLC texting registrations (`phone::tendlc_registry::run_job`):
//!    reads sent ones from Telnyx again (a sent one whose brand is now
//!    registered gets its campaign) and the number assignments still pending
//!    (a few a run; Telnyx's 10DLC webhooks are the fast path).

use super::JobCaller;
use crate::ai::{AiError, Caller, Chat, MINI_MODEL};
use crate::call_notes;
use crate::error::{ApiResult, ErrorBody};
use crate::openapi::ToSchema;
use crate::phone::calls::{CallDirection, CallStatus, NOT_TEST_CALL, TranscriptStatus};
use crate::phone::customer_index;
use crate::phone::outcomes::{CONVERSATION_SECS, move_out_of_first_stage};
use crate::phone::recordings::{self, CopyOutcome, RecordingKind};
use crate::phone::revoke::{self, RevokeReason};
use crate::routes::ai::call_note::{CALL_PROMPT, INBOUND_VOICEMAIL_PROMPT, VOICEMAIL_PROMPT};
use crate::routes::deals::service::{Notification, notify_assignees};
use crate::routes::followups::call_check::{CallCheck, CallId, PhoneDigits, check_call};
use crate::state::AppState;
use crate::transcribe::TranscribeError;
use crate::voicemail;
use axum::Json;
use axum::extract::State;
use chrono::{DateTime, Utc};
use lambda_http::tracing;
use serde::Serialize;
use sqlx::{FromRow, MySqlPool};
use std::time::{Duration, Instant};

/// A run starts no new work after this long.
const RUN_BUDGET: Duration = Duration::from_secs(20);
/// Device sessions signed out per run, for lost access and for expiry each.
const SIGN_OUTS_PER_RUN: i64 = 50;
/// Leftover Telnyx credentials (and Telnyx's recording copies) deleted per
/// run.
const DELETES_PER_RUN: i64 = 20;
/// SQL: the minutes a delete that failed `{attempts}` times waits before the
/// next try: 1, 2, 4 ... up to a day.
const BACKOFF_MINUTES: &str = "LEAST(1 << LEAST({attempts}, 11), 1440)";
/// Telnyx callbacks are kept this many days.
const EVENT_DAYS: i64 = 30;
/// Old callbacks deleted per run.
const EVENTS_PER_RUN: i64 = 5_000;
/// A call Telnyx never said the end of is closed this long after it started.
const OPEN_CALL_HOURS: i64 = 6;
/// Open calls closed per run.
const CLOSES_PER_RUN: i64 = 200;
/// Recording copies retried per run (one at a time: up to 100 MB each).
const RECORDINGS_PER_RUN: i64 = 5;
/// Copies of a recording tried in all.
const MAX_RECORDING_ATTEMPTS: i64 = 5;
/// A copy is retried this long after the last one started (and a copy
/// started this recently is left alone: it may be running).
const STUCK_COPY_MINUTES: i64 = 5;
/// Answered outbound calls get their call points within this many days of
/// their end.
const POINTS_DAYS: i64 = 3;
/// AI notes written per run, all at once.
const NOTES_PER_RUN: i64 = 3;
/// A claimed AI note is another run's after this long.
const CLAIM_TTL_MINUTES: i64 = 15;
/// AI notes tried in all.
const MAX_NOTE_ATTEMPTS: i64 = 5;
/// The CRM's `CALL_ATTEMPT_MAX_TALK_SECONDS`.
const ATTEMPT_MAX_TALK_SECONDS: i32 = 30;
/// Answered outbound calls given call points per run.
const POINTS_PER_RUN: i64 = 200;
/// Who the AI notes are by (`deal_notes.created_by`, the notifications'
/// actor).
pub const AI_NOTE_AUTHOR: &str = "Granite Phone";
/// The longest AI note kept (a deal note's limit).
const MAX_NOTE_CHARS: usize = 5000;

/// What a run did.
#[derive(Debug, Default, Serialize, ToSchema)]
pub struct PhoneJobRun {
    /// The work done: the counts below together (old callbacks and the
    /// customer phone index not counted).
    pub processed: usize,
    /// Phones signed out because their user may no longer use Granite
    /// Phone (or their company was deleted).
    pub phones_signed_out: usize,
    /// Phones signed out because their sign-in expired (12 months) or went
    /// unused for 30 days.
    pub sessions_expired: usize,
    /// Telnyx credentials of signed-out phones deleted (left over from a
    /// failed delete).
    pub credentials_deleted: usize,
    /// Telnyx callbacks older than 30 days deleted.
    pub events_purged: u64,
    /// Calls closed because Telnyx never reported their end.
    pub calls_closed: u64,
    /// Customers whose rows in the customer phone index (which matches
    /// calls to customers) were rewritten because they changed, were added
    /// or were deleted since the last run. One that changed in the last 2
    /// minutes is read again by the next run and counted again.
    pub phone_index_refreshed: usize,
    /// Customers read by the first fill of a company's phone index (each
    /// company's index is filled once, at most 5,000 customers a run).
    pub phone_index_filled: usize,
    /// Recordings copied to our bucket on a retry.
    pub recordings_copied: usize,
    /// Recordings given up (5 copies failed, or not the call's).
    pub recordings_failed: usize,
    /// Telnyx's copies of copied recordings deleted on a retry.
    pub telnyx_recordings_deleted: usize,
    /// AI notes written (added to the call, and to its deal when it has one).
    pub notes_written: usize,
    /// Calls with nothing to note (no speech, a mailbox greeting only, or
    /// nothing about the job).
    pub notes_skipped: usize,
    /// AI notes given up (5 attempts failed).
    pub notes_failed: usize,
    /// Answered outbound calls written to the call points.
    pub call_points: usize,
    /// Port-in orders read again from Telnyx (`phone::porting`); left out
    /// when none.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub port_orders_refreshed: usize,
    /// Ported orders whose numbers were added to the company and set up
    /// on Telnyx; left out when none.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub port_orders_completed: usize,
    /// 10DLC texting registrations read again from Telnyx
    /// (`phone::tendlc_registry`); left out when none.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub tendlc_refreshed: usize,
    /// 10DLC number assignments read again from Telnyx; left out when none.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub tendlc_numbers_refreshed: usize,
}

/// Run Granite Phone's upkeep
///
/// Signs out the phones of users who may no longer use Granite Phone (or
/// whose company was deleted) and expired or idle sign-ins, deletes
/// leftover Telnyx credentials of signed-out phones (backing off on
/// failures), deletes Telnyx callbacks older than 30 days, closes calls
/// Telnyx never reported the end of (after 6 hours), keeps the customer
/// phone index that matches calls to customers in step (customers changed
/// or added since the last run; each company's first fill, at most 5,000
/// customers a run), writes up to 3 AI notes (transcript, note tidied like
/// the CRM's, on the call and its deal, the deal's people notified), gives
/// answered outbound calls their call points (provider `telnyx`), deletes
/// Telnyx's copies of copied recordings whose delete failed, retries failed
/// recording copies (5 attempts), brings port-in orders up to date from
/// Telnyx (adding ported numbers to the company), and last reads sent 10DLC
/// texting registrations and pending number assignments again. Bounded work per run, and
/// no new work after 20 seconds; the next run takes the rest. Returns what
/// was done.
#[crate::openapi::path(
    post,
    path = "/v1/jobs/phone",
    operation_id = "run_phone_job",
    tag = "jobs",
    responses(
        (status = 200, description = "What was done", body = PhoneJobRun),
        (status = 401, description = "Missing or wrong job key", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("job_key" = []))
)]
pub async fn run(State(state): State<AppState>, _job: JobCaller) -> ApiResult<Json<PhoneJobRun>> {
    let deadline = Instant::now() + RUN_BUDGET;
    let mut run = PhoneJobRun {
        phones_signed_out: sign_out(&state, Sessions::LostAccess, deadline).await?,
        sessions_expired: sign_out(&state, Sessions::Expired, deadline).await?,
        ..PhoneJobRun::default()
    };
    run.credentials_deleted = delete_leftover_credentials(&state, deadline).await?;
    run.events_purged = purge_events(&state.pool).await?;
    run.calls_closed = close_open_calls(&state.pool).await?;
    (run.phone_index_refreshed, run.phone_index_filled) = phone_index(&state.pool, deadline).await;
    if Instant::now() < deadline {
        let notes = write_notes(&state).await?;
        run.notes_written = notes.written;
        run.notes_skipped = notes.skipped;
        run.notes_failed = notes.failed;
    }
    if Instant::now() < deadline {
        run.call_points = call_points(&state).await?;
    }
    run.telnyx_recordings_deleted = delete_telnyx_recordings(&state, deadline).await?;
    (run.recordings_copied, run.recordings_failed) = retry_recordings(&state, deadline).await?;
    if Instant::now() < deadline {
        let porting = crate::phone::porting::run_job(&state, deadline).await?;
        run.port_orders_refreshed = porting.refreshed;
        run.port_orders_completed = porting.completed;
    }
    if Instant::now() < deadline {
        let tendlc = crate::phone::tendlc_registry::run_job(&state, deadline).await?;
        run.tendlc_refreshed = tendlc.refreshed;
        run.tendlc_numbers_refreshed = tendlc.numbers_refreshed;
    }
    run.processed = run.phones_signed_out
        + run.sessions_expired
        + run.credentials_deleted
        + usize::try_from(run.calls_closed).unwrap_or(usize::MAX)
        + run.recordings_copied
        + run.recordings_failed
        + run.telnyx_recordings_deleted
        + run.notes_written
        + run.notes_skipped
        + run.notes_failed
        + run.call_points
        + run.port_orders_refreshed
        + run.port_orders_completed
        + run.tendlc_refreshed
        + run.tendlc_numbers_refreshed;
    Ok(Json(run))
}

/// A count of nothing done, left out of the run.
#[allow(clippy::trivially_copy_pass_by_ref)] // serde passes a reference
const fn is_zero(count: &usize) -> bool {
    *count == 0
}

/// Which sign-ins step 1 ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sessions {
    /// Of users who may no longer use Granite Phone, or of a deleted
    /// company.
    LostAccess,
    /// Expired (12 months) or unused for 30 days.
    Expired,
}

/// Step 1: the phones of users who may no longer use Granite Phone, and
/// sign-ins that expired or idled (through `revoke_device_session`, so their
/// credentials and push tokens go too).
async fn sign_out(state: &AppState, which: Sessions, deadline: Instant) -> sqlx::Result<usize> {
    let (sessions, reason) = match which {
        Sessions::LostAccess => (
            revoke::lost_access_sessions(&state.pool, None, SIGN_OUTS_PER_RUN).await?,
            RevokeReason::UserRemoved,
        ),
        Sessions::Expired => (
            revoke::expired_sessions(&state.pool, SIGN_OUTS_PER_RUN).await?,
            RevokeReason::Expired,
        ),
    };
    let mut ended = 0;
    for session_id in sessions {
        if Instant::now() >= deadline {
            break;
        }
        if revoke::revoke_device_session(state, session_id, reason).await? {
            ended += 1;
        }
    }
    Ok(ended)
}

/// Step 2: Telnyx credentials of signed-out phones still to delete, the
/// ones never tried first; each failure waits longer before the next try.
async fn delete_leftover_credentials(state: &AppState, deadline: Instant) -> sqlx::Result<usize> {
    if !state.telnyx.is_enabled() {
        return Ok(0);
    }
    let backoff = BACKOFF_MINUTES.replace("{attempts}", "credential_delete_attempts");
    let leftovers: Vec<(i64, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT CAST(id AS SIGNED), telnyx_credential_id FROM phone_devices
          WHERE revoked_at IS NOT NULL AND telnyx_credential_id IS NOT NULL
            AND credential_deleted_at IS NULL
            AND (credential_delete_tried_at IS NULL
                 OR credential_delete_tried_at < UTC_TIMESTAMP() - INTERVAL {backoff} MINUTE)
          ORDER BY credential_delete_tried_at IS NOT NULL, credential_delete_tried_at, id
          LIMIT ?"
    )))
    .bind(DELETES_PER_RUN)
    .fetch_all(&state.pool)
    .await?;
    let mut deleted = 0;
    for (device_id, credential_id) in leftovers {
        if Instant::now() >= deadline {
            break;
        }
        match state.telnyx.delete_credential(&credential_id).await {
            Ok(()) => {
                sqlx::query(
                    "UPDATE phone_devices SET credential_deleted_at = UTC_TIMESTAMP()
                      WHERE id = ? AND credential_deleted_at IS NULL",
                )
                .bind(device_id)
                .execute(&state.pool)
                .await?;
                deleted += 1;
            }
            Err(err) => {
                tracing::warn!(device_id, error = %err, "Telnyx credential still not deleted; a later run retries");
                revoke::credential_delete_failed(&state.pool, device_id).await?;
            }
        }
    }
    Ok(deleted)
}

/// Step 3: Telnyx's callbacks older than 30 days.
async fn purge_events(pool: &MySqlPool) -> sqlx::Result<u64> {
    Ok(sqlx::query(
        "DELETE FROM phone_call_events WHERE created_at < UTC_TIMESTAMP() - INTERVAL ? DAY
          ORDER BY id LIMIT ?",
    )
    .bind(EVENT_DAYS)
    .bind(EVENTS_PER_RUN)
    .execute(pool)
    .await?
    .rows_affected())
}

/// Step 4: calls Telnyx never reported the end of, 6 hours on: ended now,
/// without a talk time (answered ones `answered`; else incoming ones
/// `missed`, outgoing ones `failed`; a missed, voicemail ... status stays).
async fn close_open_calls(pool: &MySqlPool) -> sqlx::Result<u64> {
    // Every status listed, so the (direction, status, ended_at) index finds
    // the open calls.
    Ok(sqlx::query(
        "UPDATE phone_calls
            SET status = CASE WHEN status NOT IN ('ringing', 'in_progress') THEN status
                              WHEN answered_at IS NOT NULL THEN 'answered'
                              WHEN direction = 'inbound' THEN 'missed'
                              ELSE 'failed' END,
                ended_at = UTC_TIMESTAMP(3)
          WHERE direction IN ('inbound', 'outbound')
            AND status IN ('ringing', 'in_progress', 'answered', 'missed', 'declined', 'voicemail',
                           'ai_message', 'busy', 'no_answer', 'failed', 'canceled')
            AND ended_at IS NULL AND started_at < UTC_TIMESTAMP(3) - INTERVAL ? HOUR
          ORDER BY id LIMIT ?",
    )
    .bind(OPEN_CALL_HOURS)
    .bind(CLOSES_PER_RUN)
    .execute(pool)
    .await?
    .rows_affected())
}

/// Step 5: the customer phone index (calls find their customers by it):
/// the customers changed or added since the last run, then the next
/// batches of the companies' first fills. A failure is logged and left to
/// the next run (the index only lags meanwhile): (customers rewritten,
/// customers read by the fills).
async fn phone_index(pool: &MySqlPool, deadline: Instant) -> (usize, usize) {
    let refreshed = customer_index::refresh(pool, deadline)
        .await
        .unwrap_or_else(|err| {
            tracing::error!(error = %err, "phone index not refreshed; the next run tries again");
            0
        });
    let filled = customer_index::fill(pool, deadline)
        .await
        .unwrap_or_else(|err| {
            tracing::error!(error = %err, "phone index not filled; the next run goes on");
            0
        });
    (refreshed, filled)
}

/// Step 8: Telnyx's copies of recordings we copied whose delete failed (or
/// never ran), the ones never tried first, backing off like credentials.
async fn delete_telnyx_recordings(state: &AppState, deadline: Instant) -> sqlx::Result<usize> {
    if !state.telnyx.is_enabled() || Instant::now() >= deadline {
        return Ok(0);
    }
    let backoff = BACKOFF_MINUTES.replace("{attempts}", "telnyx_delete_attempts");
    let leftovers: Vec<(i64, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT CAST(id AS SIGNED), recording_telnyx_id FROM phone_calls
          WHERE recording_status = 'copied' AND telnyx_deleted_at IS NULL
            AND recording_telnyx_id IS NOT NULL
            AND (telnyx_delete_tried_at IS NULL
                 OR telnyx_delete_tried_at < UTC_TIMESTAMP() - INTERVAL {backoff} MINUTE)
          ORDER BY telnyx_delete_tried_at IS NOT NULL, telnyx_delete_tried_at, id
          LIMIT ?"
    )))
    .bind(DELETES_PER_RUN)
    .fetch_all(&state.pool)
    .await?;
    let mut deleted = 0;
    for (call_id, recording_id) in leftovers {
        if Instant::now() >= deadline {
            break;
        }
        if recordings::delete_telnyx_copy(state, call_id, &recording_id).await? {
            deleted += 1;
        }
    }
    Ok(deleted)
}

/// A recording whose copy is to be retried.
#[derive(FromRow)]
struct PendingRecording {
    id: i64,
    company_id: i32,
    recording_telnyx_id: String,
    voicemail: bool,
}

/// Step 9, last (a copy downloads up to 100 MB): recording copies that
/// failed or never finished, one at a time while the run has time: (copied,
/// given up).
async fn retry_recordings(state: &AppState, deadline: Instant) -> sqlx::Result<(usize, usize)> {
    let pool = &state.pool;
    // Out of attempts (and none running).
    let mut failed = usize::try_from(
        sqlx::query(
            "UPDATE phone_calls SET recording_status = 'failed'
              WHERE recording_status = 'pending' AND recording_attempts >= ?
                AND updated_at < UTC_TIMESTAMP() - INTERVAL ? MINUTE",
        )
        .bind(MAX_RECORDING_ATTEMPTS)
        .bind(STUCK_COPY_MINUTES)
        .execute(pool)
        .await?
        .rows_affected(),
    )
    .unwrap_or(usize::MAX);
    if !state.telnyx.is_enabled() || state.phone.recordings_bucket.is_none() {
        return Ok((0, failed));
    }
    let pending: Vec<PendingRecording> = sqlx::query_as(
        "SELECT CAST(id AS SIGNED) AS id, company_id, recording_telnyx_id,
                status = 'voicemail' AS voicemail
           FROM phone_calls
          WHERE recording_status = 'pending' AND recording_telnyx_id IS NOT NULL
            AND recording_attempts < ? AND updated_at < UTC_TIMESTAMP() - INTERVAL ? MINUTE
          ORDER BY id LIMIT ?",
    )
    .bind(MAX_RECORDING_ATTEMPTS)
    .bind(STUCK_COPY_MINUTES)
    .bind(RECORDINGS_PER_RUN)
    .fetch_all(pool)
    .await?;
    let mut copied = 0;
    for recording in pending {
        if Instant::now() >= deadline {
            break;
        }
        let kind = if recording.voicemail {
            RecordingKind::Voicemail
        } else {
            RecordingKind::Call
        };
        let outcome = recordings::copy(
            state,
            recording.company_id,
            recording.id,
            &recording.recording_telnyx_id,
            kind,
            None,
        )
        .await?;
        // The attempt was counted when the copy started: the last one that
        // failed, or a recording of another call, gives the copy up.
        let give_up = match outcome {
            CopyOutcome::Copied => {
                copied += 1;
                false
            }
            CopyOutcome::NotTheCalls | CopyOutcome::Failed(_) => true,
            CopyOutcome::AlreadyCopied => false,
        };
        if give_up {
            let marked = sqlx::query(
                "UPDATE phone_calls SET recording_status = 'failed'
                  WHERE id = ? AND recording_status = 'pending' AND recording_telnyx_id = ?
                    AND (? OR recording_attempts >= ?)",
            )
            .bind(recording.id)
            .bind(&recording.recording_telnyx_id)
            .bind(outcome == CopyOutcome::NotTheCalls)
            .bind(MAX_RECORDING_ATTEMPTS)
            .execute(pool)
            .await?
            .rows_affected();
            if marked > 0 {
                tracing::warn!(call_id = recording.id, "recording given up");
                failed += 1;
            }
        }
    }
    Ok((copied, failed))
}

/// A call waiting for its AI note.
#[derive(Debug, Clone, FromRow)]
struct NoteCall {
    id: i64,
    company_id: i32,
    user_id: Option<i32>,
    #[sqlx(try_from = "String")]
    direction: CallDirection,
    #[sqlx(try_from = "String")]
    status: CallStatus,
    recording_key: Option<String>,
    /// Kept from an earlier attempt that got this far.
    transcript: Option<String>,
    /// Attempts before this one.
    attempts: i64,
    /// The other party's number.
    other_number: String,
    customer_id: Option<i32>,
    deal_id: Option<i64>,
    /// Talk time in seconds.
    duration_secs: Option<i32>,
    /// Its user had AI notes on: a note is written (voicemails always get
    /// one); else the transcript is only for the call points and call check.
    ai_notes: bool,
}

/// SQL condition: a call waiting for its AI note that no run holds (binds:
/// the most attempts, the claim's minutes).
const NOTE_DUE: &str = "transcript_status IN ('pending', 'processing') AND transcribe_attempts < ?
     AND (recording_key IS NOT NULL OR transcript IS NOT NULL)
     AND (transcribe_claimed_at IS NULL
          OR transcribe_claimed_at < UTC_TIMESTAMP() - INTERVAL ? MINUTE)";

/// The AI notes of a run.
#[derive(Debug, Default)]
struct NotesRun {
    written: usize,
    skipped: usize,
    failed: usize,
}

/// How one call's AI note went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NoteOutcome {
    Written,
    /// Nothing to note.
    Skipped,
    /// Given up after the last attempt.
    Failed,
    /// Left for a later run (a claim lost, or a failure with attempts left).
    Later,
    /// AssemblyAI or OpenAI is not set up here: the claim is given back.
    NotConfigured,
}

/// Step 6: up to [`NOTES_PER_RUN`] AI notes, all at once.
async fn write_notes(state: &AppState) -> sqlx::Result<NotesRun> {
    let pool = &state.pool;
    let mut run = NotesRun {
        failed: usize::try_from(
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "UPDATE phone_calls SET transcript_status = 'failed', transcribe_claimed_at = NULL
                  WHERE transcript_status IN ('pending', 'processing') AND transcribe_attempts >= ?
                    AND (transcribe_claimed_at IS NULL
                         OR transcribe_claimed_at < UTC_TIMESTAMP() - INTERVAL {CLAIM_TTL_MINUTES} MINUTE)"
            )))
            .bind(MAX_NOTE_ATTEMPTS)
            .execute(pool)
            .await?
            .rows_affected(),
        )
        .unwrap_or(usize::MAX),
        ..NotesRun::default()
    };
    if state.phone.recordings_bucket.is_none() {
        return Ok(run);
    }
    let due: Vec<NoteCall> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT CAST(id AS SIGNED) AS id, company_id, user_id, CAST(direction AS CHAR) AS direction,
                CAST(status AS CHAR) AS status, recording_key, transcript,
                CAST(transcribe_attempts AS SIGNED) AS attempts, other_number, customer_id,
                CAST(deal_id AS SIGNED) AS deal_id, duration_secs, ai_notes <> 0 AS ai_notes
           FROM phone_calls
          WHERE {NOTE_DUE}
          ORDER BY id LIMIT ?"
    )))
    .bind(MAX_NOTE_ATTEMPTS)
    .bind(CLAIM_TTL_MINUTES)
    .bind(NOTES_PER_RUN)
    .fetch_all(pool)
    .await?;
    let tasks: Vec<_> = due
        .into_iter()
        .map(|call| {
            let state = state.clone();
            tokio::spawn(async move { note_one(&state, &call).await })
        })
        .collect();
    for task in tasks {
        match task.await {
            Ok(Ok(NoteOutcome::Written)) => run.written += 1,
            Ok(Ok(NoteOutcome::Skipped)) => run.skipped += 1,
            Ok(Ok(NoteOutcome::Failed)) => run.failed += 1,
            Ok(Ok(NoteOutcome::Later | NoteOutcome::NotConfigured)) => {}
            Ok(Err(err)) => return Err(err),
            Err(err) => tracing::error!(error = %err, "AI note task panicked"),
        }
    }
    Ok(run)
}

/// Claims, transcribes and notes one call.
async fn note_one(state: &AppState, call: &NoteCall) -> sqlx::Result<NoteOutcome> {
    let pool = &state.pool;
    let claimed = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE phone_calls
            SET transcript_status = 'processing', transcribe_claimed_at = UTC_TIMESTAMP(),
                transcribe_attempts = transcribe_attempts + 1
          WHERE id = ? AND {NOTE_DUE}"
    )))
    .bind(call.id)
    .bind(MAX_NOTE_ATTEMPTS)
    .bind(CLAIM_TTL_MINUTES)
    .execute(pool)
    .await?
    .rows_affected();
    if claimed == 0 {
        return Ok(NoteOutcome::Later);
    }
    // With AI notes off only the call points and the call check want the
    // transcript: of an answered outgoing call over 30 seconds (shorter ones
    // are attempts).
    let note_wanted = call.ai_notes || call.status == CallStatus::Voicemail;
    let scored = call.direction == CallDirection::Outbound
        && call.status == CallStatus::Answered
        && call.duration_secs.unwrap_or(0) > ATTEMPT_MAX_TALK_SECONDS;
    if !note_wanted && !scored {
        return finish(state, call, TranscriptStatus::None, None).await;
    }
    let caller = Caller {
        feature: "phone_call_note",
        user_id: call.user_id,
        company_id: Some(call.company_id),
    };
    let text = match call.transcript.as_deref().map(str::trim) {
        Some(text) if !text.is_empty() => text.to_string(),
        _ => match transcribe(state, call, caller).await? {
            Ok(text) => text,
            Err(outcome) => return Ok(outcome),
        },
    };
    conversation_check(state, call).await;
    if !note_wanted {
        // Kept on the call for its call points; no note.
        return finish(state, call, TranscriptStatus::None, None).await;
    }
    let outbound = call.direction == CallDirection::Outbound;
    if outbound && voicemail::greeting_only(&text) {
        // The rep reached a mailbox and left no message: nothing to note.
        return finish(state, call, TranscriptStatus::Skipped, None).await;
    }
    let is_voicemail = call.status == CallStatus::Voicemail
        || (outbound && voicemail::sounds_like_voicemail(&text));
    // A voicemail left for us has its own prompt; the CRM's is for the
    // rep's voicemails.
    let prompt = if !is_voicemail {
        CALL_PROMPT
    } else if call.direction == CallDirection::Inbound {
        INBOUND_VOICEMAIL_PROMPT
    } else {
        VOICEMAIL_PROMPT
    };
    let chat = Chat::new(MINI_MODEL)
        .message("system", prompt)
        .message("user", &text)
        .max_tokens(is_voicemail.then_some(180));
    let note = match state.ai.chat(pool, caller, &chat).await {
        // Tidied as the CRM tidies a call note before it saves one.
        Ok(note) => call_notes::sanitize(note.trim(), is_voicemail, Some(&text))
            .trim()
            .chars()
            .take(MAX_NOTE_CHARS)
            .collect::<String>(),
        Err(AiError::NotConfigured) => return give_back(state, call).await,
        Err(err) => {
            tracing::warn!(call_id = call.id, error = %err, "AI note not written; retried later");
            return failed_attempt(state, call).await;
        }
    };
    if note.is_empty() {
        return finish(state, call, TranscriptStatus::Skipped, None).await;
    }
    finish(state, call, TranscriptStatus::Done, Some(&note)).await
}

/// The CRM's call check (`POST /v1/followups/call-check`, run in-process) on
/// a rep's answered call of 31 to 89 seconds with a customer, now its
/// transcript is kept: a real conversation (turn-taking) stops the
/// customer's flows and moves the call's deal out of its first stage, as
/// after a `CloudTalk` call. Longer calls counted when they ended. Best
/// effort: a failure is logged.
async fn conversation_check(state: &AppState, call: &NoteCall) {
    let talk = call.duration_secs.unwrap_or(0);
    if call.direction != CallDirection::Outbound
        || call.status != CallStatus::Answered
        || call.customer_id.is_none()
        || !(31..CONVERSATION_SECS).contains(&talk)
    {
        return;
    }
    let token = crate::routes::calls::telnyx::token(call.id);
    let check = CallCheck {
        company_id: call.company_id,
        phone_digits: PhoneDigits::Text(call.other_number.clone()),
        call_id: CallId::Text(token.clone()),
        talking_time: i64::from(talk),
        is_voicemail: false,
        recording_link: Some(token),
    };
    match check_call(state, &check).await {
        Ok(result) if result.customer_responded => {
            let Some(deal_id) = call.deal_id else {
                return;
            };
            if let Err(err) = move_out_of_first_stage(&state.pool, call.company_id, deal_id).await {
                tracing::warn!(call_id = call.id, deal_id, error = %err, "deal not moved after a call");
            }
        }
        Ok(_) => {}
        Err(err) => tracing::warn!(call_id = call.id, error = %err, "call check failed"),
    }
}

/// The call's recording as text, saved on the call; else what became of
/// the attempt.
async fn transcribe(
    state: &AppState,
    call: &NoteCall,
    caller: Caller<'_>,
) -> sqlx::Result<Result<String, NoteOutcome>> {
    let Some(bucket) = state.phone.recordings_bucket.as_deref() else {
        return Ok(Err(give_back(state, call).await?));
    };
    let Some(key) = call.recording_key.as_deref() else {
        return Ok(Err(
            finish(state, call, TranscriptStatus::Failed, None).await?
        ));
    };
    let audio = match state.storage.get_url(&format!("s3://{bucket}/{key}")).await {
        Ok(Some(object)) if !object.bytes.is_empty() => object.bytes,
        Ok(_) => {
            tracing::warn!(
                call_id = call.id,
                "recording missing from the bucket; AI note given up"
            );
            return Ok(Err(
                finish(state, call, TranscriptStatus::Failed, None).await?
            ));
        }
        Err(err) => {
            tracing::warn!(call_id = call.id, error = %err, "recording not read; retried later");
            return Ok(Err(failed_attempt(state, call).await?));
        }
    };
    let transcript = match state
        .transcriber
        .transcribe(&state.pool, caller, audio, true)
        .await
    {
        Ok(transcript) => transcript,
        Err(TranscribeError::NotConfigured) => return Ok(Err(give_back(state, call).await?)),
        Err(err) => {
            tracing::warn!(call_id = call.id, error = %err, "call not transcribed; retried later");
            return Ok(Err(failed_attempt(state, call).await?));
        }
    };
    let text = transcript.labeled_text();
    if text.is_empty() {
        return Ok(Err(
            finish(state, call, TranscriptStatus::Skipped, None).await?
        ));
    }
    sqlx::query("UPDATE phone_calls SET transcript = ? WHERE id = ?")
        .bind(&text)
        .bind(call.id)
        .execute(&state.pool)
        .await?;
    Ok(Ok(text))
}

/// Gives the claim back without counting the attempt (a service is not set
/// up here).
async fn give_back(state: &AppState, call: &NoteCall) -> sqlx::Result<NoteOutcome> {
    sqlx::query(
        "UPDATE phone_calls
            SET transcript_status = 'pending', transcribe_claimed_at = NULL,
                transcribe_attempts = GREATEST(transcribe_attempts, 1) - 1
          WHERE id = ? AND transcript_status = 'processing'",
    )
    .bind(call.id)
    .execute(&state.pool)
    .await?;
    Ok(NoteOutcome::NotConfigured)
}

/// An attempt that failed: the call waits for a later run (its claim time
/// kept, so not before the claim would have lapsed), or is given up after
/// the last attempt.
async fn failed_attempt(state: &AppState, call: &NoteCall) -> sqlx::Result<NoteOutcome> {
    if call.attempts + 1 >= MAX_NOTE_ATTEMPTS {
        return finish(state, call, TranscriptStatus::Failed, None).await;
    }
    sqlx::query(
        "UPDATE phone_calls SET transcript_status = 'pending'
          WHERE id = ? AND transcript_status = 'processing'",
    )
    .bind(call.id)
    .execute(&state.pool)
    .await?;
    Ok(NoteOutcome::Later)
}

/// The call's deal for its note: the call's own when it is still live in
/// the company, else the customer's oldest open deal.
async fn note_deal(pool: &MySqlPool, call_id: i64) -> sqlx::Result<Option<i64>> {
    let deals: Option<(Option<i64>, Option<i64>)> = sqlx::query_as(
        "SELECT (SELECT CAST(d.id AS SIGNED) FROM deals d JOIN customers dc ON dc.id = d.customer_id
                  WHERE d.id = pc.deal_id AND d.deleted_at IS NULL AND dc.company_id = pc.company_id),
                (SELECT CAST(MIN(d.id) AS SIGNED) FROM deals d JOIN customers c ON c.id = d.customer_id
                  WHERE d.customer_id = pc.customer_id AND c.company_id = pc.company_id
                    AND c.deleted_at IS NULL AND d.deleted_at IS NULL AND d.is_won IS NULL)
           FROM phone_calls pc WHERE pc.id = ?",
    )
    .bind(call_id)
    .fetch_optional(pool)
    .await?;
    Ok(deals.and_then(|(call_deal, open_deal)| call_deal.or(open_deal)))
}

/// Ends the call's AI note as `status`: with `note`, kept on the call and
/// added to its deal (one transaction), whose people are then notified.
async fn finish(
    state: &AppState,
    call: &NoteCall,
    status: TranscriptStatus,
    note: Option<&str>,
) -> sqlx::Result<NoteOutcome> {
    let pool = &state.pool;
    let deal_id = match note {
        Some(_) => note_deal(pool, call.id).await?,
        None => None,
    };
    let mut tx = pool.begin().await?;
    let note_id = match (note, deal_id) {
        (Some(note), Some(deal_id)) => Some(
            sqlx::query(
                "INSERT INTO deal_notes (deal_id, company_id, content, created_by) VALUES (?, ?, ?, ?)",
            )
            .bind(deal_id)
            .bind(call.company_id)
            .bind(note)
            .bind(AI_NOTE_AUTHOR)
            .execute(&mut *tx)
            .await?
            .last_insert_id(),
        ),
        _ => None,
    };
    let saved = sqlx::query(
        "UPDATE phone_calls
            SET transcript_status = ?, ai_note = ?, note_id = ?, transcribe_claimed_at = NULL
          WHERE id = ? AND transcript_status = 'processing'",
    )
    .bind(status.as_str())
    .bind(note)
    .bind(note_id)
    .bind(call.id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if saved == 0 {
        // Another run took the call over meanwhile: its note wins.
        tx.rollback().await?;
        return Ok(NoteOutcome::Later);
    }
    tx.commit().await?;
    if let (Some(note), Some(deal_id), Some(_)) = (note, deal_id, note_id) {
        // No user wrote it, so nobody is left out.
        notify_assignees(
            state,
            deal_id,
            0,
            AI_NOTE_AUTHOR,
            note,
            Notification::NoteAdded,
        )
        .await;
    }
    Ok(match status {
        TranscriptStatus::Done => NoteOutcome::Written,
        TranscriptStatus::Failed => NoteOutcome::Failed,
        _ => NoteOutcome::Skipped,
    })
}

/// An answered outbound call for the call points.
#[derive(FromRow)]
struct PointsCall {
    id: i64,
    company_id: i32,
    user_id: i32,
    other_last10: String,
    started_at: DateTime<Utc>,
    duration_secs: Option<i32>,
    recording_status: String,
    #[sqlx(try_from = "String")]
    transcript_status: TranscriptStatus,
    transcript: Option<String>,
}

/// How an answered outbound call scores: an `attempt` (30 seconds of talk
/// or less, or nothing shows more), `voicemail` or `conversation` (by the
/// transcript), or `pending` while its recording or transcript is coming.
fn points_outcome(call: &PointsCall) -> &'static str {
    if call.duration_secs.unwrap_or(0) <= ATTEMPT_MAX_TALK_SECONDS {
        return "attempt";
    }
    if let Some(text) = call
        .transcript
        .as_deref()
        .filter(|text| !text.trim().is_empty())
    {
        return voicemail::classify(false, text);
    }
    let coming = call.recording_status == "pending"
        || matches!(
            call.transcript_status,
            TranscriptStatus::Pending | TranscriptStatus::Processing
        );
    if coming { "pending" } else { "attempt" }
}

/// The columns of a [`PointsCall`] (`pc` the call).
const POINTS_COLUMNS: &str = "CAST(pc.id AS SIGNED) AS id, pc.company_id, pc.user_id,
        pc.other_last10, pc.started_at, pc.duration_secs,
        CAST(pc.recording_status AS CHAR) AS recording_status,
        CAST(pc.transcript_status AS CHAR) AS transcript_status, pc.transcript";

/// Step 7: answered outbound calls into the call points (provider
/// `telnyx`): the ones that ended in the last [`POINTS_DAYS`] days without
/// points yet (emergency calls, with no number, left out), and the ones
/// still `pending`; a classified call is never classified again.
async fn call_points(state: &AppState) -> sqlx::Result<usize> {
    let pool = &state.pool;
    let mut calls: Vec<PointsCall> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {POINTS_COLUMNS}
           FROM phone_calls pc
           JOIN activity_points_sources src ON src.source = 'calls'
           LEFT JOIN phone_call_points pcp
             ON pcp.company_id = pc.company_id AND pcp.provider = 'telnyx'
            AND pcp.provider_call_id = CAST(pc.id AS CHAR)
          WHERE pc.direction = 'outbound' AND pc.status = 'answered'
            AND pc.ended_at >= UTC_TIMESTAMP(3) - INTERVAL {POINTS_DAYS} DAY
            AND pc.user_id IS NOT NULL AND pc.other_last10 <> '' AND {NOT_TEST_CALL}
            AND pc.started_at >= src.counts_from AND pcp.id IS NULL
          ORDER BY pc.id LIMIT ?"
    )))
    .bind(POINTS_PER_RUN)
    .fetch_all(pool)
    .await?;
    let pending: Vec<PointsCall> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {POINTS_COLUMNS}
           FROM phone_call_points pcp
           JOIN phone_calls pc
             ON pc.id = CAST(pcp.provider_call_id AS UNSIGNED) AND pc.company_id = pcp.company_id
          WHERE pcp.outcome = 'pending' AND pcp.provider = 'telnyx' AND pc.user_id IS NOT NULL
          ORDER BY pcp.id LIMIT ?"
    )))
    .bind(POINTS_PER_RUN)
    .fetch_all(pool)
    .await?;
    calls.extend(pending);
    let written = calls.len();
    for call in calls {
        let outcome = points_outcome(&call);
        let digits = Some(call.other_last10.as_str()).filter(|digits| digits.len() == 10);
        sqlx::query(
            "INSERT INTO phone_call_points
               (company_id, provider, provider_call_id, user_id, customer_digits, started_at,
                talk_seconds, outcome, provider_voicemail, classified_at)
             VALUES (?, 'telnyx', ?, ?, ?, ?, ?, ?, 0, IF(? = 'pending', NULL, UTC_TIMESTAMP())) AS new
             ON DUPLICATE KEY UPDATE
               user_id = new.user_id, customer_digits = new.customer_digits,
               started_at = new.started_at, talk_seconds = new.talk_seconds,
               classified_at = IF(phone_call_points.outcome = 'pending', new.classified_at, phone_call_points.classified_at),
               outcome = IF(phone_call_points.outcome = 'pending', new.outcome, phone_call_points.outcome)",
        )
        .bind(call.company_id)
        .bind(call.id.to_string())
        .bind(call.user_id)
        .bind(digits)
        .bind(call.started_at)
        .bind(call.duration_secs.unwrap_or(0))
        .bind(outcome)
        .bind(outcome)
        .execute(pool)
        .await?;
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcribe::{Transcript, Utterance};

    fn points(
        talk: Option<i32>,
        recording: &str,
        status: TranscriptStatus,
        transcript: Option<&str>,
    ) -> &'static str {
        points_outcome(&PointsCall {
            id: 1,
            company_id: 100,
            user_id: 100,
            other_last10: "3175550142".into(),
            started_at: Utc::now(),
            duration_secs: talk,
            recording_status: recording.into(),
            transcript_status: status,
            transcript: transcript.map(str::to_string),
        })
    }

    #[test]
    fn answered_outbound_calls_score_like_the_crm() {
        use TranscriptStatus::{Done, Pending, Processing};
        let none = TranscriptStatus::None;
        assert_eq!(
            points(Some(30), "copied", Done, Some("Hello? Yes.")),
            "attempt"
        );
        assert_eq!(points(None, "none", none, None), "attempt");
        assert_eq!(
            points(
                Some(95),
                "copied",
                Done,
                Some("Hello? Yes, the white quartz.")
            ),
            "conversation"
        );
        assert_eq!(
            points(
                Some(45),
                "copied",
                Done,
                Some(
                    "Please leave a message after the tone. Hi, this is Rita with Acme about your quote."
                )
            ),
            "voicemail"
        );
        assert_eq!(points(Some(95), "pending", none, None), "pending");
        assert_eq!(points(Some(95), "copied", Pending, None), "pending");
        assert_eq!(points(Some(95), "copied", Processing, None), "pending");
        // Never recorded or transcribed: nothing shows more than an attempt.
        assert_eq!(points(Some(95), "none", none, None), "attempt");
        assert_eq!(
            points(Some(95), "failed", TranscriptStatus::Failed, None),
            "attempt"
        );
    }

    #[test]
    fn notes_are_written_from_the_speakers_turns() {
        let turn = |speaker: &str, text: &str| Utterance {
            speaker: speaker.into(),
            text: text.into(),
        };
        let labeled = Transcript {
            text: "Hi. Hello.".into(),
            utterances: Some(vec![turn("A", " Hi. "), turn("B", "Hello.")]),
            audio_seconds: Some(4.0),
        };
        assert_eq!(labeled.labeled_text(), "Speaker A: Hi.\nSpeaker B: Hello.");
        // Kept that way, it reads back with its turns.
        let kept = Transcript::from_labeled_text(&labeled.labeled_text());
        assert_eq!(kept.labeled_text(), "Speaker A: Hi.\nSpeaker B: Hello.");
        assert_eq!(kept.utterances.map(|turns| turns.len()), Some(2));
        assert!(
            Transcript::from_labeled_text("Hi there.\nSpeaker B: Hello.")
                .utterances
                .is_none()
        );
        let plain = Transcript {
            text: " Hi there. ".into(),
            utterances: Some(Vec::new()),
            audio_seconds: None,
        };
        assert_eq!(plain.labeled_text(), "Hi there.");
    }
}
