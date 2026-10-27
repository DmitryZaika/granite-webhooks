//! Printed quote and contract forms (Admin > Quotes > Forms): layouts the
//! quote page prints, emails and sends for signature.
//!
//! - `GET    /v1/quote-forms?active=` → the company's forms in order
//! - `POST   /v1/quote-forms`         → a new form
//! - `PUT    /v1/quote-forms/order`   → put the forms in this order
//! - `GET    /v1/quote-forms/letterhead` → the company header a form prints
//! - `POST   /v1/quote-forms/images`  → finish an uploaded logo or picture
//! - `GET    /v1/quote-forms/{id}`    → one form with its layout
//! - `PUT    /v1/quote-forms/{id}`    → save name, status, sequence and layout (the editor's autosave)
//! - `PATCH  /v1/quote-forms/{id}`    → (de)activate
//! - `DELETE /v1/quote-forms/{id}`
//! - `POST   /v1/quote-forms/{id}/copies` → duplicate
//!
//! The layout (`QuoteFormConfig`) is checked in the browser with the CRM's
//! schema; the API keeps it as JSON (an object whose `kind` is `quote` or
//! `contract`, text blocks within the CRM's limit). Port of
//! `quoteForms.server.ts` and the `admin.quotes.forms` routes.

use crate::auth::{AdminUser, EmployeeUser};
use crate::entitlements::AddonKey;
use crate::error::{AddonRequiredBody, ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::{IntoParams, ToSchema};
use crate::openapi::{OpenApiRouter, routes};
use crate::routes::created::CreatedId;
use crate::routes::uploads::schemas::UploadFolder;
use crate::state::AppState;
use crate::thumbnails::trim_border;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::MySqlPool;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_forms, create_form))
        .routes(routes!(reorder_forms))
        .routes(routes!(letterhead))
        .routes(routes!(finish_image))
        .routes(routes!(get_form, save_form, set_form_status, delete_form))
        .routes(routes!(copy_form))
}

/// `FORM_TEXT_MAX_LENGTH`.
const TEXT_MAX_LENGTH: usize = 200_000;
const NAME_MAX_LENGTH: usize = 160;

/// A form without its layout.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct QuoteFormSummary {
    /// Form id.
    pub id: i32,
    /// Name.
    pub name: String,
    /// `active` or `inactive`.
    pub status: String,
    /// Order in the print and send dialogs.
    pub sequence: i32,
    /// `quote` or `contract` (ends with signature lines).
    pub kind: String,
}

/// A form with its layout.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct QuoteForm {
    /// Form id.
    pub id: i32,
    /// Name.
    pub name: String,
    /// `active` or `inactive`.
    pub status: String,
    /// Order in the print and send dialogs.
    pub sequence: i32,
    /// `quote` or `contract`.
    pub kind: String,
    /// The layout (`QuoteFormConfig`), as saved; null when unreadable.
    #[schema(value_type = Object)]
    pub config: Value,
}

fn kind_of(config: Option<&Value>) -> String {
    if config.and_then(|c| c.get("kind")).and_then(Value::as_str) == Some("contract") {
        "contract".into()
    } else {
        "quote".into()
    }
}

/// `?active=true`: only active forms.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct FormFilter {
    /// Only active forms (what quotes offer).
    #[serde(default)]
    pub active: bool,
}

/// `listQuoteForms`.
pub async fn summaries(
    pool: &MySqlPool,
    company_id: i32,
    active_only: bool,
) -> ApiResult<Vec<QuoteFormSummary>> {
    let rows: Vec<(i32, String, String, i32, Option<String>)> = sqlx::query_as(
        "SELECT id, name, CAST(status AS CHAR), sequence,
                JSON_UNQUOTE(JSON_EXTRACT(config_json, '$.kind'))
           FROM quote_forms
          WHERE company_id = ? AND deleted_at IS NULL AND (? = 0 OR status = 'active')
          ORDER BY sequence, name",
    )
    .bind(company_id)
    .bind(active_only)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, name, status, sequence, kind)| QuoteFormSummary {
            id,
            name,
            status,
            sequence,
            kind: if kind.as_deref() == Some("contract") {
                "contract".into()
            } else {
                "quote".into()
            },
        })
        .collect())
}

/// `getQuoteForm`.
pub async fn find(pool: &MySqlPool, company_id: i32, id: i32) -> ApiResult<Option<QuoteForm>> {
    let row: Option<(i32, String, String, i32, Option<String>)> = sqlx::query_as(
        "SELECT id, name, CAST(status AS CHAR), sequence, CAST(config_json AS CHAR)
           FROM quote_forms WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(id, name, status, sequence, config)| {
        let config: Value = config
            .and_then(|text| serde_json::from_str(&text).ok())
            .filter(Value::is_object)
            .unwrap_or(Value::Null);
        QuoteForm {
            kind: kind_of(Some(&config)),
            id,
            name,
            status,
            sequence,
            config,
        }
    }))
}

/// Why a layout cannot be saved, if it cannot (`quoteFormConfigError`'s
/// checks the API can make).
fn config_problem(config: &Value) -> Option<String> {
    fn long_text(value: &Value) -> bool {
        match value {
            Value::Object(map) => map.iter().any(|(key, value)| {
                (key == "text"
                    && value
                        .as_str()
                        .is_some_and(|t| t.chars().count() > TEXT_MAX_LENGTH))
                    || long_text(value)
            }),
            Value::Array(items) => items.iter().any(long_text),
            _ => false,
        }
    }
    if !config.is_object()
        || !matches!(
            config.get("kind").and_then(Value::as_str),
            None | Some("quote" | "contract")
        )
    {
        return Some("The form could not be read.".into());
    }
    long_text(config)
        .then(|| "A text block is too long. The limit is 200,000 characters.".to_string())
}

fn form_name(name: &str) -> ApiResult<String> {
    let name: String = name.trim().chars().take(NAME_MAX_LENGTH).collect();
    if name.is_empty() {
        return Err(ApiError::BadRequest("Name is required.".into()));
    }
    Ok(name)
}

async fn insert(
    pool: &MySqlPool,
    company_id: i32,
    user_id: i32,
    name: &str,
    config: &Value,
) -> ApiResult<i64> {
    let last: i64 = sqlx::query_scalar(
        "SELECT CAST(COALESCE(MAX(sequence), 0) AS SIGNED) FROM quote_forms
          WHERE company_id = ? AND deleted_at IS NULL",
    )
    .bind(company_id)
    .fetch_one(pool)
    .await?;
    let id = sqlx::query(
        "INSERT INTO quote_forms (company_id, name, sequence, config_json, created_by)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(company_id)
    .bind(name)
    .bind(last + 1)
    .bind(config.to_string())
    .bind(user_id)
    .execute(pool)
    .await?
    .last_insert_id();
    Ok(i64::try_from(id).unwrap_or_default())
}

/// List quote forms
///
/// The company's forms by sequence; `active=true` for the ones quotes offer.
#[crate::openapi::path(
    get,
    path = "/v1/quote-forms",
    operation_id = "list_quote_forms",
    tag = "quotes",
    params(FormFilter),
    responses(
        (status = 200, description = "The forms", body = [QuoteFormSummary]),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin, or the plan lacks the Quotes add-on (`addon`: `quotes`)", body = AddonRequiredBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list_forms(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Query(filter): Query<FormFilter>,
) -> ApiResult<Json<Vec<QuoteFormSummary>>> {
    user.plan.require(AddonKey::Quotes)?;
    Ok(Json(
        summaries(&state.pool, user.company_id, filter.active).await?,
    ))
}

/// A new form.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewQuoteForm {
    /// Name (up to 160 characters).
    pub name: String,
    /// The starting layout (`emptyQuoteFormConfig(kind)`).
    #[schema(value_type = Object)]
    pub config: Value,
}

/// Add a quote form
///
/// Last in order.
#[crate::openapi::path(
    post,
    path = "/v1/quote-forms",
    operation_id = "create_quote_form",
    tag = "quotes",
    request_body = NewQuoteForm,
    responses(
        (status = 201, description = "Created", body = CreatedId),
        (status = 400, description = "No name, or the layout cannot be read", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin, or the plan lacks the Quotes add-on (`addon`: `quotes`)", body = AddonRequiredBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create_form(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<NewQuoteForm>,
) -> ApiResult<(StatusCode, Json<CreatedId>)> {
    user.plan.require(AddonKey::Quotes)?;
    let name = form_name(&input.name)?;
    if let Some(problem) = config_problem(&input.config) {
        return Err(ApiError::BadRequest(problem));
    }
    let id = insert(&state.pool, user.company_id, user.id, &name, &input.config).await?;
    Ok((StatusCode::CREATED, Json(CreatedId { id })))
}

/// Duplicate a quote form
///
/// A copy named "… (copy)", last in order.
#[crate::openapi::path(
    post,
    path = "/v1/quote-forms/{id}/copies",
    operation_id = "copy_quote_form",
    tag = "quotes",
    params(("id" = i32, Path, description = "Form id")),
    responses(
        (status = 201, description = "Created", body = CreatedId),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin, or the plan lacks the Quotes add-on (`addon`: `quotes`)", body = AddonRequiredBody),
        (status = 404, description = "No such form", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn copy_form(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
) -> ApiResult<(StatusCode, Json<CreatedId>)> {
    user.plan.require(AddonKey::Quotes)?;
    let form = find(&state.pool, user.company_id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let name = form_name(&format!("{} (copy)", form.name))?;
    let id = insert(&state.pool, user.company_id, user.id, &name, &form.config).await?;
    Ok((StatusCode::CREATED, Json(CreatedId { id })))
}

/// Forms in their new order.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FormOrder {
    /// Form ids, first to last; other companies' ids are ignored.
    pub ids: Vec<i32>,
}

/// Reorder quote forms
///
/// Sequence 1, 2, 3… in this order: the order the print and send dialogs
/// list them in.
#[crate::openapi::path(
    put,
    path = "/v1/quote-forms/order",
    operation_id = "reorder_quote_forms",
    tag = "quotes",
    request_body = FormOrder,
    responses(
        (status = 204, description = "Saved"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin, or the plan lacks the Quotes add-on (`addon`: `quotes`)", body = AddonRequiredBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn reorder_forms(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<FormOrder>,
) -> ApiResult<StatusCode> {
    user.plan.require(AddonKey::Quotes)?;
    let mut order: Vec<i32> = Vec::new();
    for id in input.ids.into_iter().filter(|id| *id > 0) {
        if !order.contains(&id) {
            order.push(id);
        }
    }
    let mut tx = state.pool.begin().await?;
    for (position, id) in order.iter().enumerate() {
        sqlx::query(
            "UPDATE quote_forms SET sequence = ? WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
        )
        .bind(i64::try_from(position).unwrap_or_default() + 1)
        .bind(id)
        .bind(user.company_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Get a quote form
///
/// With its layout (for printing and the editor).
#[crate::openapi::path(
    get,
    path = "/v1/quote-forms/{id}",
    operation_id = "get_quote_form",
    tag = "quotes",
    params(("id" = i32, Path, description = "Form id")),
    responses(
        (status = 200, description = "The form", body = QuoteForm),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin, or the plan lacks the Quotes add-on (`addon`: `quotes`)", body = AddonRequiredBody),
        (status = 404, description = "No such form", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get_form(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<QuoteForm>> {
    user.plan.require(AddonKey::Quotes)?;
    find(&state.pool, user.company_id, id)
        .await?
        .map(Json)
        .ok_or(ApiError::NotFound)
}

/// A form as the editor saves it.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SavedQuoteForm {
    /// Name (up to 160 characters).
    pub name: String,
    /// `active` or `inactive`.
    pub status: String,
    /// Order (1 or more).
    pub sequence: f64,
    /// The layout (`QuoteFormConfig`).
    #[schema(value_type = Object)]
    pub config: Value,
}

/// Save a quote form
///
/// Name, status, sequence and layout, as the editor stores every change.
#[crate::openapi::path(
    put,
    path = "/v1/quote-forms/{id}",
    operation_id = "save_quote_form",
    tag = "quotes",
    params(("id" = i32, Path, description = "Form id")),
    request_body = SavedQuoteForm,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "No name, or the layout cannot be read", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin, or the plan lacks the Quotes add-on (`addon`: `quotes`)", body = AddonRequiredBody),
        (status = 404, description = "No such form", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn save_form(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<SavedQuoteForm>,
) -> ApiResult<StatusCode> {
    user.plan.require(AddonKey::Quotes)?;
    find(&state.pool, user.company_id, id)
        .await?
        .ok_or_else(|| ApiError::NotFound)?;
    let name = form_name(&input.name)?;
    if let Some(problem) = config_problem(&input.config) {
        return Err(ApiError::BadRequest(problem));
    }
    #[allow(clippy::cast_possible_truncation)]
    let sequence = if input.sequence.is_finite() {
        input.sequence.round().max(1.0) as i64
    } else {
        1
    };
    sqlx::query(
        "UPDATE quote_forms SET name = ?, status = ?, sequence = ?, config_json = ?
          WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(name)
    .bind(if input.status == "inactive" {
        "inactive"
    } else {
        "active"
    })
    .bind(sequence)
    .bind(input.config.to_string())
    .bind(id)
    .bind(user.company_id)
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// A form's status.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FormStatus {
    /// `active` or `inactive`.
    pub status: String,
}

/// Activate or deactivate a quote form
///
/// Inactive forms are not offered on quotes.
#[crate::openapi::path(
    patch,
    path = "/v1/quote-forms/{id}",
    operation_id = "set_quote_form_status",
    tag = "quotes",
    params(("id" = i32, Path, description = "Form id")),
    request_body = FormStatus,
    responses(
        (status = 204, description = "Saved"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin, or the plan lacks the Quotes add-on (`addon`: `quotes`)", body = AddonRequiredBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn set_form_status(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<FormStatus>,
) -> ApiResult<StatusCode> {
    user.plan.require(AddonKey::Quotes)?;
    sqlx::query(
        "UPDATE quote_forms SET status = ? WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(if input.status == "inactive" {
        "inactive"
    } else {
        "active"
    })
    .bind(id)
    .bind(user.company_id)
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Delete a quote form
///
/// It is no longer offered; quotes printed with it are unchanged.
#[crate::openapi::path(
    delete,
    path = "/v1/quote-forms/{id}",
    operation_id = "delete_quote_form",
    tag = "quotes",
    params(("id" = i32, Path, description = "Form id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin, or the plan lacks the Quotes add-on (`addon`: `quotes`)", body = AddonRequiredBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_form(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    user.plan.require(AddonKey::Quotes)?;
    sqlx::query(
        "UPDATE quote_forms SET deleted_at = NOW() WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(user.company_id)
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The company header a form prints (`loadFormCompany`).
#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct Letterhead {
    /// Company name.
    pub name: Option<String>,
    /// Address.
    pub address: Option<String>,
    /// Website.
    pub domain: Option<String>,
    /// Logo.
    #[serde(rename = "logoUrl")]
    #[sqlx(rename = "logo_url")]
    pub logo_url: Option<String>,
}

/// `loadFormCompany`.
pub async fn load_letterhead(pool: &MySqlPool, company_id: i32) -> ApiResult<Letterhead> {
    let row: Option<Letterhead> =
        sqlx::query_as("SELECT name, address, domain, logo_url FROM company WHERE id = ?")
            .bind(company_id)
            .fetch_optional(pool)
            .await?;
    Ok(row.unwrap_or(Letterhead {
        name: None,
        address: None,
        domain: None,
        logo_url: None,
    }))
}

/// Get the form letterhead
///
/// The company name, address, website and logo a printed form shows.
#[crate::openapi::path(
    get,
    path = "/v1/quote-forms/letterhead",
    operation_id = "get_quote_form_letterhead",
    tag = "quotes",
    responses(
        (status = 200, description = "The letterhead", body = Letterhead),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin, or the plan lacks the Quotes add-on (`addon`: `quotes`)", body = AddonRequiredBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn letterhead(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<Letterhead>> {
    user.plan.require(AddonKey::Quotes)?;
    Ok(Json(load_letterhead(&state.pool, user.company_id).await?))
}

/// An uploaded form image.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FormImage {
    /// Its URL, from `POST /v1/uploads` with folder `quote-forms`.
    pub url: String,
    /// A header logo: cut off its blank border, which would push the form down.
    #[serde(default)]
    pub trim_border: bool,
}

/// The image to put on the form.
#[derive(Debug, Serialize, ToSchema)]
pub struct FinishedImage {
    /// Its URL.
    pub url: String,
}

/// Finish a form image
///
/// Checks an image uploaded to `quote-forms/` and, for a header logo, cuts
/// off its blank border.
#[crate::openapi::path(
    post,
    path = "/v1/quote-forms/images",
    operation_id = "finish_quote_form_image",
    tag = "quotes",
    request_body = FormImage,
    responses(
        (status = 200, description = "The image", body = FinishedImage),
        (status = 400, description = "Not an uploaded form image", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin, or the plan lacks the Quotes add-on (`addon`: `quotes`)", body = AddonRequiredBody),
        (status = 500, description = "Storage error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn finish_image(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<FormImage>,
) -> ApiResult<Json<FinishedImage>> {
    user.plan.require(AddonKey::Quotes)?;
    // Trimming overwrites the file: only one the company uploaded.
    let key = crate::routes::uploads::company_key(
        &state.storage,
        &input.url,
        UploadFolder::QuoteForms,
        user.company_id,
    )
    .ok_or_else(|| ApiError::BadRequest("Upload failed.".into()))?;
    let object = state
        .storage
        .get(&key)
        .await?
        .ok_or_else(|| ApiError::BadRequest("Upload failed.".into()))?;
    if input.trim_border
        && let Some(trimmed) = trim_border(&object.bytes)
    {
        state
            .storage
            .put(&key, trimmed, &object.content_type)
            .await?;
    }
    Ok(Json(FinishedImage { url: input.url }))
}
