//! The rep's own sign-off for AI follow-up texts.
//!
//! Added under every text the AI writes for the rep
//! (`users.followup_sms_signature`), so the model never has to sign. Emails
//! keep the rep's email signature.

use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use sqlx::MySqlPool;

/// Longest signature, in characters, after trimming.
pub const MAX_CHARS: usize = 200;

/// The caller's sign-off for AI follow-up texts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct FollowupSignature {
    /// The sign-off, line breaks kept; empty when the rep has none.
    pub signature: String,
}

/// Trailing spaces off every line, blank lines off both ends.
fn tidy(signature: &str) -> String {
    signature
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

/// The rep's sign-off; empty when none.
pub async fn load(pool: &MySqlPool, user_id: i32, company_id: i32) -> Result<String, sqlx::Error> {
    let saved: Option<Option<String>> = sqlx::query_scalar(
        "SELECT followup_sms_signature FROM users WHERE id = ? AND company_id = ?",
    )
    .bind(user_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    Ok(tidy(&saved.flatten().unwrap_or_default()))
}

fn comparable(line: &str) -> String {
    line.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// The text with the sign-off under it, once: a sign-off already there is
/// kept, and the model's own closing lines that repeat it (a name) go.
pub fn with_text_signature(body: &str, signature: &str) -> String {
    let signature = tidy(signature);
    let body = body.trim_end();
    if signature.is_empty() || comparable(body).ends_with(&comparable(&signature)) {
        return body.to_string();
    }
    let signed: Vec<String> = signature.lines().map(comparable).collect();
    let mut lines: Vec<&str> = body.lines().collect();
    while lines.len() > 1
        && lines
            .last()
            .is_some_and(|line| line.trim().is_empty() || signed.contains(&comparable(line)))
    {
        lines.pop();
    }
    format!("{}\n\n{signature}", lines.join("\n").trim_end())
}

/// Get your AI follow-up text signature
///
/// The caller's sign-off, added under every text the AI writes for them.
/// Empty when none is saved.
#[crate::openapi::path(
    get,
    path = "/v1/followups/signature",
    operation_id = "get_followup_signature",
    tag = "followups",
    responses(
        (status = 200, description = "The signature", body = FollowupSignature),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<FollowupSignature>> {
    let signature = load(&state.pool, user.id, user.company_id).await?;
    Ok(Json(FollowupSignature { signature }))
}

/// Save your AI follow-up text signature
///
/// Replaces the caller's sign-off with `signature` (trailing spaces and
/// blank edge lines dropped). An empty signature removes it. Texts already
/// scheduled keep the sign-off they were written with. Answers the saved
/// signature.
#[crate::openapi::path(
    put,
    path = "/v1/followups/signature",
    operation_id = "save_followup_signature",
    tag = "followups",
    request_body = FollowupSignature,
    responses(
        (status = 200, description = "The saved signature", body = FollowupSignature),
        (status = 400, description = "Longer than 200 characters", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn put(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<FollowupSignature>,
) -> ApiResult<Json<FollowupSignature>> {
    let signature = tidy(&input.signature);
    if signature.chars().count() > MAX_CHARS {
        return Err(ApiError::BadRequest(format!(
            "signature is longer than {MAX_CHARS} characters"
        )));
    }
    sqlx::query("UPDATE users SET followup_sms_signature = ? WHERE id = ? AND company_id = ?")
        .bind(Some(&signature).filter(|s| !s.is_empty()))
        .bind(user.id)
        .bind(user.company_id)
        .execute(&state.pool)
        .await?;
    Ok(Json(FollowupSignature { signature }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sign_off_goes_under_the_text_once() {
        let signature = "Liza \nGranite Depot of Indianapolis\n";
        assert_eq!(
            with_text_signature("Want me to resend the quote?", signature),
            "Want me to resend the quote?\n\nLiza\nGranite Depot of Indianapolis"
        );
        // Already signed: unchanged.
        let signed = "Want me to resend the quote?\n\nLiza\nGranite Depot of Indianapolis";
        assert_eq!(with_text_signature(signed, signature), signed);
        // The model's own "Liza" goes, so the name is not there twice.
        assert_eq!(
            with_text_signature("Want me to resend the quote?\nLiza", signature),
            "Want me to resend the quote?\n\nLiza\nGranite Depot of Indianapolis"
        );
        assert_eq!(with_text_signature("Hi Ann ", "  "), "Hi Ann");
        assert_eq!(
            tidy(" \nDeana Craig  \r\n859-396-7076\n\n"),
            "Deana Craig\n859-396-7076"
        );
    }
}
