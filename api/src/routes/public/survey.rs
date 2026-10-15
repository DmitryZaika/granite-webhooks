//! The post-installation survey customers fill in from the link they get
//! after a checklist (no session).

use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::routes::jobs::survey_notifications::survey_token;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use utoipa::ToSchema;

/// Sales rep and installer positions (`positions.id`).
const SALES_REP: i32 = 1;
const INSTALLER: i32 = 6;

/// A person the customer can rate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow, ToSchema)]
pub struct SurveyPerson {
    /// User id.
    pub id: i32,
    /// Name.
    pub name: Option<String>,
}

/// What the survey form shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct SurveyForm {
    /// Company name; empty when the company does not exist.
    pub company_name: String,
    /// The company's sales reps, by name.
    pub sales_reps: Vec<SurveyPerson>,
    /// The company's installers, by name.
    pub installers: Vec<SurveyPerson>,
}

async fn people(
    state: &AppState,
    company_id: i32,
    position: i32,
) -> Result<Vec<SurveyPerson>, sqlx::Error> {
    sqlx::query_as(
        "SELECT DISTINCT u.id, u.name FROM users u
           JOIN users_positions up ON up.user_id = u.id
          WHERE up.company_id = ? AND up.position_id = ? AND u.is_deleted = 0
          ORDER BY u.name ASC, u.id ASC",
    )
    .bind(company_id)
    .bind(position)
    .fetch_all(&state.pool)
    .await
}

/// The survey form
///
/// The company's name, sales reps and installers, for the customer to pick
/// who they worked with. No session needed.
#[utoipa::path(
    get,
    path = "/v1/public/companies/{company_id}/survey",
    operation_id = "get_public_survey",
    tag = "public",
    params(("company_id" = i32, Path, description = "Company id")),
    responses(
        (status = 200, description = "The form", body = SurveyForm),
        (status = 500, description = "Database error", body = ErrorBody),
    )
)]
pub async fn survey_form(
    State(state): State<AppState>,
    Path(company_id): Path<i32>,
) -> ApiResult<Json<SurveyForm>> {
    let company_name: Option<Option<String>> =
        sqlx::query_scalar("SELECT name FROM company WHERE id = ? AND deleted_at IS NULL")
            .bind(company_id)
            .fetch_optional(&state.pool)
            .await?;
    Ok(Json(SurveyForm {
        company_name: company_name.flatten().unwrap_or_default(),
        sales_reps: people(&state, company_id, SALES_REP).await?,
        installers: people(&state, company_id, INSTALLER).await?,
    }))
}

/// Body of `POST /v1/public/companies/{company_id}/surveys`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SurveyInput {
    /// The customer's name, only if they choose to give it (up to 255
    /// characters); null or blank keeps the answers anonymous.
    #[serde(default)]
    pub customer_name: Option<String>,
    /// The sales rep rated; one of the company's sales reps.
    pub sales_rep_id: i32,
    /// The installer rated; one of the company's installers.
    pub installer_id: i32,
    /// 1 to 5.
    pub sales_rep_rating: i32,
    /// Comments on the sales rep.
    #[serde(default)]
    pub sales_rep_comments: Option<String>,
    /// 1 to 5.
    pub technician_rating: i32,
    /// Comments on the installer.
    #[serde(default)]
    pub technician_comments: Option<String>,
    /// 1 to 5.
    pub installation_rating: i32,
    /// Comments on the installation.
    #[serde(default)]
    pub installation_comments: Option<String>,
    /// The checklist of the survey link (`c`), with its signed token (`t`):
    /// recording the completion stops the reminders. The answers are saved
    /// without them, so the survey stays anonymous.
    #[serde(default)]
    pub checklist_id: Option<i32>,
    /// The link's token for `checklist_id`.
    #[serde(default)]
    pub token: Option<String>,
}

fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

fn comment(value: Option<&String>) -> Option<&str> {
    value.map(|v| v.trim()).filter(|v| !v.is_empty())
}

/// Answer the survey
///
/// Saves the customer's ratings for the company, with their name only when
/// they give one (otherwise anonymous). With a valid link
/// (`checklist_id` and `token`) the checklist's reminders stop; a missing or
/// forged token only skips that. No session needed.
#[utoipa::path(
    post,
    path = "/v1/public/companies/{company_id}/surveys",
    operation_id = "create_public_survey",
    tag = "public",
    params(("company_id" = i32, Path, description = "Company id")),
    request_body = SurveyInput,
    responses(
        (status = 201, description = "Saved"),
        (status = 400, description = "A rating outside 1-5, a name over 255 characters, or someone who is not the company's sales rep or installer", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    )
)]
pub async fn create_survey(
    State(state): State<AppState>,
    Path(company_id): Path<i32>,
    JsonBody(input): JsonBody<SurveyInput>,
) -> ApiResult<StatusCode> {
    for rating in [
        input.sales_rep_rating,
        input.technician_rating,
        input.installation_rating,
    ] {
        if !(1..=5).contains(&rating) {
            return Err(ApiError::BadRequest("Ratings are 1 to 5".into()));
        }
    }
    let customer_name = comment(input.customer_name.as_ref());
    if customer_name.is_some_and(|name| name.chars().count() > 255) {
        return Err(ApiError::BadRequest(
            "Your name is too long: shorten it to 255 characters or leave it blank".into(),
        ));
    }
    let reps = people(&state, company_id, SALES_REP).await?;
    let installers = people(&state, company_id, INSTALLER).await?;
    if !reps.iter().any(|p| p.id == input.sales_rep_id) {
        return Err(ApiError::BadRequest("Select your sales rep".into()));
    }
    if !installers.iter().any(|p| p.id == input.installer_id) {
        return Err(ApiError::BadRequest("Select your installer".into()));
    }
    sqlx::query(
        "INSERT INTO customer_surveys (sales_rep_id, installer_id, sales_rep_rating,
            sales_rep_comment, technician_rating, technician_comment, installation_rating,
            installation_comment, customer_name, company_id, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW())",
    )
    .bind(input.sales_rep_id)
    .bind(input.installer_id)
    .bind(input.sales_rep_rating)
    .bind(comment(input.sales_rep_comments.as_ref()))
    .bind(input.technician_rating)
    .bind(comment(input.technician_comments.as_ref()))
    .bind(input.installation_rating)
    .bind(comment(input.installation_comments.as_ref()))
    .bind(customer_name)
    .bind(company_id)
    .execute(&state.pool)
    .await?;
    if let (Some(checklist_id), Some(token)) = (input.checklist_id, input.token.as_deref())
        && checklist_id > 0
        && same(
            survey_token(&state.session_secret, checklist_id).as_bytes(),
            token.as_bytes(),
        )
    {
        record_completion(&state, company_id, checklist_id).await?;
    }
    Ok(StatusCode::CREATED)
}

/// Leaves the `checklist-survey-done-<id>` marker that stops the reminders.
async fn record_completion(
    state: &AppState,
    company_id: i32,
    checklist_id: i32,
) -> Result<(), sqlx::Error> {
    let email: Option<Option<String>> =
        sqlx::query_scalar("SELECT email FROM checklists WHERE id = ? AND company_id = ?")
            .bind(checklist_id)
            .bind(company_id)
            .fetch_optional(&state.pool)
            .await?;
    let Some(email) = email else {
        return Ok(());
    };
    sqlx::query(
        "INSERT IGNORE INTO emails (subject, body, message_id, company_id, sender_email,
            receiver_email, sent_at)
         VALUES ('Post-Installation Survey Completed', '', ?, ?, 'noreply@granite-manager.com', ?,
            UTC_TIMESTAMP())",
    )
    .bind(format!("checklist-survey-done-{checklist_id}"))
    .bind(company_id)
    .bind(email.as_deref().map_or("", str::trim))
    .execute(&state.pool)
    .await?;
    Ok(())
}
