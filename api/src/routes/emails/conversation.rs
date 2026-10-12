//! `GET /v1/emails/{id}/conversation`: an email with the rest of its thread,
//! for the admin activity viewer. Remix: `api.users.activity.email.$emailId`.

use crate::auth::AdminUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use chrono::{DateTime, Utc};
use serde::Serialize;
use utoipa::ToSchema;

/// Long threads are rare; cap them so one click cannot pull hundreds of bodies.
const MAX_MESSAGES: usize = 50;

/// Emails predate `emails.company_id`, so a sender of the company counts too.
const COMPANY_SCOPE: &str = "(e.company_id = ? OR su.company_id = ?)";
const COLUMNS: &str = "CAST(e.id AS SIGNED) AS id, e.thread_id, e.subject, e.body,
    su.email_signature AS signature, e.sent_at, e.sender_email, e.receiver_email,
    e.sender_user_id, su.name AS sender_name,
    (SELECT MIN(er.read_at) FROM email_reads er WHERE er.message_id = e.message_id) AS opened_at";

/// id, email id, file name, url, content type, subtype.
type FileRow = (i64, i64, String, String, Option<String>, Option<String>);

#[derive(sqlx::FromRow)]
struct Row {
    id: i64,
    thread_id: Option<String>,
    subject: Option<String>,
    body: Option<String>,
    signature: Option<String>,
    sent_at: Option<DateTime<Utc>>,
    sender_email: Option<String>,
    receiver_email: Option<String>,
    sender_user_id: Option<i32>,
    sender_name: Option<String>,
    opened_at: Option<DateTime<Utc>>,
}

/// A file of the message.
#[derive(Debug, Serialize, ToSchema)]
pub struct ConversationFile {
    /// The attachment.
    pub id: i64,
    /// File name.
    pub filename: String,
    /// A link that opens it (presigned for stored mail).
    pub url: String,
}

/// One message of the conversation.
#[derive(Debug, Serialize, ToSchema)]
pub struct ConversationMessage {
    /// The email.
    pub id: i64,
    /// Subject.
    pub subject: Option<String>,
    /// Body as stored (HTML or text).
    pub body: String,
    /// The sending employee's signature.
    pub signature: Option<String>,
    /// When it was sent.
    pub sent_at: Option<DateTime<Utc>>,
    /// Sender address.
    pub from: Option<String>,
    /// Recipient address.
    pub to: Option<String>,
    /// The sending employee's name.
    pub sender_name: Option<String>,
    /// Sent by the customer, not an employee.
    pub is_from_customer: bool,
    /// First time the customer opened it.
    pub opened_at: Option<DateTime<Utc>>,
    /// Its files.
    pub attachments: Vec<ConversationFile>,
}

/// `GET /v1/emails/{id}/conversation`.
#[derive(Debug, Serialize, ToSchema)]
pub struct Conversation {
    /// The thread.
    pub thread_id: Option<String>,
    /// Oldest first, the latest 50.
    pub messages: Vec<ConversationMessage>,
    /// Older messages were left out.
    pub truncated: bool,
}

/// Get an email's conversation
///
/// The email and the other messages of its thread (the latest 50), with
/// their files, for an email of the caller's company.
#[utoipa::path(
    get,
    path = "/v1/emails/{id}/conversation",
    operation_id = "get_email_conversation",
    tag = "emails",
    params(("id" = i64, Path, description = "Email id")),
    responses(
        (status = 200, description = "Conversation", body = Conversation),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such email in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn conversation(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<Conversation>> {
    let pool = &state.pool;
    let company = user.company_id;
    let email: Option<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM emails e LEFT JOIN users su ON su.id = e.sender_user_id
          WHERE e.id = ? AND e.deleted_at IS NULL AND {COMPANY_SCOPE}"
    )))
    .bind(id)
    .bind(company)
    .bind(company)
    .fetch_optional(pool)
    .await?;
    let email = email.ok_or(ApiError::NotFound)?;
    let thread_id = email.thread_id.clone();
    let mut rows = match &thread_id {
        Some(thread) => {
            sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "SELECT {COLUMNS} FROM emails e LEFT JOIN users su ON su.id = e.sender_user_id
                  WHERE e.thread_id = ? AND e.deleted_at IS NULL AND e.is_draft = 0 AND {COMPANY_SCOPE}
                  ORDER BY e.sent_at DESC, e.id DESC LIMIT ?"
            )))
            .bind(thread)
            .bind(company)
            .bind(company)
            .bind(i64::try_from(MAX_MESSAGES + 1).unwrap_or(51))
            .fetch_all(pool)
            .await?
        }
        None => vec![email],
    };
    let truncated = rows.len() > MAX_MESSAGES;
    rows.truncate(MAX_MESSAGES);
    rows.reverse();
    // Never empty: the email itself is in it.
    let mut sql = sqlx::QueryBuilder::<sqlx::MySql>::new(
        "SELECT CAST(id AS SIGNED), CAST(email_id AS SIGNED), filename, url, content_type, content_subtype
           FROM email_attachments WHERE email_id IN (",
    );
    let mut list = sql.separated(", ");
    for row in &rows {
        list.push_bind(row.id);
    }
    sql.push(") ORDER BY id");
    let files: Vec<FileRow> = sql.build_query_as().fetch_all(pool).await?;
    let mut messages = Vec::with_capacity(rows.len());
    for row in rows {
        let mut attachments = Vec::new();
        for (file_id, email_id, filename, url, kind, sub) in &files {
            if *email_id != row.id {
                continue;
            }
            let kind = kind.as_deref().unwrap_or("").trim().to_lowercase();
            let sub = sub.as_deref().unwrap_or("").trim().to_lowercase();
            let mime = (!kind.is_empty() && !sub.is_empty()).then(|| format!("{kind}/{sub}"));
            attachments.push(ConversationFile {
                id: *file_id,
                filename: filename.clone(),
                url: state.storage.readable_url(url, mime.as_deref()).await,
            });
        }
        messages.push(ConversationMessage {
            id: row.id,
            subject: row.subject,
            body: row.body.unwrap_or_default(),
            signature: row.signature,
            sent_at: row.sent_at,
            from: row.sender_email,
            to: row.receiver_email,
            sender_name: row.sender_name,
            is_from_customer: row.sender_user_id.is_none(),
            opened_at: row.opened_at,
            attachments,
        });
    }
    Ok(Json(Conversation {
        thread_id,
        messages,
        truncated,
    }))
}
