//! `POST /v1/jobs/signature-reminders`: natively signed documents' lifecycle.
//!
//! Open requests past their expiry are expired (unless a signature was
//! already accepted), then reminders (days 3, 6 and 10 after sending) and
//! the sender's notifications (viewed, completed, declined) go out. Each send
//! is a row in `native_signature_jobs`, claimed with a lease so two runs
//! never send it twice; a send whose outcome is unknown is never retried.
//! Port of the CRM's `nativeSignatureReminders.server.ts`.

use super::JobCaller;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::native_signing::{
    CompletedFile, FrozenSettings, LifecycleEvent, NativeSigning, Recipient, reminder_schedule,
    sha256_hex,
};
use crate::state::AppState;
use crate::webhooks_client::{OutgoingAttachment, OutgoingEmail, SendFailure};
use axum::Json;
use axum::extract::State;
use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use lambda_http::tracing;
use serde::Serialize;
use sqlx::{MySql, MySqlPool, Transaction};
use utoipa::ToSchema;

/// Most sends per run.
const MAX_SENDS: usize = 100;
/// Attempts at a send the mail service refused.
const MAX_ATTEMPTS: i64 = 3;

/// What one run did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
pub struct SignatureRun {
    /// Reminders and notifications sent (what the scheduler logs).
    pub processed: usize,
    /// Reminders and notifications sent.
    pub sent: usize,
    /// Sends that failed or were blocked.
    pub failed: usize,
    /// Open requests expired.
    pub expired: usize,
}

const fn naive(at: DateTime<Utc>) -> NaiveDateTime {
    at.naive_utc()
}

#[derive(sqlx::FromRow, Debug)]
struct DocumentRow {
    request_id: i64,
    company_id: i64,
    document_id: String,
    status: String,
    filename: String,
    signer_email: String,
    settings_json: serde_json::Value,
    sent_at: Option<NaiveDateTime>,
    expires_at: Option<NaiveDateTime>,
    revoked_at: Option<NaiveDateTime>,
    completed_object_key: Option<String>,
    completed_sha256: Option<String>,
}

const DOCUMENT_COLUMNS: &str = "CAST(nd.request_id AS SIGNED) AS request_id, CAST(nd.company_id AS SIGNED) AS company_id,
    nd.document_id, CAST(sr.status AS CHAR) AS status, sr.filename, sr.signer_email, nd.settings_json,
    nd.sent_at, nd.expires_at, nd.revoked_at, nd.completed_object_key, nd.completed_sha256";

const fn utc(at: NaiveDateTime) -> DateTime<Utc> {
    at.and_utc()
}

/// Queues the reminders and notifications that are due.
async fn seed(pool: &MySqlPool, now: DateTime<Utc>) -> Result<(), sqlx::Error> {
    let mut cursor = 0i64;
    loop {
        let rows: Vec<DocumentRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {DOCUMENT_COLUMNS} FROM native_signature_documents nd
              INNER JOIN signature_requests sr ON sr.id = nd.request_id AND sr.company_id = nd.company_id
              WHERE nd.request_id > ? AND sr.provider = 'native' AND nd.sent_at IS NOT NULL AND nd.revoked_at IS NULL
                AND ((sr.status IN ('sent', 'viewed') AND JSON_EXTRACT(nd.settings_json, '$.reminders') = CAST('true' AS JSON))
                  OR JSON_EXTRACT(nd.settings_json, '$.notifications.viewed') = CAST('true' AS JSON)
                  OR JSON_EXTRACT(nd.settings_json, '$.notifications.completed') = CAST('true' AS JSON)
                  OR JSON_EXTRACT(nd.settings_json, '$.notifications.declined') = CAST('true' AS JSON))
                AND nd.sent_at <= ?
              ORDER BY nd.request_id LIMIT 500"
        )))
        .bind(cursor)
        .bind(naive(now))
        .fetch_all(pool)
        .await?;
        let Some(last) = rows.last() else {
            return Ok(());
        };
        cursor = last.request_id;
        for row in rows {
            let Some(settings) = FrozenSettings::parse(&row.settings_json) else {
                tracing::warn!(
                    request_id = row.request_id,
                    "native signature settings are invalid"
                );
                continue;
            };
            let expired = row.expires_at.is_some_and(|at| utc(at) <= now);
            let due: Vec<(i64, DateTime<Utc>)> = row
                .sent_at
                .map(|sent| reminder_schedule(utc(sent), &settings, &row.status))
                .unwrap_or_default()
                .into_iter()
                .filter(|(_, at)| *at <= now && !expired)
                .collect();
            // After an outage only the latest overdue reminder goes out.
            for (index, (day, at)) in due.iter().enumerate() {
                sqlx::query(
                    "INSERT IGNORE INTO native_signature_jobs (request_id, company_id, job_key, kind, due_at, state)
                     VALUES (?, ?, ?, 'reminder', ?, ?)",
                )
                .bind(row.request_id)
                .bind(row.company_id)
                .bind(format!("day:{day}"))
                .bind(naive(*at))
                .bind(if index + 1 == due.len() { "pending" } else { "cancelled" })
                .execute(pool)
                .await?;
            }
            for event in LifecycleEvent::ALL {
                if !settings.notifies(event) {
                    continue;
                }
                let happened: Option<NaiveDateTime> = sqlx::query_scalar(
                    "SELECT created_at FROM native_signature_audit
                      WHERE request_id = ? AND company_id = ? AND event_type = ? LIMIT 1",
                )
                .bind(row.request_id)
                .bind(row.company_id)
                .bind(event.as_str())
                .fetch_optional(pool)
                .await?;
                if let Some(at) = happened {
                    sqlx::query(
                        "INSERT IGNORE INTO native_signature_jobs (request_id, company_id, job_key, kind, due_at, state)
                         VALUES (?, ?, ?, 'notification', ?, 'pending')",
                    )
                    .bind(row.request_id)
                    .bind(row.company_id)
                    .bind(event.as_str())
                    .bind(at)
                    .execute(pool)
                    .await?;
                }
            }
        }
    }
}

struct Claimed {
    id: i64,
    request_id: i64,
    company_id: i64,
    owner: String,
}

/// Takes the next due send, with a 10-minute lease.
async fn claim(pool: &MySqlPool, now: DateTime<Utc>) -> Result<Option<Claimed>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let job: Option<(i64, i64, i64)> = sqlx::query_as(
        "SELECT id, CAST(request_id AS SIGNED), CAST(company_id AS SIGNED) FROM native_signature_jobs
          WHERE state IN ('pending', 'retry') AND due_at <= ? AND attempts < ?
          ORDER BY due_at, id LIMIT 1 FOR UPDATE SKIP LOCKED",
    )
    .bind(naive(now))
    .bind(MAX_ATTEMPTS)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((id, request_id, company_id)) = job else {
        tx.commit().await?;
        return Ok(None);
    };
    let owner = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "UPDATE native_signature_jobs SET state = 'sending', attempts = attempts + 1, lease_owner = ?, lease_until = ?
          WHERE id = ?",
    )
    .bind(&owner)
    .bind(naive(now + Duration::minutes(10)))
    .bind(id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Some(Claimed {
        id,
        request_id,
        company_id,
        owner,
    }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Sent,
    Failed,
    Cancelled,
}

async fn set_state(
    tx: &mut Transaction<'_, MySql>,
    id: i64,
    state: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE native_signature_jobs SET state = ?, lease_owner = NULL, lease_until = NULL WHERE id = ?")
        .bind(state)
        .bind(id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// `Name <address>` or a bare address.
fn sender(from: &str) -> (Option<String>, String) {
    match (from.rfind('<'), from.rfind('>')) {
        (Some(open), Some(close)) if open < close => {
            let name = from[..open].trim().trim_matches('"').trim();
            (
                Some(name.to_string()).filter(|n| !n.is_empty()),
                from[open + 1..close].trim().to_string(),
            )
        }
        _ => (None, from.trim().to_string()),
    }
}

/// The message a job sends; `None` when it cannot be built (the job is
/// blocked for a person to look at).
async fn build_message(
    state: &AppState,
    signing: &NativeSigning,
    kind: &str,
    job_key: &str,
    row: &DocumentRow,
    settings: &FrozenSettings,
) -> Option<OutgoingEmail> {
    let invitation = signing
        .lifecycle_mail(&state.pool, row.request_id, row.company_id)
        .await
        .ok()?;
    let (from_name, from_address) = sender(&invitation.snapshot.from);
    let mut message = OutgoingEmail {
        from_name,
        from_address: from_address.clone(),
        to: Vec::new(),
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: String::new(),
        body: String::new(),
        signature: None,
        reply_to: Vec::new(),
        in_reply_to: None,
        references: invitation.snapshot.references.clone(),
        attachments: Vec::new(),
        transactional: true,
    };
    if kind == "reminder" {
        message.to = vec![row.signer_email.clone()];
        message.subject = format!("Signature reminder: {}", row.filename);
        message.body = format!(
            "<p>Please review and sign {}.</p><p><a href=\"{}\">Review and sign</a></p>",
            escape_html(&row.filename),
            escape_html(&invitation.signing_url)
        );
        return Some(message);
    }
    let event = LifecycleEvent::parse(job_key)?;
    let policy = settings
        .policy(event)
        .filter(|_| settings.notifies(event))?;
    if event != LifecycleEvent::Completed && policy.completed_file != CompletedFile::None {
        return None;
    }
    let link = if policy.completed_file == CompletedFile::Link {
        invitation.signing_url.as_str()
    } else {
        ""
    };
    let fill = |text: &str| {
        text.replace("{{filename}}", &row.filename)
            .replace("{{link}}", link)
    };
    if !from_address.contains('@') {
        return None;
    }
    let mut to: Vec<String> = Vec::new();
    for recipient in &policy.recipients {
        let address = match recipient {
            Recipient::Sender => from_address.clone(),
            Recipient::Signer => row.signer_email.clone(),
        };
        if !to.contains(&address) {
            to.push(address);
        }
    }
    message.to = to;
    message.subject = fill(&policy.subject);
    message.body = escape_html(&fill(&policy.text)).replace('\n', "<br>");
    if policy.completed_file == CompletedFile::Attachment {
        let (Some(key), Some(hash)) = (
            row.completed_object_key.as_deref(),
            row.completed_sha256.as_deref(),
        ) else {
            return None;
        };
        let scoped = format!(
            "{}/signatures/{}/completed-{hash}.pdf",
            row.company_id, row.document_id
        );
        if row.status != "signed" || key != scoped {
            return None;
        }
        let bucket = state.signature_bucket.as_deref()?;
        // Only an intact signed file goes out.
        let object = state
            .storage
            .get_url(&format!("s3://{bucket}/{key}"))
            .await
            .ok()??;
        if sha256_hex(&object.bytes) != hash {
            return None;
        }
        let url = state
            .storage
            .presign_get_in(Some(bucket), key, "application/pdf", Some(&row.filename))
            .await
            .ok()?;
        message.attachments.push(OutgoingAttachment {
            filename: row.filename.clone(),
            content_type: "application/pdf".into(),
            url,
        });
    }
    Some(message)
}

/// Sends one claimed job, holding the request's row so finishing, declining
/// or expiring cannot race the send.
async fn dispatch(
    state: &AppState,
    signing: &NativeSigning,
    job: &Claimed,
    now: DateTime<Utc>,
) -> Result<Outcome, sqlx::Error> {
    let mut tx = state.pool.begin().await?;
    let row: Option<DocumentRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {DOCUMENT_COLUMNS} FROM signature_requests sr
          INNER JOIN native_signature_documents nd ON nd.request_id = sr.id AND nd.company_id = sr.company_id
          WHERE sr.id = ? AND sr.company_id = ? AND sr.provider = 'native' FOR UPDATE"
    )))
    .bind(job.request_id)
    .bind(job.company_id)
    .fetch_optional(&mut *tx)
    .await?;
    let stored: Option<(String, String, NaiveDateTime, i64)> = sqlx::query_as(
        "SELECT CAST(kind AS CHAR), job_key, due_at, attempts FROM native_signature_jobs
          WHERE id = ? AND state = 'sending' AND lease_owner = ? FOR UPDATE",
    )
    .bind(job.id)
    .bind(&job.owner)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((kind, job_key, due_at, attempts)) = stored else {
        tx.commit().await?;
        return Ok(Outcome::Cancelled);
    };
    let reminder = kind == "reminder";
    let open = |row: &DocumentRow| matches!(row.status.as_str(), "sent" | "viewed");
    let gone = row.as_ref().is_none_or(|row| {
        row.revoked_at.is_some()
            || (reminder
                && (!open(row)
                    || row.sent_at.is_none()
                    || row.expires_at.is_some_and(|at| utc(at) <= now)))
    });
    let settings = row
        .as_ref()
        .and_then(|row| FrozenSettings::parse(&row.settings_json));
    let (Some(row), Some(settings)) = (row.filter(|_| !gone), settings) else {
        let outcome = if gone { "cancelled" } else { "blocked" };
        set_state(&mut tx, job.id, outcome).await?;
        tx.commit().await?;
        return Ok(if gone {
            Outcome::Cancelled
        } else {
            Outcome::Failed
        });
    };
    if reminder && (!settings.reminders || reminder_superseded(&mut tx, job, due_at, now).await?) {
        set_state(&mut tx, job.id, "cancelled").await?;
        tx.commit().await?;
        return Ok(Outcome::Cancelled);
    }
    let Some(message) = build_message(state, signing, &kind, &job_key, &row, &settings).await
    else {
        set_state(&mut tx, job.id, "blocked").await?;
        tx.commit().await?;
        return Ok(Outcome::Failed);
    };
    let result = state.webhooks.send_email(&message).await;
    let outcome = record_send(&mut tx, job.id, attempts, result, now).await?;
    tx.commit().await?;
    Ok(outcome)
}

/// A signature was accepted, or a later reminder is already due.
async fn reminder_superseded(
    tx: &mut Transaction<'_, MySql>,
    job: &Claimed,
    due_at: NaiveDateTime,
    now: DateTime<Utc>,
) -> Result<bool, sqlx::Error> {
    let accepted: Option<i64> = sqlx::query_scalar(
        "SELECT CAST(request_id AS SIGNED) FROM native_signature_finalizations WHERE request_id = ? AND company_id = ? LIMIT 1",
    )
    .bind(job.request_id)
    .bind(job.company_id)
    .fetch_optional(&mut **tx)
    .await?;
    let later: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM native_signature_jobs
          WHERE request_id = ? AND company_id = ? AND kind = 'reminder' AND due_at > ? AND due_at <= ? LIMIT 1",
    )
    .bind(job.request_id)
    .bind(job.company_id)
    .bind(due_at)
    .bind(naive(now))
    .fetch_optional(&mut **tx)
    .await?;
    Ok(accepted.is_some() || later.is_some())
}

/// Records how a send went. A refused message may be retried; an unknown
/// outcome never is (the signer could get it twice).
async fn record_send(
    tx: &mut Transaction<'_, MySql>,
    id: i64,
    attempts: i64,
    result: Result<String, SendFailure>,
    now: DateTime<Utc>,
) -> Result<Outcome, sqlx::Error> {
    if let Some(message_id) = result.as_ref().ok().filter(|id| !id.is_empty()) {
        sqlx::query(
            "UPDATE native_signature_jobs SET state = 'sent', sent_at = ?, message_id = ?, lease_owner = NULL, lease_until = NULL
              WHERE id = ?",
        )
        .bind(naive(now))
        .bind(message_id)
        .bind(id)
        .execute(&mut **tx)
        .await?;
        return Ok(Outcome::Sent);
    }
    let next = match &result {
        Err(SendFailure::Rejected(_)) if attempts < MAX_ATTEMPTS => "retry",
        Err(SendFailure::Rejected(_)) => "failed",
        _ => "uncertain",
    };
    if let Err(err) = &result {
        tracing::warn!(?err, job = id, "native signature mail failed");
    }
    sqlx::query(
        "UPDATE native_signature_jobs SET state = ?, due_at = ?, lease_owner = NULL, lease_until = NULL WHERE id = ?",
    )
    .bind(next)
    .bind(naive(now + Duration::hours(1)))
    .bind(id)
    .execute(&mut **tx)
    .await?;
    Ok(Outcome::Failed)
}

/// Sends the due reminders and notifications.
pub async fn process_due_reminders(
    state: &AppState,
    signing: &NativeSigning,
    now: DateTime<Utc>,
) -> Result<(usize, usize), sqlx::Error> {
    sqlx::query(
        "UPDATE native_signature_jobs SET state = 'uncertain', lease_owner = NULL, lease_until = NULL
          WHERE state = 'sending' AND lease_until <= ?",
    )
    .bind(naive(now))
    .execute(&state.pool)
    .await?;
    seed(&state.pool, now).await?;
    let (mut sent, mut failed) = (0, 0);
    for _ in 0..MAX_SENDS {
        let Some(job) = claim(&state.pool, now).await? else {
            break;
        };
        match dispatch(state, signing, &job, now).await? {
            Outcome::Sent => sent += 1,
            Outcome::Failed => failed += 1,
            Outcome::Cancelled => {}
        }
    }
    Ok((sent, failed))
}

/// Expires open requests past their expiry, unless a signature was accepted.
pub async fn process_expired(pool: &MySqlPool, now: DateTime<Utc>) -> Result<usize, sqlx::Error> {
    let rows: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT CAST(nd.request_id AS SIGNED), CAST(nd.company_id AS SIGNED) FROM native_signature_documents nd
          INNER JOIN signature_requests sr ON sr.id = nd.request_id AND sr.company_id = nd.company_id
          WHERE sr.provider = 'native' AND sr.status IN ('sent', 'viewed') AND nd.sent_at IS NOT NULL AND nd.expires_at <= ?
            AND NOT EXISTS (SELECT 1 FROM native_signature_finalizations nf
                             WHERE nf.request_id = nd.request_id AND nf.company_id = nd.company_id)
          ORDER BY nd.request_id LIMIT 100",
    )
    .bind(naive(now))
    .fetch_all(pool)
    .await?;
    let mut expired = 0;
    for (request_id, company_id) in rows {
        let mut tx = pool.begin().await?;
        let current: Option<(String, Option<NaiveDateTime>)> = sqlx::query_as(
            "SELECT CAST(sr.status AS CHAR), nd.expires_at FROM signature_requests sr
              INNER JOIN native_signature_documents nd ON nd.request_id = sr.id AND nd.company_id = sr.company_id
              WHERE sr.id = ? AND sr.company_id = ? AND sr.provider = 'native' FOR UPDATE",
        )
        .bind(request_id)
        .bind(company_id)
        .fetch_optional(&mut *tx)
        .await?;
        let still_due = current.is_some_and(|(status, expires_at)| {
            matches!(status.as_str(), "draft" | "sent" | "viewed")
                && expires_at.is_some_and(|at| utc(at) <= now)
        });
        let accepted: Option<i64> = sqlx::query_scalar(
            "SELECT CAST(request_id AS SIGNED) FROM native_signature_finalizations WHERE request_id = ? AND company_id = ?",
        )
        .bind(request_id)
        .bind(company_id)
        .fetch_optional(&mut *tx)
        .await?;
        if !still_due || accepted.is_some() {
            tx.commit().await?;
            continue;
        }
        let updated = sqlx::query(
            "UPDATE signature_requests SET status = 'expired'
              WHERE id = ? AND company_id = ? AND status IN ('draft', 'sent', 'viewed') AND provider = 'native'",
        )
        .bind(request_id)
        .bind(company_id)
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() == 0 {
            tx.commit().await?;
            continue;
        }
        sqlx::query(
            "UPDATE native_signature_jobs SET state = 'cancelled'
              WHERE request_id = ? AND company_id = ? AND kind = 'reminder' AND state IN ('pending', 'retry')",
        )
        .bind(request_id)
        .bind(company_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT IGNORE INTO native_signature_audit (request_id, company_id, event_key, event_type, evidence_json, created_at)
             VALUES (?, ?, 'expired', 'expired', ?, ?)",
        )
        .bind(request_id)
        .bind(company_id)
        .bind(serde_json::json!({ "expiredAt": now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true) }).to_string())
        .bind(naive(now))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        expired += 1;
    }
    Ok(expired)
}

/// Run the native signature lifecycle
///
/// Expires open requests past their expiry, then sends due reminders and
/// the senders' notifications. Called by the `time-triggered` Lambda with
/// `Authorization: <LAMBDA_KEY>`. While native signatures are not enabled
/// it does nothing.
#[utoipa::path(
    post,
    path = "/v1/jobs/signature-reminders",
    operation_id = "run_signature_reminders",
    tag = "jobs",
    responses(
        (status = 200, description = "What the run did", body = SignatureRun),
        (status = 401, description = "Missing or wrong job key", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("job_key" = []))
)]
pub async fn run(
    State(state): State<AppState>,
    _caller: JobCaller,
) -> ApiResult<Json<SignatureRun>> {
    let Some(signing) = state.native_signing.clone() else {
        return Ok(Json(SignatureRun::default()));
    };
    let now = Utc::now();
    let expired = process_expired(&state.pool, now)
        .await
        .map_err(ApiError::from)?;
    let (sent, failed) = process_due_reminders(&state, &signing, now).await?;
    Ok(Json(SignatureRun {
        processed: sent,
        sent,
        failed,
        expired,
    }))
}
