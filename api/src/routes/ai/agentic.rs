//! Agentic chat questions, answered by the claude-job-runner agent.
//!
//! - `POST /v1/ai/agentic-questions` → queue a question
//! - `GET  /v1/ai/agentic-questions/{id}` → its progress, and the answer
//! - `POST /v1/ai/agentic-questions/{id}/agent-token` → the runner's token to
//!   read the API as the asker (see [`crate::auth::agent`])
//!
//! A question is a `chat_messages` row (`sender = 'user'`, `is_agentic = 1`)
//! the runner polls and answers with a `sender = 'ai'` row it points at from
//! `payload.reply_id`. `payload.requester_is_admin` (set here from the
//! session) tells the runner whether the agent may show other users' data.
//! Replaces the Remix `api.agentic-chat` route.

use crate::auth::EmployeeUser;
use crate::auth::agent::{self, AgentRunner, Claims, TOKEN_TTL_SECS};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Longest question, in UTF-16 units like the browser counts.
pub const MAX_QUESTION_LENGTH: usize = 8000;
const MAX_TITLE_LENGTH: usize = 200;

/// Body of `POST /v1/ai/agentic-questions`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewAgenticQuestion {
    /// The question (1 to 8000 characters once trimmed).
    pub query: String,
    /// Start a new chat instead of continuing the latest one.
    #[serde(default)]
    pub new_chat: bool,
}

/// A queued question.
#[derive(Debug, Serialize, ToSchema)]
pub struct QueuedQuestion {
    /// The question's id, to poll.
    pub id: i64,
    /// Always `queued`.
    pub status: &'static str,
}

/// Where a question is.
#[derive(Debug, Serialize, ToSchema, PartialEq, Eq)]
pub struct AgenticJob {
    /// The question's id.
    pub id: i64,
    /// `queued` (not picked up yet), `pending`, `done` or `failed`.
    pub status: String,
    /// The answer, once `done`.
    pub reply: Option<String>,
    /// Why the agent failed, once `failed`.
    pub error: Option<String>,
    /// What the agent answered before failing, if anything.
    pub partial: Option<String>,
}

/// The chat's title: the question on one line, cut to 200 characters.
pub fn chat_title(question: &str) -> String {
    let one_line = question.split_whitespace().collect::<Vec<_>>().join(" ");
    let units: Vec<u16> = one_line.encode_utf16().collect();
    if units.len() <= MAX_TITLE_LENGTH {
        return if one_line.is_empty() {
            "New chat".into()
        } else {
            one_line
        };
    }
    format!(
        "{}...",
        String::from_utf16_lossy(&units[..MAX_TITLE_LENGTH - 3])
    )
}

/// Ask the agent
///
/// Queues a question for the claude-job-runner agent, in the caller's latest
/// chat or a new one. Poll `GET /v1/ai/agentic-questions/{id}` for the answer.
#[utoipa::path(
    post,
    path = "/v1/ai/agentic-questions",
    operation_id = "ask_agent",
    tag = "ai",
    request_body = NewAgenticQuestion,
    responses(
        (status = 201, description = "Queued", body = QueuedQuestion),
        (status = 400, description = "Empty or too long question", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn ask(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<NewAgenticQuestion>,
) -> ApiResult<(StatusCode, Json<QueuedQuestion>)> {
    let question = input.query.trim();
    let length = question.encode_utf16().count();
    if length == 0 || length > MAX_QUESTION_LENGTH {
        return Err(ApiError::BadRequest(format!(
            "query must be 1 to {MAX_QUESTION_LENGTH} characters"
        )));
    }
    let mut tx = state.pool.begin().await?;
    let latest: Option<i64> = if input.new_chat {
        None
    } else {
        sqlx::query_scalar(
            "SELECT CAST(id AS SIGNED) FROM chats WHERE user_id = ? AND company_id = ?
              ORDER BY updated_at DESC, id DESC LIMIT 1",
        )
        .bind(user.id)
        .bind(user.company_id)
        .fetch_optional(&mut *tx)
        .await?
    };
    let chat_id = if let Some(id) = latest {
        id
    } else {
        let created =
            sqlx::query("INSERT INTO chats (user_id, company_id, title) VALUES (?, ?, ?)")
                .bind(user.id)
                .bind(user.company_id)
                .bind(chat_title(question))
                .execute(&mut *tx)
                .await?;
        i64::try_from(created.last_insert_id())
            .map_err(|err| ApiError::Internal(err.to_string()))?
    };
    // A real JSON boolean, which the runner checks.
    let inserted = sqlx::query(
        "INSERT INTO chat_messages (chat_id, sender, content, is_agentic, payload)
         VALUES (?, 'user', ?, 1, JSON_OBJECT('requester_is_admin', CAST(? AS JSON)))",
    )
    .bind(chat_id)
    .bind(question)
    .bind(if user.is_admin || user.is_superuser {
        "true"
    } else {
        "false"
    })
    .execute(&mut *tx)
    .await?;
    if latest.is_some() {
        sqlx::query("UPDATE chats SET updated_at = CURRENT_TIMESTAMP WHERE id = ?")
            .bind(chat_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    let id = i64::try_from(inserted.last_insert_id())
        .map_err(|err| ApiError::Internal(err.to_string()))?;
    Ok((
        StatusCode::CREATED,
        Json(QueuedQuestion {
            id,
            status: "queued",
        }),
    ))
}

#[derive(sqlx::FromRow)]
struct JobRow {
    id: i64,
    status: Option<String>,
    error: Option<String>,
    partial: Option<String>,
    reply: Option<String>,
}

/// Follow a question
///
/// Where the caller's question is, and the agent's answer once it is done.
/// Other people's questions read as not found.
#[utoipa::path(
    get,
    path = "/v1/ai/agentic-questions/{id}",
    operation_id = "get_agentic_question",
    tag = "ai",
    params(("id" = i64, Path, description = "The question's id")),
    responses(
        (status = 200, description = "The question's state", body = AgenticJob),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such question of the caller", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn job(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i64>,
) -> ApiResult<([(header::HeaderName, &'static str); 1], Json<AgenticJob>)> {
    let row: Option<JobRow> = sqlx::query_as(
        "SELECT CAST(m.id AS SIGNED) AS id, CAST(m.status AS CHAR) AS status,
                JSON_UNQUOTE(JSON_EXTRACT(m.payload, '$.error')) AS error,
                JSON_UNQUOTE(JSON_EXTRACT(m.payload, '$.result')) AS partial,
                r.content AS reply
           FROM chat_messages m
           JOIN chats c ON c.id = m.chat_id
           LEFT JOIN chat_messages r
             ON r.id = CAST(JSON_EXTRACT(m.payload, '$.reply_id') AS UNSIGNED)
            AND r.sender = 'ai'
            AND r.chat_id = m.chat_id
          WHERE m.id = ? AND m.sender = 'user' AND m.is_agentic = 1 AND c.user_id = ?",
    )
    .bind(id)
    .bind(user.id)
    .fetch_optional(&state.pool)
    .await?;
    let row = row.ok_or(ApiError::NotFound)?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(to_job(row))))
}

fn to_job(row: JobRow) -> AgenticJob {
    // JSON null unquotes to the text "null".
    let text = |value: Option<String>| value.filter(|v| !v.is_empty() && v != "null");
    let status = row.status.unwrap_or_else(|| "queued".into());
    AgenticJob {
        id: row.id,
        reply: (status == "done").then(|| row.reply.unwrap_or_default()),
        error: (status == "failed")
            .then(|| text(row.error).unwrap_or_else(|| "The agent could not finish.".into())),
        partial: if status == "failed" {
            text(row.partial)
        } else {
            None
        },
        status,
    }
}

/// A token the agent reads the API with.
#[derive(Debug, Serialize, ToSchema)]
pub struct AgentToken {
    /// Send as `Authorization: Bearer <token>`.
    pub token: String,
    /// When it stops working (unix seconds); it also stops once the question
    /// is `done` or `failed`.
    pub expires_at: i64,
    /// The asker, whom the token acts as.
    pub user_id: i32,
    /// The only company the token sees.
    pub company_id: i32,
}

/// Issue an agent token
///
/// For the claude-job-runner, with `AGENT_RUNNER_KEY`: a token that lets its
/// agent read the API as the person who asked question `id`, in the company
/// they asked in. It reads only (plus `POST /v1/call-transcripts`), never as
/// a superuser, and works for 30 minutes or until the question is answered.
/// Ask for a new one when it runs out mid-answer.
#[utoipa::path(
    post,
    path = "/v1/ai/agentic-questions/{id}/agent-token",
    operation_id = "issue_agent_token",
    tag = "ai",
    params(("id" = i64, Path, description = "The question's id")),
    responses(
        (status = 201, description = "The token", body = AgentToken),
        (status = 401, description = "Missing or wrong runner key", body = ErrorBody),
        (status = 404, description = "No open question asked in the last 7 days by someone who can still act in its company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("agent_runner_key" = []))
)]
pub async fn agent_token(
    State(state): State<AppState>,
    _runner: AgentRunner,
    Path(id): Path<i64>,
) -> ApiResult<(
    StatusCode,
    [(header::HeaderName, &'static str); 1],
    Json<AgentToken>,
)> {
    let (user_id, company_id) = agent::open_question(&state.pool, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    agent::asker(&state.pool, user_id, company_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let claims = Claims {
        q: id,
        u: user_id,
        c: company_id,
        exp: chrono::Utc::now().timestamp() + TOKEN_TTL_SECS,
    };
    lambda_http::tracing::info!(
        question = id,
        user = user_id,
        company = company_id,
        "agent token issued"
    );
    Ok((
        StatusCode::CREATED,
        [(header::CACHE_CONTROL, "no-store")],
        Json(AgentToken {
            token: agent::issue(&state.session_secret, &claims),
            expires_at: claims.exp,
            user_id,
            company_id,
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_are_one_line_and_cut() {
        assert_eq!(chat_title("  Show\n my   deals "), "Show my deals");
        assert_eq!(chat_title("   "), "New chat");
        let long = "a".repeat(250);
        assert_eq!(chat_title(&long), format!("{}...", "a".repeat(197)));
    }

    #[test]
    fn jobs_read_like_the_crm() {
        let row = |status: Option<&str>,
                   error: Option<&str>,
                   partial: Option<&str>,
                   reply: Option<&str>| JobRow {
            id: 9,
            status: status.map(str::to_string),
            error: error.map(str::to_string),
            partial: partial.map(str::to_string),
            reply: reply.map(str::to_string),
        };
        assert_eq!(to_job(row(None, None, None, None)).status, "queued");
        let done = to_job(row(Some("done"), None, None, None));
        assert_eq!(done.reply.as_deref(), Some(""));
        let failed = to_job(row(Some("failed"), Some("null"), Some("half"), None));
        assert_eq!(failed.error.as_deref(), Some("The agent could not finish."));
        assert_eq!(failed.partial.as_deref(), Some("half"));
        assert_eq!(
            to_job(row(Some("pending"), Some("x"), Some("y"), Some("z"))).reply,
            None
        );
    }
}
