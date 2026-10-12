//! `GET /v1/teach-mode/progress`: how the company's sales reps answered the
//! training questions, for the Employee Progress page.
//!
//! Replaces the Remix `admin.employee-progress` loader.

use crate::auth::EmployeeUser;
use crate::error::{ApiResult, ErrorBody};
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use serde::Serialize;
use sqlx::FromRow;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(progress))
}

/// `positions.id` of a sales rep.
const SALES_REP: i32 = 1;

/// A sales rep of the company.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow, ToSchema)]
pub struct ProgressUser {
    /// User id.
    pub id: i32,
    /// Email.
    pub email: String,
    /// Name.
    pub name: Option<String>,
}

/// One answer to a training question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow, ToSchema)]
pub struct AnswerAttempt {
    /// Who answered.
    pub employee_id: i32,
    /// Their try at the question, from 1.
    pub attempt_number: i32,
    /// The question.
    pub question_id: i32,
    /// The answer they picked; null when none.
    pub selected_answer_id: Option<i32>,
    /// Whether it was right, as recorded.
    pub is_correct: Option<bool>,
    /// The question.
    pub question_text: String,
    /// What they picked.
    pub selected_answer_text: Option<String>,
    /// The right answer.
    pub correct_answer_text: String,
}

/// The training progress.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct TeachProgress {
    /// The company's sales reps, by name.
    pub users: Vec<ProgressUser>,
    /// Their answers, by employee, newest try first.
    pub attempts: Vec<AnswerAttempt>,
}

/// Training progress
///
/// The company's sales reps and every answer they gave to the training
/// questions.
#[utoipa::path(
    get,
    path = "/v1/teach-mode/progress",
    operation_id = "get_teach_progress",
    tag = "company",
    responses(
        (status = 200, description = "Reps and their answers", body = TeachProgress),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn progress(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<TeachProgress>> {
    let users: Vec<ProgressUser> = sqlx::query_as(
        "SELECT u.id, u.email, u.name FROM users u
          WHERE u.company_id = ? AND u.is_deleted = 0
            AND EXISTS (SELECT 1 FROM users_positions up
                         WHERE up.user_id = u.id AND up.position_id = ?)
          ORDER BY u.name, u.email",
    )
    .bind(user.company_id)
    .bind(SALES_REP)
    .fetch_all(&state.pool)
    .await?;
    let attempts: Vec<AnswerAttempt> = sqlx::query_as(
        "SELECT aa.employee_id, aa.attempt_number, aa.question_id, aa.selected_answer_id,
                aa.is_correct, q.text AS question_text, sa.text AS selected_answer_text,
                ca.text AS correct_answer_text
           FROM answer_attempts aa
           JOIN questions q ON aa.question_id = q.id
           LEFT JOIN answer_choices sa ON aa.selected_answer_id = sa.id
           JOIN answer_choices ca ON q.id = ca.question_id AND ca.is_correct = 1
          WHERE aa.employee_id IN (SELECT id FROM users WHERE company_id = ?)
            AND aa.deleted_at IS NULL
          ORDER BY aa.employee_id, aa.attempt_number DESC, aa.created_date",
    )
    .bind(user.company_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(TeachProgress { users, attempts }))
}
