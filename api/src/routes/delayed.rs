//! Scheduled emails and texts of the caller: list, edit, send now, cancel.
//!
//! - `GET /v1/delayed/sms?phone_digits=` / `GET /v1/delayed/emails?thread_id=&customer_id=`
//! - `PUT /v1/delayed/{kind}/{id}` → new text and send time
//! - `POST /v1/delayed/{kind}/{id}/send-now`
//! - `DELETE /v1/delayed/{kind}/{id}` → cancel
//!
//! Reading the list first sends the caller's messages that are due (and
//! drops those whose customer wrote back, when asked to). The scheduled job
//! (`POST /v1/jobs/sms-followups`) sends everyone else's, e-signature
//! requests included.
//! Replaces the Remix `api.sms.delayed` and `api.email.delayed` routes.

use crate::auth::EmployeeUser;
use crate::auth::session::{SessionUser, find_user};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::routes::emails::schemas::{SendAttachment, SignatureRequestInput};
use crate::routes::emails::send::{Cleaned, deliver};
use crate::routes::sms::provider::SmsProvider;
use crate::routes::sms::send::{SendSmsRequest, SmsFile, Undelivered, check, deliver_now};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::MySqlPool;
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};

/// Due messages sent per table and call.
const DUE_BATCH_LIMIT: i64 = 20;
const MAX_DELAY_DAYS: i64 = 90;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_sms))
        .routes(routes!(list_emails))
        .routes(routes!(edit, cancel))
        .routes(routes!(send_now))
}

/// Which scheduled messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Texts.
    Sms,
    /// Emails.
    Emails,
}

impl Kind {
    const fn table(self) -> &'static str {
        match self {
            Self::Sms => "delayed_sms",
            Self::Emails => "delayed_emails",
        }
    }

    const fn text_field(self) -> &'static str {
        match self {
            Self::Sms => "text",
            Self::Emails => "body",
        }
    }
}

/// A scheduled message.
#[derive(Debug, Serialize, ToSchema)]
pub struct Pending {
    /// The row.
    pub id: i64,
    /// When it goes out (UTC).
    pub send_at: DateTime<Utc>,
    /// The text; for an email the subject and body, shortened.
    pub text: String,
    /// The full text or email body.
    pub message: String,
    /// Dropped if the customer writes first.
    pub cancel_if_customer_messages: bool,
}

fn json(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or(Value::Null)
}

fn field(payload: &Value, key: &str) -> String {
    payload[key].as_str().unwrap_or("").to_string()
}

fn email_preview(payload: &Value) -> String {
    let subject = field(payload, "subject").trim().to_string();
    let body = field(payload, "body").trim().to_string();
    let joined = if !subject.is_empty() && !body.is_empty() {
        format!("{subject} — {body}")
    } else if subject.is_empty() {
        body
    } else {
        subject
    };
    joined.chars().take(240).collect()
}

fn last10(value: &str) -> String {
    let digits: String = value.chars().filter(char::is_ascii_digit).collect();
    if digits.len() >= 10 {
        digits[digits.len() - 10..].to_string()
    } else {
        String::new()
    }
}

/// A due or pending row.
#[derive(sqlx::FromRow)]
struct Row {
    id: i64,
    company_id: i32,
    user_id: i32,
    customer_id: Option<i32>,
    cancel_if_customer_messages: bool,
    baseline_at: NaiveDateTime,
    send_at: NaiveDateTime,
    payload_json: String,
    /// Phone of a text; recipients (JSON) of an email.
    target: String,
    provider: Option<String>,
}

async fn customer_contact(
    pool: &MySqlPool,
    company_id: i32,
    customer_id: i32,
) -> Result<(Vec<String>, Vec<String>), sqlx::Error> {
    let emails: Vec<Option<String>> = sqlx::query_scalar(
        "SELECT ce.email FROM customers_emails ce JOIN customers c ON c.id = ce.customer_id
          WHERE ce.customer_id = ? AND c.company_id = ? AND c.deleted_at IS NULL",
    )
    .bind(customer_id)
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    let phones: Option<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT phone, phone_2 FROM customers WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(customer_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    let emails = emails
        .into_iter()
        .flatten()
        .map(|email| email.trim().to_lowercase())
        .filter(|email| !email.is_empty())
        .collect();
    let phones = phones
        .map(<[Option<String>; 2]>::from)
        .into_iter()
        .flatten()
        .flatten()
        .map(|phone| last10(&phone))
        .filter(|phone| phone.len() == 10)
        .collect();
    Ok((emails, phones))
}

/// The customer wrote (email or text) after `since`.
async fn customer_messaged_since(
    pool: &MySqlPool,
    company_id: i32,
    since: NaiveDateTime,
    emails: &[String],
    phones: &[String],
    customer_id: Option<i32>,
) -> Result<bool, sqlx::Error> {
    let mut emails: Vec<String> = emails
        .iter()
        .map(|e| e.trim().to_lowercase())
        .filter(|e| !e.is_empty())
        .collect();
    emails.sort();
    emails.dedup();
    let mut phones: Vec<String> = phones
        .iter()
        .map(|p| last10(p))
        .filter(|p| p.len() == 10)
        .collect();
    phones.sort();
    phones.dedup();
    let customer = customer_id.filter(|id| *id > 0).unwrap_or(0);
    if !emails.is_empty() || customer > 0 {
        let mut sql = sqlx::QueryBuilder::<sqlx::MySql>::new(
            "SELECT e.id FROM emails e WHERE e.deleted_at IS NULL AND e.sender_user_id IS NULL AND e.sent_at > ",
        );
        sql.push_bind(since)
            .push(" AND (e.company_id = ")
            .push_bind(company_id)
            .push(" OR e.company_id IS NULL) AND ((")
            .push_bind(customer)
            .push(" > 0 AND EXISTS (SELECT 1 FROM email_participants pc WHERE pc.email_id = e.id AND pc.type = 'from' AND pc.customer_id = ")
            .push_bind(customer)
            .push("))");
        if !emails.is_empty() {
            sql.push(" OR LOWER(TRIM(SUBSTRING_INDEX(SUBSTRING_INDEX(e.sender_email, '<', -1), '>', 1))) IN (");
            let mut list = sql.separated(", ");
            for email in &emails {
                list.push_bind(email.clone());
            }
            sql.push(") OR EXISTS (SELECT 1 FROM email_participants ep WHERE ep.email_id = e.id AND ep.type = 'from' AND LOWER(TRIM(ep.email)) IN (");
            let mut list = sql.separated(", ");
            for email in &emails {
                list.push_bind(email.clone());
            }
            sql.push("))");
        }
        sql.push(") LIMIT 1");
        let found: Option<i64> = sql.build_query_scalar().fetch_optional(pool).await?;
        if found.is_some() {
            return Ok(true);
        }
    }
    for phone in &phones {
        for table in SmsProvider::ALL.map(SmsProvider::sms_table) {
            let found: Option<i64> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT CAST(id AS SIGNED) FROM {table}
                  WHERE company_id = ? AND direction = 'inbound' AND created_date > ?
                    AND (phone_digits = ? OR sender10 = ?
                         OR RIGHT(REGEXP_REPLACE(COALESCE(sender, ''), '[^0-9]', ''), 10) = ?)
                  LIMIT 1"
            )))
            .bind(company_id)
            .bind(since)
            .bind(phone)
            .bind(phone)
            .bind(phone)
            .fetch_optional(pool)
            .await?;
            if found.is_some() {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// The addresses and phones a reply could come from.
async fn contacts(
    pool: &MySqlPool,
    kind: Kind,
    row: &Row,
) -> Result<(Vec<String>, Vec<String>), sqlx::Error> {
    let (mut emails, mut phones) = match kind {
        Kind::Emails => (
            serde_json::from_str::<Vec<String>>(&row.target).unwrap_or_default(),
            Vec::new(),
        ),
        Kind::Sms => (Vec::new(), vec![row.target.clone()]),
    };
    if let Some(customer) = row.customer_id {
        let (more_emails, more_phones) = customer_contact(pool, row.company_id, customer).await?;
        emails.extend(more_emails);
        phones.extend(more_phones);
    }
    Ok((emails, phones))
}

async fn customer_replied(pool: &MySqlPool, kind: Kind, row: &Row) -> Result<bool, sqlx::Error> {
    if !row.cancel_if_customer_messages {
        return Ok(false);
    }
    let (emails, phones) = contacts(pool, kind, row).await?;
    // A text's reply check looks at addresses and phones only, as the CRM did.
    let customer = if kind == Kind::Emails {
        row.customer_id
    } else {
        None
    };
    customer_messaged_since(
        pool,
        row.company_id,
        row.baseline_at,
        &emails,
        &phones,
        customer,
    )
    .await
}

fn select(kind: Kind) -> String {
    let (target, provider) = match kind {
        Kind::Sms => ("phone_digits", "provider"),
        Kind::Emails => ("recipient_emails", "NULL"),
    };
    format!(
        "SELECT CAST(id AS SIGNED) AS id, company_id, user_id, customer_id,
                COALESCE(cancel_if_customer_messages, 0) = 1 AS cancel_if_customer_messages,
                baseline_at, send_at, payload_json, {target} AS target, {provider} AS provider
           FROM {}",
        kind.table()
    )
}

async fn mark(
    pool: &MySqlPool,
    kind: Kind,
    id: i64,
    status: &str,
    error: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE {} SET status = ?, sent_at = IF(? = 'sent', UTC_TIMESTAMP(), NULL), error_message = ?
          WHERE id = ? AND status = 'sending'",
        kind.table()
    )))
    .bind(status)
    .bind(status)
    .bind(error.map(|e| e.chars().take(500).collect::<String>()))
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// The deal a scheduled message belongs to: the one it was sent from, else
/// the customer's latest.
async fn deal_for(pool: &MySqlPool, row: &Row) -> Result<Option<i32>, sqlx::Error> {
    if let Some(deal_id) = number(&json(&row.payload_json), "dealId").filter(|id| *id > 0) {
        return Ok(Some(deal_id));
    }
    let Some(customer_id) = row.customer_id else {
        return Ok(None);
    };
    // `deals.id` is unsigned: cast it, or decoding fails and the notice is lost.
    let id: Option<i64> = sqlx::query_scalar(
        "SELECT CAST(d.id AS SIGNED) FROM deals d
           JOIN customers c ON c.id = d.customer_id
          WHERE d.customer_id = ? AND c.company_id = ? AND d.deleted_at IS NULL
          ORDER BY d.is_won IS NULL DESC, d.id DESC
          LIMIT 1",
    )
    .bind(customer_id)
    .bind(row.company_id)
    .fetch_optional(pool)
    .await?;
    Ok(id.and_then(|id| i32::try_from(id).ok()))
}

/// The bell notice for a sent (or failed) scheduled message, e.g.
/// `Scheduled SMS sent: "See you Monday"`, at most 255 characters.
fn outcome_message(kind: Kind, payload: &Value, sent: bool) -> String {
    let key = match kind {
        Kind::Sms => "text",
        Kind::Emails => "subject",
    };
    let preview = field(payload, key)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let what = match kind {
        Kind::Sms => "Scheduled SMS",
        Kind::Emails => "Scheduled email",
    };
    let outcome = if sent { "sent" } else { "failed to send" };
    let message = if preview.is_empty() {
        format!("{what} {outcome}")
    } else {
        format!("{what} {outcome}: \"{preview}\"")
    };
    if message.chars().count() > 255 {
        format!("{}...\"", message.chars().take(251).collect::<String>())
    } else {
        message
    }
}

/// Tells the sender their scheduled message went out (or could not). The
/// bell lists deal notifications, so a message with no deal gets none; a
/// failure here never changes the message's own status.
async fn notify_outcome(pool: &MySqlPool, kind: Kind, row: &Row, sent: bool) {
    let notified = async {
        let Some(deal_id) = deal_for(pool, row).await? else {
            return Ok(());
        };
        sqlx::query(
            "INSERT INTO notifications (user_id, customer_id, deal_id, message, notification_type, due_at)
             VALUES (?, ?, ?, ?, ?, NOW())",
        )
        .bind(row.user_id)
        .bind(row.customer_id)
        .bind(deal_id)
        .bind(outcome_message(kind, &json(&row.payload_json), sent))
        .bind(if sent {
            "delayed_message_sent"
        } else {
            "delayed_message_failed"
        })
        .execute(pool)
        .await
        .map(|_| ())
    };
    if let Err(error) = notified.await {
        tracing::warn!(id = row.id, table = kind.table(), %error, "scheduled message notice not saved");
    }
}

fn number(payload: &Value, key: &str) -> Option<i32> {
    match &payload[key] {
        Value::String(text) => text.trim().parse().ok(),
        Value::Number(number) => number.as_i64().and_then(|n| i32::try_from(n).ok()),
        _ => None,
    }
    .filter(|id| *id > 0)
}

fn strings(payload: &Value, key: &str) -> Vec<String> {
    payload[key]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item.as_str().map(str::to_string))
        .collect()
}

/// A JSON list stored as a string (or as a list).
fn embedded_list(payload: &Value, key: &str) -> Vec<Value> {
    match &payload[key] {
        Value::String(text) => serde_json::from_str(text).unwrap_or_default(),
        Value::Array(items) => items.clone(),
        _ => Vec::new(),
    }
}

/// The scheduled email as the send route takes it; signature documents are
/// sent with the row as the send's identity.
fn email_from_payload(payload: &Value, delayed_email_id: i64) -> Cleaned {
    let signature_requests: Vec<SignatureRequestInput> =
        embedded_list(payload, "signatureRequests")
            .into_iter()
            .filter_map(|request| serde_json::from_value(request).ok())
            .collect();
    let (subject, body) = crate::routes::emails::signatures::with_defaults(
        field(payload, "subject").trim(),
        &field(payload, "body"),
        &signature_requests,
    );
    let attachments = embedded_list(payload, "uploadedAttachments")
        .iter()
        .filter_map(|file| {
            let kind = file["contentType"].as_str().unwrap_or("application");
            let content_type = match file["contentSubtype"].as_str() {
                Some(sub) if !kind.contains('/') => format!("{kind}/{sub}"),
                _ => kind.to_string(),
            };
            Some(SendAttachment {
                url: file["url"].as_str()?.to_string(),
                filename: file["filename"]
                    .as_str()
                    .unwrap_or("attachment")
                    .to_string(),
                content_type,
                token: file["token"].as_str().map(str::to_string),
            })
        })
        .collect();
    Cleaned {
        to: strings(payload, "to"),
        cc: strings(payload, "cc"),
        bcc: strings(payload, "bcc"),
        subject: subject.trim().to_string(),
        body,
        thread_id: Some(field(payload, "threadId")).filter(|id| !id.is_empty()),
        deal_id: number(payload, "dealId"),
        customer_id: number(payload, "customerId"),
        send_together: payload["sendTogether"] == true,
        attachments,
        template_id: number(payload, "templateId"),
        send_at: None,
        cancel_if_customer_messages: false,
        automated: false,
        signature_requests,
        signature_send_key: None,
        delayed_email_id: Some(delayed_email_id),
    }
}

fn sms_from_payload(payload: &Value) -> SendSmsRequest {
    let attachments = embedded_list(payload, "attachments")
        .iter()
        .filter_map(|file| {
            Some(SmsFile {
                s3_key: file["s3Key"].as_str()?.to_string(),
                content_type: file["contentType"].as_str().unwrap_or("").to_string(),
                filename: file["filename"].as_str().unwrap_or("").to_string(),
                width: file["width"].as_i64().and_then(|w| i32::try_from(w).ok()),
                height: file["height"].as_i64().and_then(|h| i32::try_from(h).ok()),
            })
        })
        .collect();
    SendSmsRequest {
        phone_digits: field(payload, "phoneDigits"),
        text: field(payload, "text"),
        attachments,
        template_id: number(payload, "templateId"),
        customer_id: None,
        send_at: None,
        cancel_if_customer_messages: false,
    }
}

/// Sends one claimed row; the failure reason to store.
async fn deliver_row(
    state: &AppState,
    kind: Kind,
    row: &Row,
    user: &SessionUser,
) -> Result<(), String> {
    let payload = json(&row.payload_json);
    if payload.is_null() {
        return Err("missing_sender_or_payload".into());
    }
    match kind {
        Kind::Emails => deliver(state, user, &email_from_payload(&payload, row.id))
            .await
            .map(|_| ())
            .map_err(|err| err.to_string()),
        Kind::Sms => {
            let provider = row
                .provider
                .as_deref()
                .and_then(SmsProvider::parse)
                .unwrap_or(SmsProvider::Cloudtalk);
            let request = sms_from_payload(&payload);
            let seat = crate::routes::sms::send::seat(&state.pool, provider, user)
                .await
                .map_err(|err| err.to_string())?
                .ok_or("agent_not_linked")?;
            check(user, &request)?;
            match deliver_now(state, user, provider, &seat, &request).await {
                Ok(_) => Ok(()),
                Err(Undelivered::Api(err)) => Err(err.to_string()),
                Err(Undelivered::Sms(failure)) => Err(failure.error),
            }
        }
    }
}

/// Sends the user's due messages (or drops those the customer answered).
pub async fn process_due(
    state: &AppState,
    company_id: i32,
    user_id: i32,
) -> Result<(), sqlx::Error> {
    process_due_in(state, Some((company_id, user_id)))
        .await
        .map(|_| ())
}

/// Due messages looked at (emails, texts).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
pub struct DueRun {
    /// Due emails looked at.
    pub emails: usize,
    /// Due texts looked at.
    pub sms: usize,
}

/// Sends the due messages of one user (`Some((company, user))`) or of
/// everyone (the scheduled job).
pub async fn process_due_in(
    state: &AppState,
    scope: Option<(i32, i32)>,
) -> Result<DueRun, sqlx::Error> {
    let pool = &state.pool;
    let mut run = DueRun::default();
    for kind in [Kind::Emails, Kind::Sms] {
        let scoped = if scope.is_some() {
            " AND company_id = ? AND user_id = ?"
        } else {
            ""
        };
        let mut query = sqlx::query_as::<_, Row>(sqlx::AssertSqlSafe(format!(
            "{} WHERE status = 'pending' AND send_at <= UTC_TIMESTAMP(){scoped}
              ORDER BY send_at, id LIMIT ?",
            select(kind)
        )));
        if let Some((company_id, user_id)) = scope {
            query = query.bind(company_id).bind(user_id);
        }
        let rows = query.bind(DUE_BATCH_LIMIT).fetch_all(pool).await?;
        match kind {
            Kind::Emails => run.emails = rows.len(),
            Kind::Sms => run.sms = rows.len(),
        }
        for row in rows {
            let claimed = sqlx::query(sqlx::AssertSqlSafe(format!(
                "UPDATE {} SET status = 'sending' WHERE id = ? AND status = 'pending' AND send_at <= UTC_TIMESTAMP()",
                kind.table()
            )))
            .bind(row.id)
            .execute(pool)
            .await?
            .rows_affected();
            if claimed != 1 {
                continue;
            }
            if customer_replied(pool, kind, &row).await? {
                mark(pool, kind, row.id, "cancelled", None).await?;
                continue;
            }
            let Some(user) = find_user(pool, row.user_id, row.company_id).await? else {
                mark(
                    pool,
                    kind,
                    row.id,
                    "failed",
                    Some("missing_sender_or_payload"),
                )
                .await?;
                notify_outcome(pool, kind, &row, false).await;
                continue;
            };
            match deliver_row(state, kind, &row, &user).await {
                Ok(()) => {
                    mark(pool, kind, row.id, "sent", None).await?;
                    notify_outcome(pool, kind, &row, true).await;
                }
                Err(error) => {
                    tracing::warn!(id = row.id, table = kind.table(), %error, "scheduled message failed");
                    mark(pool, kind, row.id, "failed", Some(&error)).await?;
                    notify_outcome(pool, kind, &row, false).await;
                }
            }
        }
    }
    Ok(run)
}

async fn cancel_row(
    pool: &MySqlPool,
    kind: Kind,
    user: &SessionUser,
    id: i64,
) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE {} SET status = 'cancelled' WHERE id = ? AND company_id = ? AND user_id = ? AND status = 'pending'",
        kind.table()
    )))
    .bind(id)
    .bind(user.company_id)
    .bind(user.id)
    .execute(pool)
    .await?
    .rows_affected()
        == 1)
}

/// Pending rows of the caller, dropping those the customer answered.
async fn pending(
    state: &AppState,
    kind: Kind,
    user: &SessionUser,
    filter: &str,
    binds: &[String],
) -> ApiResult<Vec<Pending>> {
    process_due(state, user.company_id, user.id).await?;
    let mut query = sqlx::query_as::<_, Row>(sqlx::AssertSqlSafe(format!(
        "{} WHERE company_id = ? AND user_id = ? AND status = 'pending' AND {filter} ORDER BY send_at, id",
        select(kind)
    )))
    .bind(user.company_id)
    .bind(user.id);
    for bind in binds {
        query = query.bind(bind);
    }
    let rows = query.fetch_all(&state.pool).await?;
    let mut items = Vec::new();
    for row in rows {
        if customer_replied(&state.pool, kind, &row).await?
            && cancel_row(&state.pool, kind, user, row.id).await?
        {
            continue;
        }
        let payload = json(&row.payload_json);
        let (text, message) = match kind {
            Kind::Sms => (field(&payload, "text"), field(&payload, "text")),
            Kind::Emails => (email_preview(&payload), field(&payload, "body")),
        };
        items.push(Pending {
            id: row.id,
            send_at: row.send_at.and_utc(),
            text,
            message,
            cancel_if_customer_messages: row.cancel_if_customer_messages,
        });
    }
    Ok(items)
}

/// Query of `GET /v1/delayed/sms`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SmsQuery {
    /// The thread's phone, 5 to 15 digits.
    #[serde(default)]
    pub phone_digits: String,
}

/// List my scheduled texts to a phone
///
/// Sends the caller's due messages first.
#[utoipa::path(
    get,
    path = "/v1/delayed/sms",
    operation_id = "list_delayed_sms",
    tag = "delayed",
    params(SmsQuery),
    responses(
        (status = 200, description = "Scheduled texts, soonest first", body = [Pending]),
        (status = 400, description = "Not a thread phone", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list_sms(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Query(query): Query<SmsQuery>,
) -> ApiResult<Json<Vec<Pending>>> {
    let phone = query.phone_digits.trim();
    if !(5..=15).contains(&phone.len()) || !phone.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ApiError::BadRequest("invalid_phone".into()));
    }
    Ok(Json(
        pending(
            &state,
            Kind::Sms,
            &user,
            "phone_digits = ?",
            &[phone.to_string()],
        )
        .await?,
    ))
}

/// Query of `GET /v1/delayed/emails`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct EmailsQuery {
    /// The conversation.
    #[serde(default)]
    pub thread_id: String,
    /// Or the customer.
    #[serde(default)]
    pub customer_id: Option<i32>,
}

/// List my scheduled emails in a conversation or to a customer
///
/// Sends the caller's due messages first; empty without a thread or customer.
#[utoipa::path(
    get,
    path = "/v1/delayed/emails",
    operation_id = "list_delayed_emails",
    tag = "delayed",
    params(EmailsQuery),
    responses(
        (status = 200, description = "Scheduled emails, soonest first", body = [Pending]),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list_emails(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Query(query): Query<EmailsQuery>,
) -> ApiResult<Json<Vec<Pending>>> {
    let thread = query.thread_id.trim().to_string();
    let customer = query.customer_id.filter(|id| *id > 0).unwrap_or(0);
    if thread.is_empty() && customer == 0 {
        process_due(&state, user.company_id, user.id).await?;
        return Ok(Json(Vec::new()));
    }
    Ok(Json(
        pending(
            &state,
            Kind::Emails,
            &user,
            "((? <> '' AND thread_id = ?) OR (CAST(? AS SIGNED) > 0 AND customer_id = ?))",
            &[
                thread.clone(),
                thread,
                customer.to_string(),
                customer.to_string(),
            ],
        )
        .await?,
    ))
}

/// Body of `PUT /v1/delayed/{kind}/{id}`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct EditPending {
    /// The new text (email body).
    pub text: String,
    /// When to send it: in the future, within 90 days.
    pub send_at: DateTime<Utc>,
}

/// Edit a scheduled message
///
/// Replaces the text (an email's body) and the send time.
#[utoipa::path(
    put,
    path = "/v1/delayed/{kind}/{id}",
    operation_id = "edit_delayed",
    tag = "delayed",
    params(("kind" = inline(Kind), Path, description = "`sms` or `emails`"), ("id" = i64, Path, description = "Row id")),
    request_body = EditPending,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Past or too far send time", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "Not the caller's pending message", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn edit(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((kind, id)): Path<(Kind, i64)>,
    JsonBody(input): JsonBody<EditPending>,
) -> ApiResult<StatusCode> {
    let now = Utc::now();
    if input.send_at <= now {
        return Err(ApiError::BadRequest("Choose a future time".into()));
    }
    if input.send_at - now > Duration::days(MAX_DELAY_DAYS) {
        return Err(ApiError::BadRequest(
            "Send time must be within 90 days".into(),
        ));
    }
    let raw: Option<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT payload_json FROM {} WHERE id = ? AND company_id = ? AND user_id = ? AND status = 'pending'",
        kind.table()
    )))
    .bind(id)
    .bind(user.company_id)
    .bind(user.id)
    .fetch_optional(&state.pool)
    .await?;
    let mut payload = raw
        .map(|raw| json(&raw))
        .filter(Value::is_object)
        .ok_or(ApiError::NotFound)?;
    payload[kind.text_field()] = Value::String(input.text);
    let updated = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE {} SET payload_json = ?, send_at = ?
          WHERE id = ? AND company_id = ? AND user_id = ? AND status = 'pending'",
        kind.table()
    )))
    .bind(payload.to_string())
    .bind(input.send_at.naive_utc())
    .bind(id)
    .bind(user.company_id)
    .bind(user.id)
    .execute(&state.pool)
    .await?
    .rows_affected();
    if updated == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Send a scheduled message now
///
/// Sends it right away, even if the customer has written since.
#[utoipa::path(
    post,
    path = "/v1/delayed/{kind}/{id}/send-now",
    operation_id = "send_delayed_now",
    tag = "delayed",
    params(("kind" = inline(Kind), Path, description = "`sms` or `emails`"), ("id" = i64, Path, description = "Row id")),
    responses(
        (status = 204, description = "Sent (or failed; the row says which)"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "Not the caller's pending message", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn send_now(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((kind, id)): Path<(Kind, i64)>,
) -> ApiResult<StatusCode> {
    let released = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE {} SET send_at = UTC_TIMESTAMP(), cancel_if_customer_messages = 0
          WHERE id = ? AND company_id = ? AND user_id = ? AND status = 'pending'",
        kind.table()
    )))
    .bind(id)
    .bind(user.company_id)
    .bind(user.id)
    .execute(&state.pool)
    .await?
    .rows_affected();
    if released == 0 {
        return Err(ApiError::NotFound);
    }
    process_due(&state, user.company_id, user.id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Cancel a scheduled message
///
/// It will not be sent.
#[utoipa::path(
    delete,
    path = "/v1/delayed/{kind}/{id}",
    operation_id = "cancel_delayed",
    tag = "delayed",
    params(("kind" = inline(Kind), Path, description = "`sms` or `emails`"), ("id" = i64, Path, description = "Row id")),
    responses(
        (status = 204, description = "Cancelled"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "Not the caller's pending message", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn cancel(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((kind, id)): Path<(Kind, i64)>,
) -> ApiResult<StatusCode> {
    if cancel_row(&state.pool, kind, &user, id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_notice_names_the_message() {
        let text = json!({ "text": "See  you\n Monday" });
        assert_eq!(
            outcome_message(Kind::Sms, &text, true),
            "Scheduled SMS sent: \"See you Monday\""
        );
        assert_eq!(
            outcome_message(Kind::Emails, &json!({}), false),
            "Scheduled email failed to send"
        );
        let long = outcome_message(Kind::Sms, &json!({ "text": "a".repeat(300) }), true);
        assert_eq!(long.chars().count(), 255);
        assert!(long.ends_with("a...\""));
    }
}
