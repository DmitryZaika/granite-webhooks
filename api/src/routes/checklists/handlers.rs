use super::queries::list_checklists;
use super::schemas::Checklist;
use crate::auth::EmployeeUser;
use crate::error::{ApiResult, ErrorBody};
use crate::state::AppState;
use axum::Json;
use axum::extract::State;

/// List the company's post-installation checklists
///
/// Returns every checklist of the caller's company, newest first (ties by
/// id, highest first), with the installer's name and the result of each
/// check. The customer's signature is not included; the signed PDF of a
/// checklist is at `/api/checklist-pdf/{id}` on the web app.
#[utoipa::path(
    get,
    path = "/v1/checklists",
    operation_id = "list_checklists",
    tag = "checklists",
    responses(
        (status = 200, description = "Checklists, newest first", body = Vec<Checklist>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<Vec<Checklist>>> {
    Ok(Json(list_checklists(&state.pool, user.company_id).await?))
}

/// Body of `POST /v1/checklists`: the customer's post-installation sign-off.
#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
pub struct NewChecklist {
    /// The customer, when known.
    #[serde(default)]
    pub customer_id: Option<i32>,
    /// Customer name; required.
    pub customer_name: String,
    /// Where the job was; required.
    pub installation_address: String,
    /// Where to send the copy.
    #[serde(default)]
    pub email: Option<String>,
    /// Each check is `on` when ticked.
    #[serde(default)]
    pub material_correct: Option<String>,
    /// See `material_correct`.
    #[serde(default)]
    pub seams_satisfaction: Option<String>,
    /// See `material_correct`.
    #[serde(default)]
    pub appliances_fit: Option<String>,
    /// See `material_correct`.
    #[serde(default)]
    pub backsplashes_correct: Option<String>,
    /// See `material_correct`.
    #[serde(default)]
    pub edges_correct: Option<String>,
    /// See `material_correct`.
    #[serde(default)]
    pub holes_drilled: Option<String>,
    /// See `material_correct`.
    #[serde(default)]
    pub cleanup_completed: Option<String>,
    /// Customer comments.
    #[serde(default)]
    pub comments: Option<String>,
    /// The customer's signature (image data URL); required.
    pub signature: String,
}

/// `POST /v1/checklists`.
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct CreatedChecklist {
    /// The new checklist.
    pub id: u64,
}

/// Save a post-installation checklist
///
/// An installer records the customer's sign-off. Offline installers queue
/// these and send them when back online.
#[utoipa::path(
    post,
    path = "/v1/checklists",
    operation_id = "create_checklist",
    tag = "checklists",
    request_body = NewChecklist,
    responses(
        (status = 201, description = "Saved", body = CreatedChecklist),
        (status = 400, description = "Missing name, address or signature, or a bad email", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an installer", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create(
    State(state): State<AppState>,
    crate::auth::InstallerUser(user): crate::auth::InstallerUser,
    crate::extract::JsonBody(input): crate::extract::JsonBody<NewChecklist>,
) -> ApiResult<(axum::http::StatusCode, Json<CreatedChecklist>)> {
    use crate::error::ApiError;
    let required = |value: &str, message: &str| {
        if value.is_empty() {
            Err(ApiError::BadRequest(message.into()))
        } else {
            Ok(())
        }
    };
    required(&input.customer_name, "customer_name is required")?;
    required(
        &input.installation_address,
        "installation_address is required",
    )?;
    required(&input.signature, "Signature is required")?;
    let email = input
        .email
        .as_deref()
        .map(str::trim)
        .filter(|email| !email.is_empty());
    if email.is_some_and(|email| !crate::routes::emails::send::valid_address(email)) {
        return Err(ApiError::BadRequest("Invalid email".into()));
    }
    let ticked = |value: Option<&String>| value.is_some_and(|value| value == "on");
    let id = sqlx::query(
        "INSERT INTO checklists (customer_id, installer_id, customer_name, installation_address,
            material_correct, seams_satisfaction, appliances_fit, backsplashes_correct,
            edges_correct, holes_drilled, cleanup_completed, comments, signature, company_id, email)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(input.customer_id.filter(|id| *id > 0))
    .bind(user.id)
    .bind(&input.customer_name)
    .bind(&input.installation_address)
    .bind(ticked(input.material_correct.as_ref()))
    .bind(ticked(input.seams_satisfaction.as_ref()))
    .bind(ticked(input.appliances_fit.as_ref()))
    .bind(ticked(input.backsplashes_correct.as_ref()))
    .bind(ticked(input.edges_correct.as_ref()))
    .bind(ticked(input.holes_drilled.as_ref()))
    .bind(ticked(input.cleanup_completed.as_ref()))
    .bind(input.comments.as_deref().filter(|c| !c.is_empty()))
    .bind(&input.signature)
    .bind(user.company_id)
    .bind(email)
    .execute(&state.pool)
    .await?
    .last_insert_id();
    Ok((
        axum::http::StatusCode::CREATED,
        Json(CreatedChecklist { id }),
    ))
}
