//! `POST /v1/ai/crm-manual-feedback`: the chat's thumbs-down on a CRM manual
//! answer.
//!
//! `dislike` gets a corrected answer (up to two attempts); the user
//! then `approve`s one or `escalate`s to the super admins, which saves the
//! conversation to `message_feedback` for the instruction corrections page.
//! Replaces the Remix `api.crm-manual-feedback` route.

use crate::ai::Caller;
use crate::auth::EmployeeUser;
use crate::chat::crm_manual;
use crate::chat::manual_feedback::{self, DialogMessage};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use serde_json::json;

const ESCALATION_MESSAGE: &str =
    "I'm sorry — your review was added to our team and we will check it.";

/// What the feedback is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Intent {
    /// The answer was wrong: ask for a corrected one.
    Dislike,
    /// A corrected answer was right: save it for review.
    Approve,
    /// Still wrong: hand it to the super admins.
    Escalate,
}

/// Body of `POST /v1/ai/crm-manual-feedback`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FeedbackInput {
    /// `dislike`, `approve` or `escalate`.
    pub intent: Intent,
    /// The user's question (`dislike`).
    #[serde(default)]
    pub question: String,
    /// The answer they disliked (`dislike`).
    #[serde(default)]
    pub original_answer: String,
    /// The correction attempt, 1 or 2 (`dislike`, `approve`).
    #[serde(default)]
    pub attempt: i64,
    /// Corrections already disliked (`dislike`).
    #[serde(default)]
    pub previous_corrections: Vec<String>,
    /// The conversation (`approve`, `escalate`).
    #[serde(default)]
    pub dialog: Vec<DialogMessage>,
    /// The approved answer (`approve`).
    #[serde(default)]
    pub corrected_answer: String,
    /// The manual section it is about.
    #[serde(default)]
    pub section_id: Option<String>,
    /// A suggested change for that section.
    #[serde(default)]
    pub proposed_manual_change: Option<String>,
    /// What the user wrote.
    #[serde(default)]
    pub feedback_text: Option<String>,
}

/// A corrected answer (`dislike`).
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CorrectionOut {
    /// The corrected answer.
    pub correction: String,
    /// The attempt it answers.
    pub attempt: i64,
    /// The manual section it belongs to.
    pub section_id: Option<String>,
    /// The change it suggests for that section.
    pub proposed_manual_change: Option<String>,
    /// Always false.
    pub escalate: bool,
}

/// Saved feedback (`approve`, `escalate`).
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SavedFeedback {
    /// The `message_feedback` row.
    pub feedback_id: u64,
    /// What to tell the user.
    pub message: String,
    /// True when escalated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub escalate: Option<bool>,
}

/// The answer to a feedback call.
#[derive(Debug, Serialize, ToSchema)]
#[serde(untagged)]
pub enum FeedbackOut {
    /// For `dislike`.
    Correction(CorrectionOut),
    /// For `approve` and `escalate`.
    Saved(SavedFeedback),
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.is_empty())
}

/// Give feedback on a CRM manual answer
///
/// `dislike`: a corrected answer from the manual and the user's access
/// (attempt 1 or 2). `approve` / `escalate`: the conversation is saved for
/// the super admins' instruction corrections page.
#[crate::openapi::path(
    post,
    path = "/v1/ai/crm-manual-feedback",
    operation_id = "give_crm_manual_feedback",
    tag = "ai",
    request_body = FeedbackInput,
    responses(
        (status = 200, description = "The correction, or the saved feedback", body = FeedbackOut),
        (status = 400, description = "Invalid request", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 500, description = "Saving failed", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
#[allow(clippy::too_many_lines)]
pub async fn feedback(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<FeedbackInput>,
) -> ApiResult<Json<FeedbackOut>> {
    let invalid = || ApiError::BadRequest("Invalid request".into());
    let company_id = (user.company_id > 0).then_some(user.company_id);
    let FeedbackInput {
        intent,
        question,
        original_answer,
        attempt,
        previous_corrections,
        dialog,
        corrected_answer,
        section_id,
        proposed_manual_change: proposed,
        feedback_text,
    } = input;
    if intent == Intent::Dislike {
        if question.is_empty() || original_answer.is_empty() || !(1..=2).contains(&attempt) {
            return Err(invalid());
        }
        let access = crm_manual::access(&state.pool, &user).await?;
        let caller = Caller {
            feature: "crm_manual_correction",
            user_id: Some(user.id),
            company_id,
        };
        let correction = manual_feedback::correction(
            &state,
            caller,
            &question,
            &original_answer,
            attempt,
            &previous_corrections,
            Some(&access),
        )
        .await;
        return Ok(Json(FeedbackOut::Correction(CorrectionOut {
            correction: correction.answer,
            attempt,
            section_id: correction
                .section_id
                .or_else(|| manual_feedback::guess_section_id(&question)),
            proposed_manual_change: correction.proposed_manual_change,
            escalate: false,
        })));
    }
    let (status, approved) = if intent == Intent::Approve {
        if corrected_answer.is_empty() || !(1..=2).contains(&attempt) {
            return Err(invalid());
        }
        ("user_approved", Some(corrected_answer))
    } else {
        ("escalated", None)
    };
    if dialog.is_empty() {
        return Err(ApiError::BadRequest("Invalid dialog payload".into()));
    }
    let feedback_text = non_empty(feedback_text);
    let section_id =
        non_empty(section_id).or_else(|| manual_feedback::guess_section_from_dialog(&dialog));
    let mut history = json!({
        "dialog": dialog,
        "feedbackText": feedback_text,
        "userId": user.id,
        "userName": user.name,
        "companyId": company_id.unwrap_or(0),
    });
    if let Some(approved) = &approved {
        history["approvedAnswer"] = json!(approved);
    }
    let saved = sqlx::query(
        "INSERT INTO message_feedback
            (user_id, company_id, history, feedback_text, status, manual_section_id, proposed_manual_change)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(user.id)
    .bind(company_id)
    .bind(history.to_string())
    .bind(&feedback_text)
    .bind(status)
    .bind(&section_id)
    .bind(non_empty(proposed))
    .execute(&state.pool)
    .await
    .map_err(|err| {
        tracing::error!(error = %err, "CRM manual feedback not saved");
        ApiError::Internal(
            if approved.is_some() { "Failed to save approved feedback" } else { "Failed to save escalated feedback" }.into(),
        )
    })?;
    let escalated = approved.is_none();
    Ok(Json(FeedbackOut::Saved(SavedFeedback {
        feedback_id: saved.last_insert_id(),
        message: if escalated {
            ESCALATION_MESSAGE.into()
        } else {
            "Thanks for confirming the updated answer.".into()
        },
        escalate: escalated.then_some(true),
    })))
}
