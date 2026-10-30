//! A rep's texting line changed, from My Account or the Users page: the
//! `RingCentral` SMS webhook covers their extension, and a rep who moved
//! from `CloudTalk` to `RingCentral` keeps every text.

use crate::state::AppState;
use common::telephony::ringcentral;
use lambda_http::tracing;
use sqlx::MySqlPool;

const COPY_MARKER: &str = "cloudtalk:";
/// Where the `CloudTalk` id starts in a copy's `idempotency_key`.
const MARKER_END: i32 = 11;

/// A user's texting line, before a save or as saved. Numbers are the last
/// ten digits.
#[derive(Debug, Clone, Default)]
pub struct Line {
    pub cloudtalk_agent: Option<String>,
    pub cloudtalk_phone: Option<String>,
    pub ringcentral_extension: Option<String>,
    pub ringcentral_phone: Option<String>,
}

/// After a save turned `before` into `after`. `matched`: `RingCentral` found
/// the saved number, so the subscription is renewed with the current
/// extensions (it never expires, so a rep added since is covered only by a
/// renewal). A rep who left `CloudTalk`, or moved the same number, keeps
/// their `CloudTalk` texts. Failures are logged; the save stands.
pub async fn follow(state: &AppState, company_id: i32, before: &Line, after: &Line, matched: bool) {
    let Some(extension) = after.ringcentral_extension.as_deref() else {
        return;
    };
    let pool = &state.pool;
    if (matched || before.ringcentral_extension.as_deref() != Some(extension))
        && let Err(error) =
            ringcentral::ensure_sms_subscription(pool, &state.telephony_http, company_id).await
    {
        tracing::error!(company_id, ?error, "RingCentral SMS subscription failed");
    }
    let agent =
        filled(after.cloudtalk_agent.as_deref()).or(filled(before.cloudtalk_agent.as_deref()));
    let line =
        filled(after.cloudtalk_phone.as_deref()).or(filled(before.cloudtalk_phone.as_deref()));
    if agent.is_none() && line.is_none() {
        return;
    }
    let left_cloudtalk = filled(after.cloudtalk_agent.as_deref()).is_none()
        && filled(after.cloudtalk_phone.as_deref()).is_none();
    let same_number = line.is_some() && line == filled(after.ringcentral_phone.as_deref());
    if !left_cloudtalk && !same_number {
        return;
    }
    if let Err(error) = copy_cloudtalk_history(pool, company_id, line, agent, extension).await {
        tracing::error!(company_id, %error, "copying CloudTalk history failed");
    }
}

fn filled(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|v| !v.is_empty())
}

/// Copies the rep's `CloudTalk` texts (their agent's, and untagged ones on
/// their line), pictures and read state to the `RingCentral` tables. Each
/// copy carries `cloudtalk:<id>`, so repeat saves copy nothing twice.
async fn copy_cloudtalk_history(
    pool: &MySqlPool,
    company_id: i32,
    line: Option<&str>,
    cloudtalk_agent: Option<&str>,
    extension: &str,
) -> sqlx::Result<u64> {
    let mut tx = pool.begin().await?;
    let messages = sqlx::query(
        "INSERT INTO ringcentral_sms
           (ringcentral_id, sender, recipient, text, direction, status, error_message,
            idempotency_key, agent, sender_user_id, company_id, created_date,
            sender10, recipient10, phone_digits, is_echo, echo_of_sms_id, is_automated)
         SELECT NULL, s.sender, s.recipient, s.text, s.direction, s.status, s.error_message,
                CONCAT(?, s.id), IF(s.agent IS NULL, NULL, ?), s.sender_user_id, s.company_id,
                s.created_date, s.sender10, s.recipient10, s.phone_digits, 0, NULL,
                s.is_automated
           FROM cloudtalk_sms s
          WHERE s.company_id = ?
            AND s.is_echo = 0
            AND (s.agent = ? OR (s.agent IS NULL AND (s.recipient10 = ? OR s.sender10 = ?)))
            AND s.id NOT IN (
              SELECT CAST(SUBSTRING(r.idempotency_key, ?) AS UNSIGNED) FROM ringcentral_sms r
               WHERE r.company_id = ? AND r.idempotency_key LIKE CONCAT(?, '%')
            )",
    )
    .bind(COPY_MARKER)
    .bind(extension)
    .bind(company_id)
    .bind(cloudtalk_agent)
    .bind(line)
    .bind(line)
    .bind(MARKER_END)
    .bind(company_id)
    .bind(COPY_MARKER)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    sqlx::query(
        "INSERT INTO ringcentral_sms_attachments
           (ringcentral_sms_id, content_type, filename, s3_key, s3_url, width, height,
            position, created_date)
         SELECT r.id, a.content_type, a.filename, a.s3_key, a.s3_url, a.width, a.height,
                a.position, a.created_date
           FROM ringcentral_sms r
           JOIN cloudtalk_sms_attachments a
             ON a.cloudtalk_sms_id = CAST(SUBSTRING(r.idempotency_key, ?) AS UNSIGNED)
          WHERE r.company_id = ?
            AND r.idempotency_key LIKE CONCAT(?, '%')
            AND NOT EXISTS (
              SELECT 1 FROM ringcentral_sms_attachments x WHERE x.ringcentral_sms_id = r.id
            )",
    )
    .bind(MARKER_END)
    .bind(company_id)
    .bind(COPY_MARKER)
    .execute(&mut *tx)
    .await?;
    // Without read state every copied inbound text would show as unread again.
    sqlx::query(
        "INSERT INTO ringcentral_sms_thread_reads
           (user_id, company_id, customer_phone_digits, last_read_at)
         SELECT cr.user_id, cr.company_id, cr.customer_phone_digits, cr.last_read_at
           FROM cloudtalk_sms_thread_reads cr
          WHERE cr.company_id = ?
            AND cr.customer_phone_digits IN (
              SELECT DISTINCT r.phone_digits FROM ringcentral_sms r
               WHERE r.company_id = ? AND r.idempotency_key LIKE CONCAT(?, '%')
            )
         ON DUPLICATE KEY UPDATE
           last_read_at = GREATEST(ringcentral_sms_thread_reads.last_read_at,
                                   VALUES(last_read_at))",
    )
    .bind(company_id)
    .bind(company_id)
    .bind(COPY_MARKER)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(messages)
}
