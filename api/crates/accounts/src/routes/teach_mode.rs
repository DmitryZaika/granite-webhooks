//! Teach Mode: multiple-choice tests on the company's instructions.
//!
//! Employees:
//! - `GET  /v1/teach-mode`          → visible questions, their answers, instructions
//! - `POST /v1/teach-mode/attempts` → grade and save a finished test
//!
//! Admins:
//! - `GET    /v1/teach-mode/questions`                    → every question
//! - `POST   /v1/teach-mode/questions`                    → add one
//! - `PUT    /v1/teach-mode/questions/{id}`               → edit one and its answers
//! - `DELETE /v1/teach-mode/questions/{id}`               → delete one
//! - `PUT    /v1/teach-mode/questions/{id}/visibility`    → publish or hide one
//! - `POST   /v1/teach-mode/instructions/{id}/publish`    → publish an instruction's drafts
//! - `POST   /v1/teach-mode/instructions/{id}/generate`   → AI-generate questions
//!
//! Replaces the Remix `employee.teach-mode` and `admin.teach-mode` loaders
//! and actions. Tests are graded here (the page used to send its own
//! `is_correct`).

use crate::ai::{Caller, Chat, MINI_MODEL};
use crate::auth::{AdminUser, EmployeeUser};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::openapi::{OpenApiRouter, routes};
use crate::routes::created::CreatedId;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::FromRow;
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::time::Duration;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(employee_test))
        .routes(routes!(submit_attempt))
        .routes(routes!(list_questions, create_question))
        .routes(routes!(update_question, delete_question))
        .routes(routes!(set_visibility))
        .routes(routes!(publish_instruction))
        .routes(routes!(generate_questions))
}

/// Instructions Teach Mode never tests: the onboarding samples and the AI
/// follow-up settings.
const HIDDEN_TITLES: [&str; 3] = [
    "Onboarding Sample — Getting Started",
    "How to look up a procedure",
    "AI follow-up instruction",
];

/// Most questions one generation adds.
const MAX_GENERATED: usize = 30;

/// A test question.
#[derive(Debug, Serialize, FromRow, ToSchema)]
pub struct TestQuestion {
    /// The question.
    pub id: i32,
    /// Its text.
    pub text: String,
    /// The instruction it tests.
    pub instruction_id: i32,
    /// `MC` or `TF`.
    pub question_type: String,
}

/// An answer to pick.
#[derive(Debug, Serialize, FromRow, ToSchema)]
pub struct AnswerChoice {
    /// The answer.
    pub id: i32,
    /// Its question.
    pub question_id: i32,
    /// Its text.
    pub text: String,
    /// The right answer.
    pub is_correct: bool,
}

/// An instruction with a test.
#[derive(Debug, Serialize, ToSchema)]
pub struct TestInstruction {
    /// The instruction.
    pub id: i32,
    /// Its title ("Untitled instruction" when blank).
    pub title: String,
    /// The caller once answered every one of its questions right in one try.
    pub completed: bool,
}

/// `GET /v1/teach-mode`.
#[derive(Debug, Serialize, ToSchema)]
pub struct EmployeeTest {
    /// Visible questions, newest first.
    pub questions: Vec<TestQuestion>,
    /// Their answers, by question.
    pub answer_choices: Vec<AnswerChoice>,
    /// Instructions with visible questions, by title.
    pub instructions: Vec<TestInstruction>,
}

#[derive(FromRow)]
struct AttemptRow {
    question_id: i32,
    instruction_id: i32,
    attempt_number: i32,
    is_correct: bool,
}

/// Instructions whose every question was answered right in one try.
fn completed_instructions(questions: &[TestQuestion], attempts: &[AttemptRow]) -> HashSet<i32> {
    let mut by_instruction: HashMap<i32, Vec<i32>> = HashMap::new();
    for question in questions {
        by_instruction
            .entry(question.instruction_id)
            .or_default()
            .push(question.id);
    }
    // (instruction, try) → (questions answered, all right)
    let mut tries: HashMap<(i32, i32), (HashSet<i32>, bool)> = HashMap::new();
    for attempt in attempts {
        let entry = tries
            .entry((attempt.instruction_id, attempt.attempt_number))
            .or_insert_with(|| (HashSet::new(), true));
        entry.0.insert(attempt.question_id);
        entry.1 &= attempt.is_correct;
    }
    by_instruction
        .into_iter()
        .filter(|(instruction, ids)| {
            tries.iter().any(|((id, _), (answered, all_right))| {
                id == instruction && *all_right && ids.iter().all(|q| answered.contains(q))
            })
        })
        .map(|(instruction, _)| instruction)
        .collect()
}

/// The employee test
///
/// The company's questions visible to employees with their answers, and the
/// instructions they test, each marked completed when the caller once
/// answered all its questions right in one try.
#[crate::openapi::path(
    get,
    path = "/v1/teach-mode",
    operation_id = "get_teach_mode",
    tag = "company",
    responses(
        (status = 200, description = "The test", body = EmployeeTest),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn employee_test(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<EmployeeTest>> {
    let pool = &state.pool;
    let visible = "q.company_id = ? AND q.is_visible_to_employees = 1
                   AND q.deleted_at IS NULL AND q.instruction_id IS NOT NULL";
    let questions: Vec<TestQuestion> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT q.id, q.text, q.instruction_id, CAST(q.question_type AS CHAR) AS question_type
           FROM questions q WHERE {visible}
          ORDER BY q.created_date DESC, q.id DESC"
    )))
    .bind(user.company_id)
    .fetch_all(pool)
    .await?;
    let answer_choices: Vec<AnswerChoice> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT ac.id, ac.question_id, ac.text, ac.is_correct = 1 AS is_correct
           FROM answer_choices ac JOIN questions q ON q.id = ac.question_id
          WHERE {visible} AND ac.deleted_at IS NULL
          ORDER BY ac.question_id, ac.id"
    )))
    .bind(user.company_id)
    .fetch_all(pool)
    .await?;
    let instructions: Vec<(i32, Option<String>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT DISTINCT i.id, i.title FROM instructions i
           JOIN questions q ON q.instruction_id = i.id
          WHERE {visible}
          ORDER BY i.title, i.id"
    )))
    .bind(user.company_id)
    .fetch_all(pool)
    .await?;
    let attempts: Vec<AttemptRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT aa.question_id, q.instruction_id, aa.attempt_number,
                COALESCE(aa.is_correct, 0) = 1 AS is_correct
           FROM answer_attempts aa JOIN questions q ON q.id = aa.question_id
          WHERE aa.employee_id = ? AND aa.deleted_at IS NULL AND {visible}"
    )))
    .bind(user.id)
    .bind(user.company_id)
    .fetch_all(pool)
    .await?;
    let completed = completed_instructions(&questions, &attempts);
    let instructions = instructions
        .into_iter()
        .map(|(id, title)| TestInstruction {
            id,
            title: title
                .map(|title| title.trim().to_string())
                .filter(|title| !title.is_empty())
                .unwrap_or_else(|| "Untitled instruction".into()),
            completed: completed.contains(&id),
        })
        .collect();
    Ok(Json(EmployeeTest {
        questions,
        answer_choices,
        instructions,
    }))
}

/// One answer of a finished test.
#[derive(Debug, Deserialize, ToSchema)]
pub struct AttemptAnswer {
    /// The question.
    pub question_id: i32,
    /// The answer picked; null when skipped.
    pub selected_answer_id: Option<i32>,
}

/// Body of `POST /v1/teach-mode/attempts`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct AttemptInput {
    /// Every question of the test, answered or not.
    pub answers: Vec<AttemptAnswer>,
}

/// The grade of a test.
#[derive(Debug, Serialize, ToSchema)]
pub struct AttemptResult {
    /// Right answers, in percent (rounded).
    pub score_percent: i32,
    /// Every answer was right.
    pub passed: bool,
}

/// Submit a test
///
/// Grades the caller's answers (a skipped question is wrong) and saves them as
/// their next try at those questions.
#[crate::openapi::path(
    post,
    path = "/v1/teach-mode/attempts",
    operation_id = "submit_teach_mode_attempt",
    tag = "company",
    request_body = AttemptInput,
    responses(
        (status = 200, description = "Graded and saved", body = AttemptResult),
        (status = 400, description = "No answers, or a question that is not the company's visible one", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn submit_attempt(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<AttemptInput>,
) -> ApiResult<Json<AttemptResult>> {
    if input.answers.is_empty() {
        return Err(ApiError::BadRequest(
            "Failed to save attempts: missing answers".into(),
        ));
    }
    let ids: Vec<i32> = input.answers.iter().map(|a| a.question_id).collect();
    let placeholders = vec!["?"; ids.len()].join(",");
    let mut known = sqlx::query_scalar::<_, i32>(sqlx::AssertSqlSafe(format!(
        "SELECT id FROM questions
          WHERE company_id = ? AND deleted_at IS NULL AND is_visible_to_employees = 1
            AND id IN ({placeholders})"
    )))
    .bind(user.company_id);
    for id in &ids {
        known = known.bind(id);
    }
    let known: HashSet<i32> = known.fetch_all(&state.pool).await?.into_iter().collect();
    if ids.iter().any(|id| !known.contains(id)) {
        return Err(ApiError::BadRequest("Unknown question".into()));
    }
    let mut right = sqlx::query_as::<_, (i32, i32)>(sqlx::AssertSqlSafe(format!(
        "SELECT question_id, id FROM answer_choices
          WHERE is_correct = 1 AND deleted_at IS NULL AND question_id IN ({placeholders})"
    )));
    for id in &ids {
        right = right.bind(id);
    }
    let right: HashSet<(i32, i32)> = right.fetch_all(&state.pool).await?.into_iter().collect();

    let mut tx = state.pool.begin().await?;
    let mut last = sqlx::query_scalar::<_, Option<i32>>(sqlx::AssertSqlSafe(format!(
        "SELECT MAX(attempt_number) FROM answer_attempts
          WHERE employee_id = ? AND deleted_at IS NULL AND question_id IN ({placeholders})
          FOR UPDATE"
    )))
    .bind(user.id);
    for id in &ids {
        last = last.bind(id);
    }
    let attempt_number = last.fetch_one(&mut *tx).await?.unwrap_or(0) + 1;
    let mut correct = 0;
    for answer in &input.answers {
        let is_correct = answer
            .selected_answer_id
            .is_some_and(|choice| right.contains(&(answer.question_id, choice)));
        if is_correct {
            correct += 1;
        }
        sqlx::query(
            "INSERT INTO answer_attempts
               (employee_id, question_id, selected_answer_id, attempt_number, is_correct)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(user.id)
        .bind(answer.question_id)
        .bind(answer.selected_answer_id)
        .bind(attempt_number)
        .bind(is_correct)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    let total = i32::try_from(input.answers.len()).unwrap_or(i32::MAX);
    let score_percent = (f64::from(correct) * 100.0 / f64::from(total)).round();
    #[allow(clippy::cast_possible_truncation)]
    let score_percent = score_percent as i32;
    Ok(Json(AttemptResult {
        score_percent,
        passed: correct == total,
    }))
}

/// A question, for the admin page.
#[derive(Debug, Serialize, FromRow, ToSchema)]
pub struct AdminQuestion {
    /// The question.
    pub id: i32,
    /// Its text.
    pub text: String,
    /// The instruction it tests.
    pub instruction_id: Option<i32>,
    /// `MC` or `TF`.
    pub question_type: String,
    /// Published to employees (else a draft).
    pub is_visible_to_employees: bool,
    /// When it was added (UTC, ISO 8601).
    pub created_date: Option<String>,
}

/// `GET /v1/teach-mode/questions`.
#[derive(Debug, Serialize, ToSchema)]
pub struct AdminQuestions {
    /// Not deleted questions, newest first.
    pub questions: Vec<AdminQuestion>,
    /// Their answers, by question.
    pub answer_choices: Vec<AnswerChoice>,
}

/// List the questions
///
/// Every question of the company, published or draft, with its answers.
#[crate::openapi::path(
    get,
    path = "/v1/teach-mode/questions",
    operation_id = "list_teach_mode_questions",
    tag = "company",
    responses(
        (status = 200, description = "Questions", body = AdminQuestions),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list_questions(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
) -> ApiResult<Json<AdminQuestions>> {
    let questions: Vec<AdminQuestion> = sqlx::query_as(
        "SELECT id, text, instruction_id, CAST(question_type AS CHAR) AS question_type,
                COALESCE(is_visible_to_employees, 0) = 1 AS is_visible_to_employees,
                DATE_FORMAT(created_date, '%Y-%m-%dT%H:%i:%sZ') AS created_date
           FROM questions
          WHERE company_id = ? AND deleted_at IS NULL
          ORDER BY created_date DESC, id DESC",
    )
    .bind(user.company_id)
    .fetch_all(&state.pool)
    .await?;
    let answer_choices: Vec<AnswerChoice> = sqlx::query_as(
        "SELECT ac.id, ac.question_id, ac.text, ac.is_correct = 1 AS is_correct
           FROM answer_choices ac JOIN questions q ON q.id = ac.question_id
          WHERE q.company_id = ? AND q.deleted_at IS NULL AND ac.deleted_at IS NULL
          ORDER BY ac.question_id, ac.id",
    )
    .bind(user.company_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(AdminQuestions {
        questions,
        answer_choices,
    }))
}

/// An answer of a question being saved.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ChoiceInput {
    /// An existing answer to keep; omitted for a new one.
    #[serde(default)]
    pub id: Option<i32>,
    /// Its text; a blank answer is dropped.
    pub text: String,
    /// The right answer.
    pub is_correct: bool,
}

/// Body of `POST /v1/teach-mode/questions`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct NewQuestion {
    /// The instruction it tests.
    pub instruction_id: i32,
    /// The question.
    pub text: String,
    /// At least two answers, exactly one right.
    pub choices: Vec<ChoiceInput>,
}

/// Body of `PUT /v1/teach-mode/questions/{id}`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct QuestionUpdate {
    /// The question.
    pub text: String,
    /// The answers it keeps (others are deleted): at least two, exactly one right.
    pub choices: Vec<ChoiceInput>,
}

/// The question's text and non-blank answers, or why they cannot be saved.
fn checked(text: &str, choices: Vec<ChoiceInput>) -> ApiResult<(String, Vec<ChoiceInput>)> {
    let text = text.trim();
    let choices: Vec<ChoiceInput> = choices
        .into_iter()
        .filter(|choice| !choice.text.trim().is_empty())
        .collect();
    if text.is_empty() {
        return Err(ApiError::BadRequest(
            "Question and answer choices are required".into(),
        ));
    }
    if choices.len() < 2 {
        return Err(ApiError::BadRequest(
            "At least two answers are required".into(),
        ));
    }
    if choices.iter().filter(|choice| choice.is_correct).count() != 1 {
        return Err(ApiError::BadRequest(
            "Choose exactly one correct answer".into(),
        ));
    }
    Ok((text.to_string(), choices))
}

/// A company instruction Teach Mode may test: its title and rich text.
async fn testable_instruction(
    state: &AppState,
    company_id: i32,
    id: i32,
) -> ApiResult<(String, String)> {
    let row: Option<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT title, rich_text FROM instructions
          WHERE id = ? AND company_id = ? AND COALESCE(title, '') NOT IN (?, ?, ?)",
    )
    .bind(id)
    .bind(company_id)
    .bind(HIDDEN_TITLES[0])
    .bind(HIDDEN_TITLES[1])
    .bind(HIDDEN_TITLES[2])
    .fetch_optional(&state.pool)
    .await?;
    let (title, text) = row.ok_or_else(|| ApiError::BadRequest("Instruction not found".into()))?;
    Ok((title.unwrap_or_default(), text.unwrap_or_default()))
}

/// Inserts a published multiple-choice question with its answers.
async fn insert_question(
    tx: &mut sqlx::MySqlConnection,
    company_id: i32,
    user_id: i32,
    instruction_id: i32,
    text: &str,
    choices: &[(String, bool)],
) -> ApiResult<i64> {
    let done = sqlx::query(
        "INSERT INTO questions
           (text, instruction_id, question_type, company_id, created_by_user_id, is_visible_to_employees)
         VALUES (?, ?, 'MC', ?, ?, 1)",
    )
    .bind(text)
    .bind(instruction_id)
    .bind(company_id)
    .bind(user_id)
    .execute(&mut *tx)
    .await?;
    let id = done.last_insert_id();
    for (choice, is_correct) in choices {
        sqlx::query("INSERT INTO answer_choices (question_id, text, is_correct) VALUES (?, ?, ?)")
            .bind(id)
            .bind(choice)
            .bind(is_correct)
            .execute(&mut *tx)
            .await?;
    }
    Ok(i64::try_from(id).unwrap_or(i64::MAX))
}

/// Add a question
///
/// A published multiple-choice question on one of the company's instructions.
#[crate::openapi::path(
    post,
    path = "/v1/teach-mode/questions",
    operation_id = "create_teach_mode_question",
    tag = "company",
    request_body = NewQuestion,
    responses(
        (status = 201, description = "Added", body = CreatedId),
        (status = 400, description = "Unknown instruction, or not two answers with one right", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create_question(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<NewQuestion>,
) -> ApiResult<(StatusCode, Json<CreatedId>)> {
    testable_instruction(&state, user.company_id, input.instruction_id).await?;
    let (text, choices) = checked(&input.text, input.choices)?;
    let choices: Vec<(String, bool)> = choices
        .into_iter()
        .map(|choice| (choice.text.trim().to_string(), choice.is_correct))
        .collect();
    let mut tx = state.pool.begin().await?;
    let id = insert_question(
        &mut tx,
        user.company_id,
        user.id,
        input.instruction_id,
        &text,
        &choices,
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(CreatedId { id })))
}

/// Edit a question
///
/// Saves the text and the answers: listed answers with an id are updated,
/// those without are added, and the question's other answers are deleted.
#[crate::openapi::path(
    put,
    path = "/v1/teach-mode/questions/{id}",
    operation_id = "update_teach_mode_question",
    tag = "company",
    params(("id" = i32, Path, description = "Question id")),
    request_body = QuestionUpdate,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Not two answers with one right", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such question in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update_question(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<QuestionUpdate>,
) -> ApiResult<StatusCode> {
    let (text, choices) = checked(&input.text, input.choices)?;
    let mut tx = state.pool.begin().await?;
    let done = sqlx::query(
        "UPDATE questions SET text = ?, updated_date = CURRENT_TIMESTAMP
          WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(&text)
    .bind(id)
    .bind(user.company_id)
    .execute(&mut *tx)
    .await?;
    if done.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }
    let kept: HashSet<i32> = choices.iter().filter_map(|choice| choice.id).collect();
    let existing: Vec<i32> = sqlx::query_scalar(
        "SELECT id FROM answer_choices WHERE question_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .fetch_all(&mut *tx)
    .await?;
    for old in existing.iter().filter(|old| !kept.contains(old)) {
        sqlx::query(
            "UPDATE answer_choices SET deleted_at = CURRENT_TIMESTAMP, updated_date = CURRENT_TIMESTAMP
              WHERE id = ? AND question_id = ?",
        )
        .bind(old)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    for choice in &choices {
        let text = choice.text.trim();
        match choice.id {
            Some(choice_id) if existing.contains(&choice_id) => {
                sqlx::query(
                    "UPDATE answer_choices SET text = ?, is_correct = ?, updated_date = CURRENT_TIMESTAMP
                      WHERE id = ? AND question_id = ?",
                )
                .bind(text)
                .bind(choice.is_correct)
                .bind(choice_id)
                .bind(id)
                .execute(&mut *tx)
                .await?;
            }
            _ => {
                sqlx::query(
                    "INSERT INTO answer_choices (question_id, text, is_correct) VALUES (?, ?, ?)",
                )
                .bind(id)
                .bind(text)
                .bind(choice.is_correct)
                .execute(&mut *tx)
                .await?;
            }
        }
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Delete a question
///
/// Soft-deletes one of the company's questions; past answers stay.
#[crate::openapi::path(
    delete,
    path = "/v1/teach-mode/questions/{id}",
    operation_id = "delete_teach_mode_question",
    tag = "company",
    params(("id" = i32, Path, description = "Question id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such question in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_question(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    let done = sqlx::query(
        "UPDATE questions SET deleted_at = CURRENT_TIMESTAMP, updated_date = CURRENT_TIMESTAMP
          WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(user.company_id)
    .execute(&state.pool)
    .await?;
    if done.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Body of `PUT /v1/teach-mode/questions/{id}/visibility`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct Visibility {
    /// Published to employees (false: a draft).
    pub visible: bool,
}

/// Publish or hide a question
///
/// Shows the question to employees, or moves it back to the drafts.
#[crate::openapi::path(
    put,
    path = "/v1/teach-mode/questions/{id}/visibility",
    operation_id = "set_teach_mode_question_visibility",
    tag = "company",
    params(("id" = i32, Path, description = "Question id")),
    request_body = Visibility,
    responses(
        (status = 204, description = "Saved"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such question in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn set_visibility(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<Visibility>,
) -> ApiResult<StatusCode> {
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT id FROM questions WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(user.company_id)
    .fetch_optional(&state.pool)
    .await?;
    if found.is_none() {
        return Err(ApiError::NotFound);
    }
    sqlx::query(
        "UPDATE questions SET is_visible_to_employees = ?, updated_date = CURRENT_TIMESTAMP
          WHERE id = ?",
    )
    .bind(input.visible)
    .bind(id)
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// How many questions a call published or generated.
#[derive(Debug, Serialize, ToSchema)]
pub struct QuestionCount {
    /// Questions.
    pub count: u64,
}

/// Publish an instruction's drafts
///
/// Shows every draft question of the instruction to employees.
#[crate::openapi::path(
    post,
    path = "/v1/teach-mode/instructions/{id}/publish",
    operation_id = "publish_teach_mode_instruction",
    tag = "company",
    params(("id" = i32, Path, description = "Instruction id")),
    responses(
        (status = 200, description = "Drafts published", body = QuestionCount),
        (status = 400, description = "Instruction not found", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn publish_instruction(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<QuestionCount>> {
    testable_instruction(&state, user.company_id, id).await?;
    let done = sqlx::query(
        "UPDATE questions SET is_visible_to_employees = 1, updated_date = CURRENT_TIMESTAMP
          WHERE instruction_id = ? AND company_id = ? AND deleted_at IS NULL
            AND COALESCE(is_visible_to_employees, 0) = 0",
    )
    .bind(id)
    .bind(user.company_id)
    .execute(&state.pool)
    .await?;
    Ok(Json(QuestionCount {
        count: done.rows_affected(),
    }))
}

/// Plain text of an instruction's HTML.
pub fn text_from_html(html: &str) -> String {
    let mut text = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut skip_until: Option<&str> = None;
    let lower = html.to_ascii_lowercase();
    let mut index = 0;
    for (at, c) in html.char_indices() {
        if at < index {
            continue;
        }
        if let Some(end) = skip_until {
            if lower[at..].starts_with(end) {
                index = at + end.len();
                skip_until = None;
            }
            continue;
        }
        if c == '<' {
            for tag in ["script", "style"] {
                if lower[at + 1..].starts_with(tag) {
                    skip_until = Some(if tag == "script" {
                        "</script>"
                    } else {
                        "</style>"
                    });
                }
            }
            in_tag = true;
            text.push(' ');
        } else if c == '>' && in_tag {
            in_tag = false;
        } else if !in_tag {
            text.push(c);
        }
    }
    let text = text
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The generation prompt.
fn generation_prompt(title: &str, content: &str, existing: &[String]) -> String {
    let existing = if existing.is_empty() {
        "None".to_string()
    } else {
        existing.iter().fold(String::new(), |mut out, question| {
            if !out.is_empty() {
                out.push('\n');
            }
            let _ = write!(out, "- {question}");
            out
        })
    };
    format!(
        "Create a comprehensive multiple-choice assessment for one employee instruction.

Instruction title:
{title}

Instruction content:
{content}

Existing questions for this instruction:
{existing}

Return one JSON object with a \"questions\" array. Each item must contain:
- \"question\": a clear question answerable only from the instruction
- \"options\": exactly four distinct, realistic answer choices
- \"correctOptionIndex\": the zero-based index of the only correct option

Coverage requirements:
- Identify every distinct fact, requirement, procedure step, threshold, exception, warning, responsibility, and decision rule in the instruction.
- Generate enough questions to cover all meaningful information. Use fewer questions for short instructions and more for detailed instructions.
- Return at most {MAX_GENERATED} questions. If the instruction has more facts than that, prioritize the most important ones.
- Do not omit critical information merely to keep the assessment short.
- Avoid duplicate questions and avoid repeating the existing questions unless needed to cover a missing detail.
- Test understanding rather than trivia about wording.
- Keep every answer fully supported by the supplied instruction.
- Do not invent facts.
- Return JSON only."
    )
}

/// A generated question: text, four answers, the right one's index.
#[derive(Debug, PartialEq, Eq)]
struct Generated {
    question: String,
    options: Vec<String>,
    correct: usize,
}

fn string_field(item: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| item.get(*key).and_then(Value::as_str))
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

fn options_field(item: &Value) -> Option<Vec<String>> {
    ["options", "choices"].iter().find_map(|key| {
        let options: Vec<String> = item
            .get(*key)?
            .as_array()?
            .iter()
            .filter_map(Value::as_str)
            .map(|option| option.trim().to_string())
            .filter(|option| !option.is_empty())
            .collect();
        (options.len() == 4).then_some(options)
    })
}

fn index_field(item: &Value) -> Option<usize> {
    [
        "correctOptionIndex",
        "correct_option_index",
        "correctIndex",
        "answerIndex",
    ]
    .iter()
    .find_map(|key| match item.get(*key)? {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    })
    .and_then(|index| usize::try_from(index).ok())
    .filter(|index| *index < 4)
}

/// The usable questions of the AI's reply: unique, four distinct answers.
fn parse_generated(raw: &str) -> Result<Vec<Generated>, String> {
    let value: Value = serde_json::from_str(raw)
        .map_err(|_| "The AI returned invalid question data".to_string())?;
    let items = match &value {
        Value::Array(items) => items.clone(),
        Value::Object(_) => ["questions", "items"]
            .iter()
            .find_map(|key| value.get(*key)?.as_array().cloned())
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    if items.is_empty() {
        return Err("The AI returned incomplete question data".into());
    }
    let mut seen = HashSet::new();
    let mut questions = Vec::new();
    for item in &items {
        if questions.len() >= MAX_GENERATED {
            break;
        }
        let (Some(question), Some(options), Some(correct)) = (
            string_field(item, &["question", "text"]),
            options_field(item),
            index_field(item),
        ) else {
            continue;
        };
        let distinct: HashSet<String> = options.iter().map(|o| o.to_lowercase()).collect();
        if distinct.len() != options.len() || !seen.insert(question.to_lowercase()) {
            continue;
        }
        questions.push(Generated {
            question,
            options,
            correct,
        });
    }
    if questions.is_empty() {
        return Err("The AI did not generate usable questions".into());
    }
    Ok(questions)
}

/// Generate questions
///
/// Asks the AI for a multiple-choice test covering the instruction (up to 30
/// questions, avoiding the existing ones) and adds them published.
#[crate::openapi::path(
    post,
    path = "/v1/teach-mode/instructions/{id}/generate",
    operation_id = "generate_teach_mode_questions",
    tag = "company",
    params(("id" = i32, Path, description = "Instruction id")),
    responses(
        (status = 200, description = "Questions added", body = QuestionCount),
        (status = 400, description = "Instruction not found or empty", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
        (status = 502, description = "The AI failed or returned no usable questions", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn generate_questions(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<QuestionCount>> {
    let (title, html) = testable_instruction(&state, user.company_id, id).await?;
    let content = text_from_html(&html);
    if content.is_empty() {
        return Err(ApiError::BadRequest(
            "This instruction has no content to generate questions from".into(),
        ));
    }
    let existing: Vec<String> = sqlx::query_scalar(
        "SELECT text FROM questions WHERE instruction_id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(user.company_id)
    .fetch_all(&state.pool)
    .await?;
    let title = title.trim();
    let title = if title.is_empty() {
        "Untitled instruction"
    } else {
        title
    };
    let mut chat = Chat::new(MINI_MODEL)
        .max_tokens(Some(12_000))
        .message(
            "system",
            "You create accurate employee assessments and return only valid JSON. Coverage must be exhaustive for the supplied instruction.",
        )
        .message("user", &generation_prompt(title, &content, &existing));
    chat.json = true;
    chat.timeout = Duration::from_secs(120);
    let caller = Caller {
        feature: "teach_mode",
        user_id: Some(user.id),
        company_id: Some(user.company_id),
    };
    let reply = state
        .ai
        .chat(&state.pool, caller, &chat)
        .await
        .map_err(|err| ApiError::Upstream(err.to_string()))?;
    let generated = parse_generated(reply.trim()).map_err(ApiError::Upstream)?;
    let mut tx = state.pool.begin().await?;
    for question in &generated {
        let choices: Vec<(String, bool)> = question
            .options
            .iter()
            .enumerate()
            .map(|(index, option)| (option.clone(), index == question.correct))
            .collect();
        insert_question(
            &mut tx,
            user.company_id,
            user.id,
            id,
            &question.question,
            &choices,
        )
        .await?;
    }
    tx.commit().await?;
    Ok(Json(QuestionCount {
        count: generated.len() as u64,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_becomes_plain_text() {
        assert_eq!(
            text_from_html(
                "<p>Seal&nbsp;the <b>edge</b></p><script>x()</script><ul><li>Wait 24h</li></ul>"
            ),
            "Seal the edge Wait 24h"
        );
        assert_eq!(text_from_html("  <br/> "), "");
    }

    #[test]
    fn generated_questions_are_checked() {
        let raw = r#"{"questions":[
            {"question":"Q1","options":["a","b","c","d"],"correctOptionIndex":1},
            {"question":"q1","options":["a","b","c","d"],"correctOptionIndex":0},
            {"text":"Q2","choices":["a","b","c","d"],"answerIndex":"3"},
            {"question":"Q3","options":["a","a","c","d"],"correctOptionIndex":0},
            {"question":"Q4","options":["a","b","c"],"correctOptionIndex":0}
        ]}"#;
        let questions = parse_generated(raw).unwrap();
        assert_eq!(
            questions
                .iter()
                .map(|q| (q.question.as_str(), q.correct))
                .collect::<Vec<_>>(),
            [("Q1", 1), ("Q2", 3)]
        );
        assert!(parse_generated("nope").is_err());
        assert!(parse_generated(r#"{"questions":[]}"#).is_err());
    }

    #[test]
    fn the_prompt_lists_existing_questions() {
        let prompt = generation_prompt("Sealing", "Seal it", &["Why seal?".into()]);
        assert!(prompt.contains("Instruction title:\nSealing"));
        assert!(prompt.contains("- Why seal?"));
        assert!(prompt.contains("at most 30 questions"));
    }
}
