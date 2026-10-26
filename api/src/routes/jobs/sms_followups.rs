//! `POST /v1/jobs/sms-followups`: the scheduled follow-up texts and emails.
//!
//! The due steps of SMS / email flows, the drip texts of lead lists
//! (`scheduled_sms`) and everyone's scheduled emails and texts; then tells reps
//! about follow-ups that ended (`followups::alerts`) and, at 9 AM, who is
//! waiting on them (`followups::digest`). Replaces the
//! Remix `api.sms-followups.process` route.

use crate::auth::job::JobCaller;
use crate::auth::session::find_user;
use crate::error::{ApiResult, ErrorBody};
use crate::followups::processor::{FollowupRun, process_due_followups, rep_sms_provider};
use crate::openapi::ToSchema;
use crate::routes::calls::to_e164;
use crate::routes::delayed::{DueRun, process_due_in};
use crate::routes::sms::flows::variable_names;
use crate::routes::sms::provider::SmsProvider;
use crate::state::AppState;
use crate::webhooks_client::OutgoingSms;
use axum::Json;
use axum::extract::State;
use lambda_http::tracing;
use serde::Serialize;

const DRIP_BATCH_LIMIT: i64 = 50;
const SMS_MAX_TEXT: usize = 1600;

/// Drip texts looked at, sent and failed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
pub struct DripRun {
    /// Due texts looked at.
    pub processed: usize,
    /// Texts sent.
    pub sent: usize,
    /// Texts that failed (with the reason on the row).
    pub failed: usize,
}

/// What a run did.
#[derive(Debug, Serialize, ToSchema)]
pub struct SmsFollowupsRun {
    /// Flow enrollments looked at.
    pub processed: usize,
    /// Flow steps sent.
    pub sent: usize,
    /// Flow runs that failed.
    pub failed: usize,
    /// Flow runs stopped by a reply or call.
    pub stopped: usize,
    /// Drip texts of lead lists.
    pub scheduled_sms: DripRun,
    /// Scheduled emails and texts.
    pub delayed: DueRun,
}

#[derive(sqlx::FromRow)]
struct DueDrip {
    id: i64,
    template_id: i32,
    deal_id: i32,
    customer_id: i32,
    user_id: i32,
    company_id: i32,
}

/// Rows a check below skips (source no longer one the template targets,
/// deal deleted or moved off the list) stay pending in case they qualify
/// again; the query leaves them out so they cannot fill the batch.
async fn due_drips(state: &AppState) -> Result<Vec<DueDrip>, sqlx::Error> {
    sqlx::query_as(
        "SELECT s.id, s.template_id, s.deal_id, s.customer_id, s.user_id, s.company_id
           FROM scheduled_sms s
           JOIN customers c ON c.id = s.customer_id AND c.company_id = s.company_id
           JOIN deals d ON d.id = s.deal_id
           JOIN cloudtalk_sms_templates t ON t.id = s.template_id
          WHERE s.status = 'pending' AND s.send_at <= UTC_TIMESTAMP()
            AND c.deleted_at IS NULL
            AND JSON_CONTAINS(COALESCE(t.customer_sources, JSON_ARRAY('leads')),
                              JSON_QUOTE(LOWER(TRIM(IFNULL(c.source, '')))))
            AND d.deleted_at IS NULL AND (s.list_id IS NULL OR d.list_id = s.list_id)
          ORDER BY s.send_at ASC, s.id ASC
          LIMIT ?",
    )
    .bind(DRIP_BATCH_LIMIT)
    .fetch_all(&state.pool)
    .await
}

async fn mark_failed(state: &AppState, id: i64, reason: &str) -> Result<(), sqlx::Error> {
    let reason: String = reason.chars().take(500).collect();
    sqlx::query("UPDATE scheduled_sms SET status = 'failed', error_message = ? WHERE id = ? AND status = 'pending'")
        .bind(reason)
        .bind(id)
        .execute(&state.pool)
        .await?;
    Ok(())
}

async fn mark_sent(
    state: &AppState,
    id: i64,
    provider_id: Option<String>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE scheduled_sms SET status = 'sent', sent_at = UTC_TIMESTAMP(), error_message = NULL, provider_message_id = ?
          WHERE id = ? AND status = 'pending'",
    )
    .bind(provider_id)
    .bind(id)
    .execute(&state.pool)
    .await?;
    Ok(())
}

/// The provider's id of the stored text.
async fn provider_id(
    state: &AppState,
    provider: SmsProvider,
    id: i64,
) -> Result<Option<String>, sqlx::Error> {
    let (table, column) = (provider.sms_table(), provider.id_column());
    if !provider.numeric_ids() {
        let found: Option<Option<String>> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT {column} FROM {table} WHERE id = ?"
        )))
        .bind(id)
        .fetch_optional(&state.pool)
        .await?;
        return Ok(found.flatten());
    }
    let found: Option<Option<i64>> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT CAST({column} AS SIGNED) FROM {table} WHERE id = ?"
    )))
    .bind(id)
    .fetch_optional(&state.pool)
    .await?;
    Ok(found.flatten().map(|id| id.to_string()))
}

/// The lead's phone (last ten digits) and the filled text, or why the text
/// cannot go out.
async fn drip_text(
    state: &AppState,
    row: &DueDrip,
) -> Result<Result<(String, String), &'static str>, sqlx::Error> {
    let pool = &state.pool;
    let template: Option<(String, bool)> = sqlx::query_as(
        "SELECT body, deleted_at IS NOT NULL FROM cloudtalk_sms_templates
          WHERE id = ? AND company_id = ? AND user_id IS NULL LIMIT 1",
    )
    .bind(row.template_id)
    .bind(row.company_id)
    .fetch_optional(pool)
    .await?;
    let Some((mut text, false)) = template else {
        return Ok(Err("Template deleted"));
    };
    let phones: Option<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT phone, phone_2 FROM customers WHERE id = ? AND company_id = ? LIMIT 1",
    )
    .bind(row.customer_id)
    .bind(row.company_id)
    .fetch_optional(pool)
    .await?;
    let phone = phones.and_then(|pair| {
        <[_; 2]>::from(pair)
            .into_iter()
            .flatten()
            .map(|p| p.trim().to_string())
            .find(|p| !p.is_empty())
    });
    let digits: String = phone
        .as_deref()
        .unwrap_or("")
        .chars()
        .filter(char::is_ascii_digit)
        .collect();
    let phone10 = if digits.len() > 10 {
        digits[digits.len() - 10..].to_string()
    } else {
        digits
    };
    if phone.as_deref().and_then(to_e164).is_none() || phone10.len() < 10 {
        return Ok(Err("Customer has no phone number"));
    }
    if text.contains("{{") {
        let Ok(data) = common::crud::template::fetch_template_variable_data(
            pool,
            row.user_id,
            Some(row.deal_id),
            Some(row.customer_id),
            row.company_id,
        )
        .await
        else {
            return Ok(Err("Failed to fill template variables"));
        };
        text = common::utils::template::replace_template_variables(&text, &data);
        if variable_names(&text).iter().any(|name| !name.is_empty()) {
            return Ok(Err("Unfilled template variables"));
        }
    }
    if text.trim().is_empty() || text.encode_utf16().count() > SMS_MAX_TEXT {
        return Ok(Err("Invalid SMS body"));
    }
    Ok(Ok((phone10, text)))
}

/// Sends one drip text; whether it went out (a failure is on the row).
async fn send_drip(state: &AppState, row: &DueDrip) -> Result<bool, sqlx::Error> {
    let (phone10, text) = match drip_text(state, row).await? {
        Ok(ready) => ready,
        Err(reason) => {
            mark_failed(state, row.id, reason).await?;
            return Ok(false);
        }
    };
    let Some(rep) = find_user(&state.pool, row.user_id, row.company_id).await? else {
        mark_failed(state, row.id, "Sales rep is not linked").await?;
        return Ok(false);
    };
    // The rep's provider: Telnyx when they have a Telnyx line there.
    let Some(provider) = rep_sms_provider(state, row.company_id, &rep).await? else {
        mark_failed(state, row.id, "SMS is not enabled").await?;
        return Ok(false);
    };
    let Some(seat) = crate::routes::sms::send::seat(&state.pool, provider, &rep).await? else {
        mark_failed(state, row.id, "Sales rep is not linked").await?;
        return Ok(false);
    };
    let outgoing = OutgoingSms {
        provider: provider.as_str().to_string(),
        company_id: row.company_id,
        user_id: row.user_id,
        agent_id: seat.agent_id.clone(),
        phone_digits: phone10,
        text,
        attachments: Vec::new(),
        is_automated: true,
        idempotency_key: Some(format!("sms-drip-{}", row.id)),
    };
    match crate::routes::sms::send::send_text(state, provider, &seat, &outgoing).await {
        Ok(ids) => {
            let provider_id = match ids.first() {
                Some(id) => provider_id(state, provider, *id).await?,
                None => None,
            };
            mark_sent(state, row.id, provider_id).await?;
            Ok(true)
        }
        Err(failure) => {
            let reason = if failure.error == "agent_no_number" {
                "Sales rep has no SMS number".to_string()
            } else if failure.error == "opted_out" {
                "Customer opted out of texts (STOP)".to_string()
            } else {
                failure.error
            };
            mark_failed(state, row.id, &reason).await?;
            Ok(false)
        }
    }
}

/// Sends the due drip texts (at most 50).
pub async fn process_due_drips(state: &AppState) -> Result<DripRun, sqlx::Error> {
    let rows = due_drips(state).await?;
    let mut run = DripRun {
        processed: rows.len(),
        ..DripRun::default()
    };
    for row in &rows {
        match send_drip(state, row).await {
            Ok(true) => run.sent += 1,
            Ok(false) => run.failed += 1,
            Err(err) => tracing::error!(?err, id = row.id, "scheduled list SMS dispatch failed"),
        }
    }
    Ok(run)
}

/// Send due follow-ups
///
/// Sends the due steps of SMS / email flows, the drip texts of lead lists
/// and everyone's scheduled emails and texts.
#[crate::openapi::path(
    post,
    path = "/v1/jobs/sms-followups",
    operation_id = "run_sms_followups",
    tag = "jobs",
    responses(
        (status = 200, description = "What was done", body = SmsFollowupsRun),
        (status = 401, description = "Missing or wrong job key", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("job_key" = []))
)]
pub async fn run(
    State(state): State<AppState>,
    _job: JobCaller,
) -> ApiResult<Json<SmsFollowupsRun>> {
    let FollowupRun {
        processed,
        sent,
        failed,
        stopped,
    } = process_due_followups(&state).await?;
    // Telling reps never holds up the sends.
    match crate::followups::alerts::alert_ended_followups(&state).await {
        Ok(0) => {}
        Ok(alerted) => tracing::info!(alerted, "follow-up end alerts sent"),
        Err(err) => tracing::warn!(?err, "follow-up end alerts failed"),
    }
    match crate::followups::digest::send_quiet_digests(&state, chrono::Utc::now()).await {
        Ok(0) => {}
        Ok(sent) => tracing::info!(sent, "follow-up digests sent"),
        Err(err) => tracing::warn!(?err, "follow-up digests failed"),
    }
    let scheduled_sms = process_due_drips(&state).await?;
    let delayed = process_due_in(&state, None).await?;
    Ok(Json(SmsFollowupsRun {
        processed,
        sent,
        failed,
        stopped,
        scheduled_sms,
        delayed,
    }))
}
