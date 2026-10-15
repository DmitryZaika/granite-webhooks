//! The caller's unsent emails (`emails` rows with `is_draft`), saved when
//! the compose page closes and reopened from the Drafts folder.

use super::schemas::{DraftAttachment, DraftInput, EmailDraft, SavedDraft};
use super::send::{
    insert_participants, is_empty_rich_text, normalize_recipients, split_content_type,
};
use crate::auth::EmployeeUser;
use crate::auth::session::SessionUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use sqlx::{MySql, MySqlPool, Transaction};

/// Kept at the end of the body: "send together" has no column.
const SEND_TOGETHER_MARKER: &str = "<!--gm-send-together:1-->";

fn strip_marker(body: &str) -> String {
    body.replace(&format!("\n{SEND_TOGETHER_MARKER}"), "")
        .replace(SEND_TOGETHER_MARKER, "")
}

/// The draft's thread when it is the caller's own live draft.
async fn owned(pool: &MySqlPool, user: &SessionUser, id: i32) -> ApiResult<String> {
    let thread: Option<Option<String>> = sqlx::query_scalar(
        "SELECT thread_id FROM emails
         WHERE id = ? AND sender_user_id = ? AND company_id = ? AND is_draft = 1
           AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(user.id)
    .bind(user.company_id)
    .fetch_optional(pool)
    .await?;
    thread.flatten().ok_or(ApiError::NotFound)
}

struct Cleaned {
    subject: String,
    body: String,
    to: Vec<String>,
    cc: Vec<String>,
    bcc: Vec<String>,
}

fn clean(input: &DraftInput) -> ApiResult<Cleaned> {
    if input.subject.trim().is_empty() && is_empty_rich_text(&input.body) {
        return Err(ApiError::BadRequest("Draft is empty".to_string()));
    }
    let to = normalize_recipients(&input.to);
    let cc: Vec<String> = normalize_recipients(&input.cc)
        .into_iter()
        .filter(|a| !to.contains(a))
        .collect();
    let bcc: Vec<String> = normalize_recipients(&input.bcc)
        .into_iter()
        .filter(|a| !to.contains(a) && !cc.contains(a))
        .collect();
    let body = strip_marker(&input.body);
    Ok(Cleaned {
        subject: input.subject.trim().to_string(),
        body: if input.send_together {
            format!("{body}\n{SEND_TOGETHER_MARKER}")
        } else {
            body
        },
        to,
        cc,
        bcc,
    })
}

/// Replaces the draft's recipients and files.
async fn replace_parts(
    tx: &mut Transaction<'_, MySql>,
    email_id: i32,
    company_id: i32,
    draft: &Cleaned,
    attachments: &[DraftAttachment],
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM email_participants WHERE email_id = ?")
        .bind(email_id)
        .execute(&mut **tx)
        .await?;
    let group = |list: &[String]| list.iter().map(|a| (a.clone(), None)).collect();
    insert_participants(
        tx,
        email_id,
        company_id,
        &[
            ("to", group(&draft.to)),
            ("cc", group(&draft.cc)),
            ("bcc", group(&draft.bcc)),
        ],
    )
    .await?;
    sqlx::query("DELETE FROM email_attachments WHERE email_id = ?")
        .bind(email_id)
        .execute(&mut **tx)
        .await?;
    for attachment in attachments {
        let (kind, sub) = split_content_type(&attachment.content_type);
        sqlx::query(
            "INSERT INTO email_attachments (email_id, content_type, content_subtype, filename, url)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(email_id)
        .bind(kind)
        .bind(sub)
        .bind(&attachment.filename)
        .bind(&attachment.url)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// The deal must be one of the company's.
async fn check_deal(pool: &MySqlPool, user: &SessionUser, deal_id: Option<i32>) -> ApiResult<()> {
    let Some(deal_id) = deal_id else {
        return Ok(());
    };
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM deals d JOIN customers c ON c.id = d.customer_id
         WHERE d.id = ? AND d.deleted_at IS NULL AND c.company_id = ?",
    )
    .bind(deal_id)
    .bind(user.company_id)
    .fetch_optional(pool)
    .await?;
    found
        .map(|_| ())
        .ok_or_else(|| ApiError::BadRequest("Deal not found".to_string()))
}

/// Get a draft
///
/// One of the caller's drafts, to reopen in the compose page.
#[utoipa::path(
    get,
    path = "/v1/emails/drafts/{id}",
    operation_id = "get_email_draft",
    tag = "emails",
    params(("id" = i32, Path, description = "Draft id")),
    responses(
        (status = 200, description = "The draft", body = EmailDraft),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 404, description = "No such draft of the caller's", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get_draft(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<EmailDraft>> {
    let pool = &state.pool;
    let thread_id = owned(pool, &user, id).await?;
    let (subject, body, deal_id, receiver): (Option<String>, String, Option<u64>, Option<String>) =
        sqlx::query_as("SELECT subject, body, deal_id, receiver_email FROM emails WHERE id = ?")
            .bind(id)
            .fetch_one(pool)
            .await?;
    let participants: Vec<(String, String)> = sqlx::query_as(
        "SELECT type, email FROM email_participants
         WHERE email_id = ? AND type IN ('to', 'cc', 'bcc') ORDER BY position",
    )
    .bind(id)
    .fetch_all(pool)
    .await?;
    let pick = |kind: &str| -> Vec<String> {
        participants
            .iter()
            .filter(|(t, _)| t == kind)
            .map(|(_, email)| email.trim().to_lowercase())
            .filter(|email| !email.is_empty())
            .collect()
    };
    let mut to = pick("to");
    if to.is_empty()
        && let Some(receiver) = receiver
            .map(|r| r.trim().to_lowercase())
            .filter(|r| !r.is_empty())
    {
        to.push(receiver);
    }
    let files: Vec<(String, String, Option<String>, String)> = sqlx::query_as(
        "SELECT filename, content_type, content_subtype, url FROM email_attachments
         WHERE email_id = ? ORDER BY id",
    )
    .bind(id)
    .fetch_all(pool)
    .await?;
    Ok(Json(EmailDraft {
        id,
        thread_id,
        subject: subject.unwrap_or_default(),
        send_together: body.contains(SEND_TOGETHER_MARKER),
        body: strip_marker(&body),
        deal_id: deal_id.and_then(|id| i64::try_from(id).ok()),
        cc: pick("cc"),
        bcc: pick("bcc"),
        to,
        attachments: files
            .into_iter()
            .map(|(filename, kind, sub, url)| DraftAttachment {
                url,
                filename,
                content_type: match sub.filter(|s| !s.is_empty()) {
                    Some(sub) => format!("{kind}/{sub}"),
                    None => kind,
                },
            })
            .collect(),
    }))
}

/// Save a new draft
///
/// Stores an unsent email in the caller's Drafts folder.
#[utoipa::path(
    post,
    path = "/v1/emails/drafts",
    operation_id = "create_email_draft",
    tag = "emails",
    request_body = DraftInput,
    responses(
        (status = 201, description = "Saved", body = SavedDraft),
        (status = 400, description = "No subject and no note, or an unknown deal", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create_draft(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<DraftInput>,
) -> ApiResult<(StatusCode, Json<SavedDraft>)> {
    let draft = clean(&input)?;
    check_deal(&state.pool, &user, input.deal_id).await?;
    let thread_id = uuid::Uuid::new_v4().to_string();
    let mut tx = state.pool.begin().await?;
    let id = sqlx::query(
        "INSERT INTO emails
           (sender_user_id, subject, body, message_id, sender_email, receiver_email,
            thread_id, deal_id, company_id, is_draft, sent_at)
         VALUES (?, ?, ?, NULL, ?, ?, ?, ?, ?, 1, NOW())",
    )
    .bind(user.id)
    .bind(&draft.subject)
    .bind(&draft.body)
    .bind(&user.email)
    .bind(draft.to.first().cloned().unwrap_or_default())
    .bind(&thread_id)
    .bind(input.deal_id)
    .bind(user.company_id)
    .execute(&mut *tx)
    .await?
    .last_insert_id();
    let id = i32::try_from(id).unwrap_or(i32::MAX);
    replace_parts(&mut tx, id, user.company_id, &draft, &input.attachments).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(SavedDraft { id, thread_id })))
}

/// Update a draft
///
/// Replaces the content, recipients and files of one of the caller's drafts
/// and moves it to the top of Drafts.
#[utoipa::path(
    put,
    path = "/v1/emails/drafts/{id}",
    operation_id = "update_email_draft",
    tag = "emails",
    params(("id" = i32, Path, description = "Draft id")),
    request_body = DraftInput,
    responses(
        (status = 200, description = "Saved", body = SavedDraft),
        (status = 400, description = "No subject and no note, or an unknown deal", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 404, description = "No such draft of the caller's", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update_draft(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<DraftInput>,
) -> ApiResult<Json<SavedDraft>> {
    let draft = clean(&input)?;
    let thread_id = owned(&state.pool, &user, id).await?;
    check_deal(&state.pool, &user, input.deal_id).await?;
    let mut tx = state.pool.begin().await?;
    sqlx::query(
        "UPDATE emails SET subject = ?, body = ?, receiver_email = ?, deal_id = ?,
                sent_at = NOW(), html_body = NULL
         WHERE id = ?",
    )
    .bind(&draft.subject)
    .bind(&draft.body)
    .bind(draft.to.first().cloned().unwrap_or_default())
    .bind(input.deal_id)
    .bind(id)
    .execute(&mut *tx)
    .await?;
    replace_parts(&mut tx, id, user.company_id, &draft, &input.attachments).await?;
    tx.commit().await?;
    Ok(Json(SavedDraft { id, thread_id }))
}

/// Delete a draft
///
/// Discards one of the caller's drafts (after it was sent, or emptied).
#[utoipa::path(
    delete,
    path = "/v1/emails/drafts/{id}",
    operation_id = "delete_email_draft",
    tag = "emails",
    params(("id" = i32, Path, description = "Draft id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 404, description = "No such draft of the caller's", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_draft(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    owned(&state.pool, &user, id).await?;
    sqlx::query("UPDATE emails SET deleted_at = NOW() WHERE id = ? AND is_draft = 1")
        .bind(id)
        .execute(&state.pool)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
