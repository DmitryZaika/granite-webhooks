//! `POST /v1/emails/inbound-push`: tell the phones about a new inbound email.
//!
//! The `webhooks` Lambda stores an email SES received, then calls this (by
//! its key) with the new row's id. The email is pushed (`{granite: "1",
//! type: "email", emailId, threadId}`: ids only) to the phones of the users
//! whose inbox badge (`GET /v1/emails/unread`) it raises.

use super::handlers::{has_inbound_email, unread_inbox};
use super::sql::{Mailbox, Sql, normalized};
use crate::auth::job::JobCaller;
use crate::error::{ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::phone::devices::push_to_users;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, MySqlPool};
use std::collections::BTreeMap;

/// Body of `POST /v1/emails/inbound-push`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct InboundPush {
    /// The stored inbound email (`emails` row id, the `id` of a message in
    /// `GET /v1/emails/threads`).
    pub email_id: i64,
}

/// How many phones were told.
#[derive(Debug, Serialize, ToSchema)]
pub struct InboundPushSent {
    /// Phones FCM accepted the push for; 0 when nobody is told (unknown,
    /// deleted, draft or outbound email, a company without inbound email,
    /// nobody whose inbox shows it, or no signed-in phone).
    pub pushed: usize,
}

/// The email as the push needs it.
#[derive(FromRow)]
struct PushedEmail {
    thread_id: String,
    company_id: i32,
}

/// A user the email may be delivered to.
#[derive(FromRow)]
struct Candidate {
    id: i32,
    email: String,
}

/// The live inbound email `email_id` of a company: none when it is deleted,
/// a draft, without a thread or company, or sent by an employee (from the
/// CRM, or from a company user's own address).
async fn inbound_email(pool: &MySqlPool, email_id: i64) -> sqlx::Result<Option<PushedEmail>> {
    let sender = normalized("e.sender_email");
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT e.thread_id, e.company_id FROM emails e
          WHERE e.id = ? AND e.deleted_at IS NULL AND e.is_draft = 0
            AND e.thread_id IS NOT NULL AND e.company_id IS NOT NULL
            AND e.sender_user_id IS NULL
            AND NOT EXISTS (SELECT 1 FROM users su
                             WHERE su.company_id = e.company_id AND su.is_deleted = 0
                               AND LOWER(TRIM(su.email)) = {sender})"
    )))
    .bind(email_id)
    .fetch_optional(pool)
    .await
}

/// Employees of the company the email names or whose deal it is on: a
/// superset of the users whose inbox shows it.
async fn candidates(
    pool: &MySqlPool,
    email_id: i64,
    company_id: i32,
) -> sqlx::Result<Vec<Candidate>> {
    let receiver = normalized("e.receiver_email");
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT u.id, u.email FROM emails e
           JOIN users u ON u.company_id = ? AND u.is_deleted = 0
                       AND (u.is_employee = 1 OR u.is_admin = 1 OR u.is_superuser = 1)
          WHERE e.id = ?
            AND (u.id = e.receiver_user_id
                 OR LOWER(TRIM(u.email)) = {receiver}
                 OR EXISTS (SELECT 1 FROM email_participants ep
                             WHERE ep.email_id = e.id AND ep.type IN ('to', 'cc')
                               AND (ep.user_id = u.id OR ep.email = LOWER(TRIM(u.email))))
                 OR EXISTS (SELECT 1 FROM deals d
                             WHERE d.user_id = u.id AND d.deleted_at IS NULL
                               AND d.id IN (SELECT td.deal_id FROM emails td
                                             WHERE td.thread_id = e.thread_id
                                               AND td.deleted_at IS NULL)))
          ORDER BY u.id"
    )))
    .bind(company_id)
    .bind(email_id)
    .fetch_all(pool)
    .await
}

/// The users whose unread inbox (`GET /v1/emails/unread`) holds the email.
async fn recipients(pool: &MySqlPool, email_id: i64, company_id: i32) -> sqlx::Result<Vec<i32>> {
    let mut users = Vec::new();
    for candidate in candidates(pool, email_id, company_id).await? {
        let mailbox = Mailbox {
            user_id: candidate.id,
            company_id,
            email: candidate.email.trim().to_lowercase(),
        };
        let unread = unread_inbox(&mailbox);
        let mut args = unread.args;
        args.push(email_id.into());
        let shown: Option<i64> = Sql::new(format!("SELECT 1 {} AND e.id = ?", unread.sql), args)
            .query_scalar()
            .fetch_optional(pool)
            .await?;
        if shown.is_some() {
            users.push(candidate.id);
        }
    }
    Ok(users)
}

/// Push a new inbound email to its recipients' phones
///
/// Called by the `webhooks` Lambda (job key), not by the CRM or the app:
/// after it stored an email SES received, it sends the new email's id. The
/// email is pushed to the signed-in phones of the users whose inbox shows it
/// unread (as `GET /v1/emails/unread` counts it) as data
/// `{granite: "1", type: "email", emailId, threadId}`: ids only, never the
/// subject, sender or text. The app opens it with
/// `GET /v1/emails/threads/{thread_id}`. Nothing is pushed (`pushed: 0`) for
/// an unknown, deleted or draft email, one sent by an employee, a company
/// without inbound email, or when nobody's inbox shows it. Calling it again
/// pushes again.
#[crate::openapi::path(
    post,
    path = "/v1/emails/inbound-push",
    operation_id = "push_inbound_email",
    tag = "emails",
    request_body = InboundPush,
    responses(
        (status = 200, description = "Phones told", body = InboundPushSent),
        (status = 400, description = "Invalid body", body = ErrorBody),
        (status = 401, description = "Missing or wrong key", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("job_key" = []))
)]
pub async fn inbound_push(
    State(state): State<AppState>,
    _caller: JobCaller,
    JsonBody(body): JsonBody<InboundPush>,
) -> ApiResult<Json<InboundPushSent>> {
    let nobody = Json(InboundPushSent { pushed: 0 });
    let Some(email) = inbound_email(&state.pool, body.email_id).await? else {
        return Ok(nobody);
    };
    if !has_inbound_email(&state.pool, email.company_id).await? {
        return Ok(nobody);
    }
    let users = recipients(&state.pool, body.email_id, email.company_id).await?;
    let data = BTreeMap::from([
        ("granite".to_string(), "1".to_string()),
        ("type".to_string(), "email".to_string()),
        ("emailId".to_string(), body.email_id.to_string()),
        ("threadId".to_string(), email.thread_id),
    ]);
    let pushed = push_to_users(&state, email.company_id, &users, &data).await?;
    Ok(Json(InboundPushSent { pushed }))
}
