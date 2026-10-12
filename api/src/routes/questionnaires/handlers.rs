use super::schemas::{
    AnsweredQuestion, AnswersInput, AnswersSaved, DealQuestionnaire, DefaultQuestionnaire,
    NameInput, NewQuestionnaire, OwnQuestionnaire, Question, QuestionInput,
};
use crate::auth::EmployeeUser;
use crate::auth::session::SessionUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::routes::created::CreatedId;
use crate::routes::deals::queries::deal_in_company;
use crate::routes::deals::service::{Notification, notify_assignees};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use std::collections::HashMap;

const MAX_NAME: usize = 255;
const MAX_QUESTION: usize = 5000;
const MAX_ANSWER: usize = 10_000;
const MAX_NOTE: usize = 5000;

fn checked(text: &str, max: usize, what: &str) -> ApiResult<String> {
    let text = text.trim();
    if text.is_empty() {
        return Err(ApiError::BadRequest(format!("{what} is required")));
    }
    if text.chars().count() > max {
        return Err(ApiError::BadRequest(format!(
            "{what} must be {max} characters or less"
        )));
    }
    Ok(text.to_string())
}

/// A live questionnaire the caller made (only those can be edited).
async fn require_own(state: &AppState, user: &SessionUser, id: i32) -> ApiResult<()> {
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT id FROM sales_questionnaires
         WHERE id = ? AND company_id = ? AND user_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(user.company_id)
    .bind(user.id)
    .fetch_optional(&state.pool)
    .await?;
    found.map(|_| ()).ok_or(ApiError::NotFound)
}

/// List the caller's own questionnaires
///
/// Questionnaires the caller made (not the company-wide ones), newest
/// first, each with its live questions in creation order. Only these can be
/// renamed, deleted or have questions changed.
#[utoipa::path(
    get,
    path = "/v1/questionnaires",
    operation_id = "list_own_questionnaires",
    tag = "questionnaires",
    responses(
        (status = 200, description = "Questionnaires, newest first", body = Vec<OwnQuestionnaire>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list_own(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<Vec<OwnQuestionnaire>>> {
    type Row = (
        i32,
        String,
        Option<DateTime<Utc>>,
        Option<i32>,
        Option<String>,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT q.id, q.name, q.created_at, qq.id, qq.question
         FROM sales_questionnaires q
         LEFT JOIN sales_questionnaire_questions qq
           ON qq.questionnaire_id = q.id AND qq.deleted_at IS NULL
         WHERE q.company_id = ? AND q.user_id = ? AND q.deleted_at IS NULL
         ORDER BY q.created_at DESC, q.id DESC, qq.id",
    )
    .bind(user.company_id)
    .bind(user.id)
    .fetch_all(&state.pool)
    .await?;
    let mut list: Vec<OwnQuestionnaire> = Vec::new();
    for (id, name, created_at, question_id, question) in rows {
        if list.last().is_none_or(|q| q.id != id) {
            list.push(OwnQuestionnaire {
                id,
                name,
                created_at,
                questions: Vec::new(),
            });
        }
        if let (Some(id), Some(question), Some(last)) = (question_id, question, list.last_mut()) {
            last.questions.push(Question { id, question });
        }
    }
    Ok(Json(list))
}

/// Create a questionnaire
///
/// A questionnaire of the caller's own with at least one question.
#[utoipa::path(
    post,
    path = "/v1/questionnaires",
    operation_id = "create_questionnaire",
    tag = "questionnaires",
    request_body = NewQuestionnaire,
    responses(
        (status = 201, description = "Created", body = CreatedId),
        (status = 400, description = "Empty or too long name or question, no questions", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<NewQuestionnaire>,
) -> ApiResult<(StatusCode, Json<CreatedId>)> {
    let name = checked(&input.name, MAX_NAME, "Name")?;
    if input.questions.is_empty() {
        return Err(ApiError::BadRequest(
            "Add at least one questionnaire question.".into(),
        ));
    }
    let questions = input
        .questions
        .iter()
        .map(|q| checked(q, MAX_QUESTION, "Question"))
        .collect::<ApiResult<Vec<_>>>()?;
    let mut tx = state.pool.begin().await?;
    let id = sqlx::query(
        "INSERT INTO sales_questionnaires (name, company_id, user_id) VALUES (?, ?, ?)",
    )
    .bind(&name)
    .bind(user.company_id)
    .bind(user.id)
    .execute(&mut *tx)
    .await?
    .last_insert_id();
    for question in questions {
        sqlx::query(
            "INSERT INTO sales_questionnaire_questions (questionnaire_id, question) VALUES (?, ?)",
        )
        .bind(id)
        .bind(question)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(CreatedId {
            id: i64::try_from(id).unwrap_or_default(),
        }),
    ))
}

/// Rename a questionnaire
///
/// Only the caller's own questionnaires.
#[utoipa::path(
    patch,
    path = "/v1/questionnaires/{id}",
    operation_id = "rename_questionnaire",
    tag = "questionnaires",
    params(("id" = i32, Path, description = "Questionnaire id")),
    request_body = NameInput,
    responses(
        (status = 204, description = "Renamed"),
        (status = 400, description = "Empty or too long name", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "Not one of the caller's questionnaires", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn rename(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<NameInput>,
) -> ApiResult<StatusCode> {
    let name = checked(&input.name, MAX_NAME, "Name")?;
    require_own(&state, &user, id).await?;
    sqlx::query("UPDATE sales_questionnaires SET name = ? WHERE id = ?")
        .bind(name)
        .bind(id)
        .execute(&state.pool)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Delete a questionnaire
///
/// Soft-deletes one of the caller's questionnaires with its questions, and
/// unsets it as the caller's default.
#[utoipa::path(
    delete,
    path = "/v1/questionnaires/{id}",
    operation_id = "delete_questionnaire",
    tag = "questionnaires",
    params(("id" = i32, Path, description = "Questionnaire id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "Not one of the caller's questionnaires", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    require_own(&state, &user, id).await?;
    let mut tx = state.pool.begin().await?;
    sqlx::query("UPDATE sales_questionnaires SET deleted_at = NOW() WHERE id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE sales_questionnaire_questions SET deleted_at = NOW()
         WHERE questionnaire_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE users SET default_questioner_id = NULL WHERE id = ? AND default_questioner_id = ?",
    )
    .bind(user.id)
    .bind(id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Add a question to a questionnaire
///
/// Appends a question to one of the caller's own questionnaires.
#[utoipa::path(
    post,
    path = "/v1/questionnaires/{id}/questions",
    operation_id = "add_questionnaire_question",
    tag = "questionnaires",
    params(("id" = i32, Path, description = "Questionnaire id")),
    request_body = QuestionInput,
    responses(
        (status = 201, description = "Added", body = CreatedId),
        (status = 400, description = "Empty or too long question", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "Not one of the caller's questionnaires", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn add_question(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<QuestionInput>,
) -> ApiResult<(StatusCode, Json<CreatedId>)> {
    let question = checked(&input.question, MAX_QUESTION, "Question")?;
    require_own(&state, &user, id).await?;
    let new_id = sqlx::query(
        "INSERT INTO sales_questionnaire_questions (questionnaire_id, question) VALUES (?, ?)",
    )
    .bind(id)
    .bind(question)
    .execute(&state.pool)
    .await?
    .last_insert_id();
    Ok((
        StatusCode::CREATED,
        Json(CreatedId {
            id: i64::try_from(new_id).unwrap_or_default(),
        }),
    ))
}

async fn require_question(state: &AppState, id: i32, question_id: i32) -> ApiResult<()> {
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT id FROM sales_questionnaire_questions
         WHERE id = ? AND questionnaire_id = ? AND deleted_at IS NULL",
    )
    .bind(question_id)
    .bind(id)
    .fetch_optional(&state.pool)
    .await?;
    found.map(|_| ()).ok_or(ApiError::NotFound)
}

/// Edit a question
///
/// Rewrites a live question of one of the caller's own questionnaires;
/// answers already given stay attached to it.
#[utoipa::path(
    patch,
    path = "/v1/questionnaires/{id}/questions/{question_id}",
    operation_id = "update_questionnaire_question",
    tag = "questionnaires",
    params(
        ("id" = i32, Path, description = "Questionnaire id"),
        ("question_id" = i32, Path, description = "Question id"),
    ),
    request_body = QuestionInput,
    responses(
        (status = 204, description = "Updated"),
        (status = 400, description = "Empty or too long question", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such live question in one of the caller's questionnaires", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update_question(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((id, question_id)): Path<(i32, i32)>,
    JsonBody(input): JsonBody<QuestionInput>,
) -> ApiResult<StatusCode> {
    let question = checked(&input.question, MAX_QUESTION, "Question")?;
    require_own(&state, &user, id).await?;
    require_question(&state, id, question_id).await?;
    sqlx::query("UPDATE sales_questionnaire_questions SET question = ? WHERE id = ?")
        .bind(question)
        .bind(question_id)
        .execute(&state.pool)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Delete a question
///
/// Soft-deletes a question of one of the caller's own questionnaires.
#[utoipa::path(
    delete,
    path = "/v1/questionnaires/{id}/questions/{question_id}",
    operation_id = "delete_questionnaire_question",
    tag = "questionnaires",
    params(
        ("id" = i32, Path, description = "Questionnaire id"),
        ("question_id" = i32, Path, description = "Question id"),
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such live question in one of the caller's questionnaires", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_question(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((id, question_id)): Path<(i32, i32)>,
) -> ApiResult<StatusCode> {
    require_own(&state, &user, id).await?;
    require_question(&state, id, question_id).await?;
    sqlx::query("UPDATE sales_questionnaire_questions SET deleted_at = NOW() WHERE id = ?")
        .bind(question_id)
        .execute(&state.pool)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(sqlx::FromRow)]
struct UsableQuestion {
    questionnaire_id: i32,
    questionnaire_name: String,
    question_id: i32,
    question: String,
    answer: Option<String>,
}

/// Questions of the questionnaires the caller can use (the company's shared
/// ones and their own), with the deal's answers, by questionnaire then question.
async fn usable_questions(
    state: &AppState,
    user: &SessionUser,
    deal_id: i64,
) -> Result<Vec<UsableQuestion>, sqlx::Error> {
    sqlx::query_as(
        "SELECT q.id AS questionnaire_id, q.name AS questionnaire_name,
                qq.id AS question_id, qq.question, r.answer
         FROM sales_questionnaires q
         JOIN sales_questionnaire_questions qq ON qq.questionnaire_id = q.id AND qq.deleted_at IS NULL
         LEFT JOIN sales_questionnaire_responses r ON r.question_id = qq.id AND r.deal_id = ?
         WHERE q.company_id = ? AND (q.user_id IS NULL OR q.user_id = ?) AND q.deleted_at IS NULL
         ORDER BY q.id, qq.id",
    )
    .bind(deal_id)
    .bind(user.company_id)
    .bind(user.id)
    .fetch_all(&state.pool)
    .await
}

async fn require_live_deal(state: &AppState, user: &SessionUser, deal_id: i64) -> ApiResult<()> {
    if deal_in_company(&state.pool, user.company_id, deal_id).await? {
        Ok(())
    } else {
        Err(ApiError::NotFound)
    }
}

/// List the questionnaires of a deal with its answers
///
/// The company's shared questionnaires and the caller's own (those with
/// questions), by id, each question with the deal's answer (empty when not
/// answered).
#[utoipa::path(
    get,
    path = "/v1/deals/{id}/questionnaires",
    operation_id = "list_deal_questionnaires",
    tag = "questionnaires",
    params(("id" = i64, Path, description = "Deal id")),
    responses(
        (status = 200, description = "Questionnaires with answers", body = Vec<DealQuestionnaire>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such live deal in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn deal_questionnaires(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<Vec<DealQuestionnaire>>> {
    require_live_deal(&state, &user, id).await?;
    let mut list: Vec<DealQuestionnaire> = Vec::new();
    for row in usable_questions(&state, &user, id).await? {
        if list.last().is_none_or(|q| q.id != row.questionnaire_id) {
            list.push(DealQuestionnaire {
                id: row.questionnaire_id,
                name: row.questionnaire_name,
                questions: Vec::new(),
            });
        }
        if let Some(last) = list.last_mut() {
            last.questions.push(AnsweredQuestion {
                id: row.question_id,
                question: row.question,
                answer: row.answer.unwrap_or_default(),
            });
        }
    }
    Ok(Json(list))
}

/// The answered questions as the note `create_note` adds: per questionnaire
/// a heading, then numbered questions with the answer in bold.
fn answers_note(questions: &[UsableQuestion], answers: &HashMap<i32, String>) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut current: Option<i32> = None;
    let mut number = 0;
    for question in questions {
        let Some(answer) = answers.get(&question.question_id).filter(|a| !a.is_empty()) else {
            continue;
        };
        if current != Some(question.questionnaire_id) {
            current = Some(question.questionnaire_id);
            number = 0;
            if lines.last().is_some_and(|l| !l.is_empty()) {
                lines.push(String::new());
            }
            lines.push(format!("Questionnaire: {}", question.questionnaire_name));
            lines.push(String::new());
        }
        number += 1;
        lines.push(format!("{number}. {}", question.question));
        lines.push(format!("- **{answer}**"));
        lines.push(String::new());
    }
    lines.join("\n").trim_end().to_string()
}

/// Save a deal's questionnaire answers
///
/// Upserts each answer (questions of questionnaires the caller cannot use
/// are ignored). With `create_note`, the non-empty answers are also added to
/// the deal as a note (which notifies the deal's people); that needs at
/// least one answer and fits 5000 characters.
#[utoipa::path(
    put,
    path = "/v1/deals/{id}/questionnaire-answers",
    operation_id = "save_deal_questionnaire_answers",
    tag = "questionnaires",
    params(("id" = i64, Path, description = "Deal id")),
    request_body = AnswersInput,
    responses(
        (status = 200, description = "Saved", body = AnswersSaved),
        (status = 400, description = "Answer too long, or a note without answers or over 5000 characters", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such live deal in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn save_answers(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i64>,
    JsonBody(input): JsonBody<AnswersInput>,
) -> ApiResult<Json<AnswersSaved>> {
    if input
        .answers
        .iter()
        .any(|a| a.answer.chars().count() > MAX_ANSWER || a.question_id <= 0)
    {
        return Err(ApiError::BadRequest("Invalid answers".into()));
    }
    require_live_deal(&state, &user, id).await?;
    let questions = usable_questions(&state, &user, id).await?;
    let allowed: std::collections::HashSet<i32> = questions.iter().map(|q| q.question_id).collect();
    let answers: Vec<(i32, String)> = input
        .answers
        .into_iter()
        .filter(|a| allowed.contains(&a.question_id))
        .map(|a| (a.question_id, a.answer))
        .collect();

    let note = if input.create_note {
        let trimmed: HashMap<i32, String> = answers
            .iter()
            .map(|(q, a)| (*q, a.trim().to_string()))
            .collect();
        let note = answers_note(&questions, &trimmed);
        if note.is_empty() {
            return Err(ApiError::BadRequest(
                "Enter at least one answer before creating a note.".into(),
            ));
        }
        if note.chars().count() > MAX_NOTE {
            return Err(ApiError::BadRequest(
                "The questionnaire note must be 5000 characters or less.".into(),
            ));
        }
        Some(note)
    } else {
        None
    };

    let mut tx = state.pool.begin().await?;
    for (question_id, answer) in &answers {
        sqlx::query(
            "INSERT INTO sales_questionnaire_responses (deal_id, question_id, answer, answered_by)
             VALUES (?, ?, ?, ?)
             ON DUPLICATE KEY UPDATE answer = VALUES(answer), answered_by = VALUES(answered_by),
               updated_at = CURRENT_TIMESTAMP",
        )
        .bind(id)
        .bind(question_id)
        .bind(answer)
        .bind(user.id)
        .execute(&mut *tx)
        .await?;
    }
    if let Some(note) = &note {
        sqlx::query(
            "INSERT INTO deal_notes (deal_id, company_id, content, created_by) VALUES (?, ?, ?, ?)",
        )
        .bind(id)
        .bind(user.company_id)
        .bind(note)
        .bind(&user.name)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    if let Some(note) = &note {
        let actor = user.name.clone().unwrap_or_default();
        notify_assignees(&state, id, user.id, &actor, note, Notification::NoteAdded).await;
    }
    Ok(Json(AnswersSaved {
        note_created: note.is_some(),
    }))
}

/// Get the caller's default questionnaire
///
/// The questionnaire the deal Questionnaire tab opens first; null when unset.
#[utoipa::path(
    get,
    path = "/v1/me/default-questionnaire",
    operation_id = "get_default_questionnaire",
    tag = "questionnaires",
    responses(
        (status = 200, description = "The default", body = DefaultQuestionnaire),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get_default(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<DefaultQuestionnaire>> {
    let questionnaire_id: Option<Option<i32>> =
        sqlx::query_scalar("SELECT default_questioner_id FROM users WHERE id = ?")
            .bind(user.id)
            .fetch_optional(&state.pool)
            .await?;
    Ok(Json(DefaultQuestionnaire {
        questionnaire_id: questionnaire_id.flatten(),
    }))
}

/// Set the caller's default questionnaire
///
/// Any live questionnaire the caller can use (shared or their own).
#[utoipa::path(
    put,
    path = "/v1/me/default-questionnaire",
    operation_id = "set_default_questionnaire",
    tag = "questionnaires",
    request_body = DefaultQuestionnaire,
    responses(
        (status = 200, description = "Saved", body = DefaultQuestionnaire),
        (status = 400, description = "Missing questionnaire id", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such questionnaire the caller can use", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn set_default(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<DefaultQuestionnaire>,
) -> ApiResult<Json<DefaultQuestionnaire>> {
    let id = input
        .questionnaire_id
        .ok_or_else(|| ApiError::BadRequest("Invalid questionnaire".into()))?;
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT id FROM sales_questionnaires
         WHERE id = ? AND company_id = ? AND (user_id IS NULL OR user_id = ?) AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(user.company_id)
    .bind(user.id)
    .fetch_optional(&state.pool)
    .await?;
    found.ok_or(ApiError::NotFound)?;
    sqlx::query("UPDATE users SET default_questioner_id = ? WHERE id = ?")
        .bind(id)
        .bind(user.id)
        .execute(&state.pool)
        .await?;
    Ok(Json(DefaultQuestionnaire {
        questionnaire_id: Some(id),
    }))
}
