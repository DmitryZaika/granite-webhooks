//! The company's AI follow-up instruction.
//!
//! A free-text rule (e.g. "two days between messages") the follow-up
//! writer must apply. Stored as the reserved `instructions` row titled `AI follow-up instruction`, which the
//! instructions tree hides.

use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use regex::Regex;
use serde::{Deserialize, Serialize};
use sqlx::MySqlPool;
use std::sync::LazyLock;

/// Reserved `instructions.title` of the rule.
pub const TITLE: &str = "AI follow-up instruction";
/// Longest instruction, in characters, after trimming.
pub const MAX_CHARS: usize = 2000;

static TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]*>").expect("tag regex"));

/// The company's AI follow-up instruction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct FollowupInstruction {
    /// The rule as plain text, trimmed; empty when the company has none.
    pub instruction: String,
}

/// The saved instruction as plain text; empty when none.
pub async fn load(pool: &MySqlPool, company_id: i32) -> Result<String, sqlx::Error> {
    let text: Option<Option<String>> = sqlx::query_scalar(
        "SELECT rich_text FROM instructions
          WHERE company_id = ? AND title = ?
          ORDER BY id ASC LIMIT 1",
    )
    .bind(company_id)
    .bind(TITLE)
    .fetch_optional(pool)
    .await?;
    let text = text.flatten().unwrap_or_default();
    Ok(TAG.replace_all(&text, "").trim().to_string())
}

/// Get the AI follow-up instruction
///
/// The company's rule for AI-written follow-ups (cadence, tone, what to
/// mention), as plain text. Empty when none is saved.
#[crate::openapi::path(
    get,
    path = "/v1/followups/instruction",
    operation_id = "get_followup_instruction",
    tag = "followups",
    responses(
        (status = 200, description = "The instruction", body = FollowupInstruction),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<FollowupInstruction>> {
    let instruction = load(&state.pool, user.company_id).await?;
    Ok(Json(FollowupInstruction { instruction }))
}

/// Save the AI follow-up instruction
///
/// Replaces the company's rule with `instruction`, trimmed. An empty (or
/// blank) instruction deletes it. Answers the saved text.
#[crate::openapi::path(
    put,
    path = "/v1/followups/instruction",
    operation_id = "save_followup_instruction",
    tag = "followups",
    request_body = FollowupInstruction,
    responses(
        (status = 200, description = "The saved instruction", body = FollowupInstruction),
        (status = 400, description = "Longer than 2000 characters", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn put(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<FollowupInstruction>,
) -> ApiResult<Json<FollowupInstruction>> {
    let text = input.instruction.trim().to_string();
    if text.chars().count() > MAX_CHARS {
        return Err(ApiError::BadRequest(format!(
            "instruction is longer than {MAX_CHARS} characters"
        )));
    }
    let mut tx = state.pool.begin().await?;
    let ids: Vec<i32> = sqlx::query_scalar(
        "SELECT id FROM instructions WHERE company_id = ? AND title = ? ORDER BY id ASC FOR UPDATE",
    )
    .bind(user.company_id)
    .bind(TITLE)
    .fetch_all(&mut *tx)
    .await?;
    match ids.split_first() {
        _ if text.is_empty() => {
            sqlx::query("DELETE FROM instructions WHERE company_id = ? AND title = ?")
                .bind(user.company_id)
                .bind(TITLE)
                .execute(&mut *tx)
                .await?;
        }
        Some((first, extras)) => {
            sqlx::query("UPDATE instructions SET rich_text = ? WHERE id = ?")
                .bind(&text)
                .bind(first)
                .execute(&mut *tx)
                .await?;
            // Duplicates (older saves raced): keep the first, as reads do.
            for id in extras {
                sqlx::query("DELETE FROM instructions WHERE id = ?")
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        None => {
            sqlx::query("INSERT INTO instructions (title, rich_text, company_id) VALUES (?, ?, ?)")
                .bind(TITLE)
                .bind(&text)
                .bind(user.company_id)
                .execute(&mut *tx)
                .await?;
        }
    }
    tx.commit().await?;
    Ok(Json(FollowupInstruction { instruction: text }))
}
