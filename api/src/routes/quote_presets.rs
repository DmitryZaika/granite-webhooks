//! Company lists a quote picks from: tax rates and payment terms (Admin >
//! Quotes). Employees read them; admins manage them.
//!
//! - `GET|POST /v1/quote-tax-rates`, `PUT|DELETE /v1/quote-tax-rates/{id}`
//! - `GET|POST /v1/quote-payment-terms`, `PUT|DELETE /v1/quote-payment-terms/{id}`
//! - `GET|PUT /v1/quote-settings` → measurement defaults
//! - `GET|POST /v1/quote-email-templates`, `PUT|DELETE .../{id}` → email defaults
//!
//! Replaces the Remix `admin.quotes.tax-rates`, `.payment-terms`,
//! `.measurements` and `.email-templates` routes.

use crate::auth::{AdminUser, EmployeeUser};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_tax_rates, create_tax_rate))
        .routes(routes!(update_tax_rate, delete_tax_rate))
        .routes(routes!(list_payment_terms, create_payment_term))
        .routes(routes!(update_payment_term, delete_payment_term))
        .routes(routes!(get_settings, save_settings))
        .routes(routes!(list_email_templates, create_email_template))
        .routes(routes!(update_email_template, delete_email_template))
}

/// A tax rate a quote can apply.
#[derive(Debug, Clone, PartialEq, Serialize, FromRow, ToSchema)]
pub struct QuoteTaxRate {
    /// Rate id.
    pub id: i32,
    /// Name, e.g. "Marion County".
    pub name: String,
    /// Order in the lists; the first one is the default.
    pub sequence: i32,
    /// Percent, e.g. `7.25`.
    pub tax_pct: f64,
}

/// Body of creating or saving a tax rate.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QuoteTaxRateInput {
    /// Name; required, at most 120 characters (longer is cut).
    pub name: String,
    /// Order; 1 or more (default 1).
    #[serde(default)]
    pub sequence: Option<i32>,
    /// Percent, 0 to 100.
    pub tax_pct: f64,
}

/// Payment terms a quote can show.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow, ToSchema)]
pub struct QuotePaymentTerm {
    /// Terms id.
    pub id: i32,
    /// Name, e.g. "50% deposit".
    pub name: String,
    /// Order in the lists; the first one prefills new quotes.
    pub sequence: i32,
    /// The terms text.
    pub terms: String,
}

/// Body of creating or saving payment terms.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QuotePaymentTermInput {
    /// Name; required, at most 120 characters (longer is cut).
    pub name: String,
    /// Order; 1 or more (default 1).
    #[serde(default)]
    pub sequence: Option<i32>,
    /// The terms text; required, at most 5000 characters (longer is cut).
    pub terms: String,
}

/// Trimmed and cut to `max` characters; `message` when empty.
fn required(value: &str, max: usize, message: &str) -> Result<String, ApiError> {
    let value: String = value.trim().chars().take(max).collect();
    if value.is_empty() {
        return Err(ApiError::BadRequest(message.into()));
    }
    Ok(value)
}

fn sequence(value: Option<i32>) -> i32 {
    value.unwrap_or(1).max(1)
}

fn tax_pct(value: f64) -> Result<f64, ApiError> {
    if value.is_finite() && (0.0..=100.0).contains(&value) {
        Ok(value)
    } else {
        Err(ApiError::BadRequest(
            "Tax % must be between 0 and 100.".into(),
        ))
    }
}

async fn soft_delete(
    state: &AppState,
    table: &str,
    company_id: i32,
    id: i32,
) -> Result<StatusCode, ApiError> {
    let deleted = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE {table} SET deleted_at = NOW() WHERE id = ? AND company_id = ? AND deleted_at IS NULL"
    )))
    .bind(id)
    .bind(company_id)
    .execute(&state.pool)
    .await?
    .rows_affected();
    if deleted == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn exists(state: &AppState, table: &str, company_id: i32, id: i32) -> Result<(), ApiError> {
    let found: Option<i32> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT 1 FROM {table} WHERE id = ? AND company_id = ? AND deleted_at IS NULL"
    )))
    .bind(id)
    .bind(company_id)
    .fetch_optional(&state.pool)
    .await?;
    found.map(|_| ()).ok_or(ApiError::NotFound)
}

/// List quote tax rates
///
/// The company's tax rates, by sequence then name.
#[utoipa::path(
    get,
    path = "/v1/quote-tax-rates",
    operation_id = "list_quote_tax_rates",
    tag = "quotes",
    responses(
        (status = 200, description = "Tax rates", body = Vec<QuoteTaxRate>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list_tax_rates(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<Vec<QuoteTaxRate>>> {
    Ok(Json(
        sqlx::query_as(
            "SELECT id, name, sequence, CAST(tax_pct AS DOUBLE) AS tax_pct FROM quote_tax_rates
              WHERE company_id = ? AND deleted_at IS NULL ORDER BY sequence, name, id",
        )
        .bind(user.company_id)
        .fetch_all(&state.pool)
        .await?,
    ))
}

/// Add a quote tax rate
///
/// Adds a tax rate quotes can pick.
#[utoipa::path(
    post,
    path = "/v1/quote-tax-rates",
    operation_id = "create_quote_tax_rate",
    tag = "quotes",
    request_body = QuoteTaxRateInput,
    responses(
        (status = 201, description = "Added", body = QuoteTaxRate),
        (status = 400, description = "No name, or a percent outside 0-100", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create_tax_rate(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<QuoteTaxRateInput>,
) -> ApiResult<(StatusCode, Json<QuoteTaxRate>)> {
    let name = required(&input.name, 120, "Name is required.")?;
    let tax_pct = tax_pct(input.tax_pct)?;
    let sequence = sequence(input.sequence);
    let id = sqlx::query(
        "INSERT INTO quote_tax_rates (company_id, name, sequence, tax_pct, created_by)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(user.company_id)
    .bind(&name)
    .bind(sequence)
    .bind(tax_pct)
    .bind(user.id)
    .execute(&state.pool)
    .await?
    .last_insert_id();
    Ok((
        StatusCode::CREATED,
        Json(QuoteTaxRate {
            id: i32::try_from(id).unwrap_or(i32::MAX),
            name,
            sequence,
            tax_pct,
        }),
    ))
}

/// Save a quote tax rate
///
/// Saves the name, sequence and percent of a tax rate.
#[utoipa::path(
    put,
    path = "/v1/quote-tax-rates/{id}",
    operation_id = "update_quote_tax_rate",
    tag = "quotes",
    params(("id" = i32, Path, description = "Tax rate id")),
    request_body = QuoteTaxRateInput,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "No name, or a percent outside 0-100", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such tax rate in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update_tax_rate(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<QuoteTaxRateInput>,
) -> ApiResult<StatusCode> {
    let name = required(&input.name, 120, "Name is required.")?;
    let tax_pct = tax_pct(input.tax_pct)?;
    exists(&state, "quote_tax_rates", user.company_id, id).await?;
    sqlx::query(
        "UPDATE quote_tax_rates SET name = ?, sequence = ?, tax_pct = ?
          WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(name)
    .bind(sequence(input.sequence))
    .bind(tax_pct)
    .bind(id)
    .bind(user.company_id)
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Delete a quote tax rate
///
/// Removes a tax rate from the lists; quotes keep what they saved.
#[utoipa::path(
    delete,
    path = "/v1/quote-tax-rates/{id}",
    operation_id = "delete_quote_tax_rate",
    tag = "quotes",
    params(("id" = i32, Path, description = "Tax rate id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such tax rate in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_tax_rate(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    soft_delete(&state, "quote_tax_rates", user.company_id, id).await
}

/// List quote payment terms
///
/// The company's payment terms, by sequence then name.
#[utoipa::path(
    get,
    path = "/v1/quote-payment-terms",
    operation_id = "list_quote_payment_terms",
    tag = "quotes",
    responses(
        (status = 200, description = "Payment terms", body = Vec<QuotePaymentTerm>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list_payment_terms(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<Vec<QuotePaymentTerm>>> {
    Ok(Json(
        sqlx::query_as(
            "SELECT id, name, sequence, terms FROM quote_payment_terms
              WHERE company_id = ? AND deleted_at IS NULL ORDER BY sequence, name, id",
        )
        .bind(user.company_id)
        .fetch_all(&state.pool)
        .await?,
    ))
}

/// Add quote payment terms
///
/// Adds payment terms quotes can pick.
#[utoipa::path(
    post,
    path = "/v1/quote-payment-terms",
    operation_id = "create_quote_payment_term",
    tag = "quotes",
    request_body = QuotePaymentTermInput,
    responses(
        (status = 201, description = "Added", body = QuotePaymentTerm),
        (status = 400, description = "No name or no terms", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create_payment_term(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<QuotePaymentTermInput>,
) -> ApiResult<(StatusCode, Json<QuotePaymentTerm>)> {
    let name = required(&input.name, 120, "Name is required.")?;
    let terms = required(&input.terms, 5000, "Terms are required.")?;
    let sequence = sequence(input.sequence);
    let id = sqlx::query(
        "INSERT INTO quote_payment_terms (company_id, name, sequence, terms, created_by)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(user.company_id)
    .bind(&name)
    .bind(sequence)
    .bind(&terms)
    .bind(user.id)
    .execute(&state.pool)
    .await?
    .last_insert_id();
    Ok((
        StatusCode::CREATED,
        Json(QuotePaymentTerm {
            id: i32::try_from(id).unwrap_or(i32::MAX),
            name,
            sequence,
            terms,
        }),
    ))
}

/// Save quote payment terms
///
/// Saves the name, sequence and text of payment terms.
#[utoipa::path(
    put,
    path = "/v1/quote-payment-terms/{id}",
    operation_id = "update_quote_payment_term",
    tag = "quotes",
    params(("id" = i32, Path, description = "Payment terms id")),
    request_body = QuotePaymentTermInput,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "No name or no terms", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such payment terms in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update_payment_term(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<QuotePaymentTermInput>,
) -> ApiResult<StatusCode> {
    let name = required(&input.name, 120, "Name is required.")?;
    let terms = required(&input.terms, 5000, "Terms are required.")?;
    exists(&state, "quote_payment_terms", user.company_id, id).await?;
    sqlx::query(
        "UPDATE quote_payment_terms SET name = ?, sequence = ?, terms = ?
          WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(name)
    .bind(sequence(input.sequence))
    .bind(terms)
    .bind(id)
    .bind(user.company_id)
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Delete quote payment terms
///
/// Removes payment terms from the lists; quotes keep what they saved.
#[utoipa::path(
    delete,
    path = "/v1/quote-payment-terms/{id}",
    operation_id = "delete_quote_payment_term",
    tag = "quotes",
    params(("id" = i32, Path, description = "Payment terms id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such payment terms in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_payment_term(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    soft_delete(&state, "quote_payment_terms", user.company_id, id).await
}

/// Measurement roundings a company can pick, in inches.
const ROUNDINGS: [f64; 4] = [0.5, 0.25, 0.125, 0.0625];
const DEFAULT_ROUNDING: f64 = 0.0625;
const DEFAULT_DEPTH: f64 = 25.5;

/// Company defaults for new quotes.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct QuoteSettings {
    /// Always `in`.
    pub units: String,
    /// Measurements round to this many inches: 0.5, 0.25, 0.125 or 0.0625.
    pub rounding: f64,
    /// Default counter depth in inches.
    pub default_depth: f64,
}

/// Body of `PUT /v1/quote-settings`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QuoteSettingsInput {
    /// 0.5, 0.25, 0.125 or 0.0625; anything else reads as 0.0625.
    pub rounding: f64,
    /// Inches, more than 0 and at most 120 (rounded to hundredths).
    pub default_depth: f64,
}

fn rounding(value: f64) -> f64 {
    ROUNDINGS
        .into_iter()
        .find(|option| (option - value).abs() < f64::EPSILON)
        .unwrap_or(DEFAULT_ROUNDING)
}

/// Get the quote settings
///
/// The company's measurement defaults for new quotes (defaults when none
/// are saved).
#[utoipa::path(
    get,
    path = "/v1/quote-settings",
    operation_id = "get_quote_settings",
    tag = "quotes",
    responses(
        (status = 200, description = "The settings", body = QuoteSettings),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get_settings(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<QuoteSettings>> {
    let row: Option<(f64, f64)> = sqlx::query_as(
        "SELECT CAST(measurement_rounding AS DOUBLE), CAST(default_counter_depth AS DOUBLE)
           FROM quote_settings WHERE company_id = ?",
    )
    .bind(user.company_id)
    .fetch_optional(&state.pool)
    .await?;
    let (saved_rounding, depth) = row.unwrap_or((DEFAULT_ROUNDING, DEFAULT_DEPTH));
    Ok(Json(QuoteSettings {
        units: "in".into(),
        rounding: rounding(saved_rounding),
        default_depth: if depth.is_finite() && depth > 0.0 {
            depth
        } else {
            DEFAULT_DEPTH
        },
    }))
}

/// Save the quote settings
///
/// Saves the company's measurement rounding and default counter depth.
#[utoipa::path(
    put,
    path = "/v1/quote-settings",
    operation_id = "save_quote_settings",
    tag = "quotes",
    request_body = QuoteSettingsInput,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Depth not in (0, 120]", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn save_settings(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<QuoteSettingsInput>,
) -> ApiResult<StatusCode> {
    let depth = input.default_depth;
    if !depth.is_finite() || depth <= 0.0 || depth > 120.0 {
        return Err(ApiError::BadRequest(
            "Counter depth must be between 0 and 120 inches.".into(),
        ));
    }
    sqlx::query(
        "INSERT INTO quote_settings
           (company_id, measurement_units, measurement_rounding, default_counter_depth, updated_by)
         VALUES (?, 'in', ?, ?, ?)
         ON DUPLICATE KEY UPDATE measurement_units = VALUES(measurement_units),
           measurement_rounding = VALUES(measurement_rounding),
           default_counter_depth = VALUES(default_counter_depth),
           updated_by = VALUES(updated_by)",
    )
    .bind(user.company_id)
    .bind(rounding(input.rounding))
    .bind((depth * 100.0).round() / 100.0)
    .bind(user.id)
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// A default for emailing quotes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow, ToSchema)]
pub struct QuoteEmailTemplate {
    /// Template id.
    pub id: i32,
    /// The caller's own template (only they see it); false for company ones.
    pub personal: bool,
    /// Name.
    pub name: String,
    /// Email subject.
    pub subject: String,
    /// Email body; links to the quote with `[QuoteLink]` or `[PDFLink]`.
    pub body: String,
    /// Order among the company's templates.
    pub sequence: i32,
}

/// Body of creating or saving a company quote email template.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QuoteEmailTemplateInput {
    /// Name; required, at most 120 characters (longer is cut).
    pub name: String,
    /// Subject; required, at most 255 characters (longer is cut).
    pub subject: String,
    /// Body; `[PDFLink]` is added when it has no quote link.
    pub body: String,
    /// Position among the company's templates (1 = first).
    #[serde(default)]
    pub sequence: Option<i32>,
}

/// Every template links to the quote: `[PDFLink]` is added when the body has none.
fn with_pdf_link(body: &str) -> String {
    if body.contains("[QuoteLink]") || body.contains("[PDFLink]") {
        return body.to_string();
    }
    let text = body.trim_end();
    if text.is_empty() {
        "[PDFLink]".into()
    } else {
        format!("{text}\n\n[PDFLink]")
    }
}

/// Numbers the company templates 1, 2, 3… in order; `saved` goes first among equals.
async fn renumber(
    state: &AppState,
    company_id: i32,
    saved: Option<i32>,
) -> Result<(), sqlx::Error> {
    let rows: Vec<(i32, i32)> = sqlx::query_as(
        "SELECT id, sequence FROM quote_email_templates
          WHERE company_id = ? AND user_id IS NULL AND deleted_at IS NULL
          ORDER BY sequence, id = ? DESC, name, id",
    )
    .bind(company_id)
    .bind(saved.unwrap_or(0))
    .fetch_all(&state.pool)
    .await?;
    for (index, (id, sequence)) in (1..).zip(rows) {
        if sequence != index {
            sqlx::query("UPDATE quote_email_templates SET sequence = ? WHERE id = ?")
                .bind(index)
                .bind(id)
                .execute(&state.pool)
                .await?;
        }
    }
    Ok(())
}

async fn company_template(state: &AppState, company_id: i32, id: i32) -> Result<(), ApiError> {
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM quote_email_templates
          WHERE id = ? AND company_id = ? AND user_id IS NULL AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(company_id)
    .fetch_optional(&state.pool)
    .await?;
    found.map(|_| ()).ok_or(ApiError::NotFound)
}

/// List quote email templates
///
/// The company's templates by sequence, then the caller's own (`personal`).
#[utoipa::path(
    get,
    path = "/v1/quote-email-templates",
    operation_id = "list_quote_email_templates",
    tag = "quotes",
    responses(
        (status = 200, description = "Templates", body = Vec<QuoteEmailTemplate>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list_email_templates(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<Vec<QuoteEmailTemplate>>> {
    Ok(Json(
        sqlx::query_as(
            "SELECT id, user_id IS NOT NULL AS personal, name, subject, body, sequence
               FROM quote_email_templates
              WHERE company_id = ? AND deleted_at IS NULL AND (user_id IS NULL OR user_id = ?)
              ORDER BY user_id IS NOT NULL, sequence, name, id",
        )
        .bind(user.company_id)
        .bind(user.id)
        .fetch_all(&state.pool)
        .await?,
    ))
}

fn template_fields(
    input: &QuoteEmailTemplateInput,
) -> Result<(String, String, String, i32), ApiError> {
    let name = required(&input.name, 120, "Name is required.")?;
    let subject = required(&input.subject, 255, "Subject is required.")?;
    Ok((
        name,
        subject,
        with_pdf_link(&input.body),
        sequence(input.sequence),
    ))
}

/// Add a quote email template
///
/// Adds a company template every sales rep can use, at `sequence`.
#[utoipa::path(
    post,
    path = "/v1/quote-email-templates",
    operation_id = "create_quote_email_template",
    tag = "quotes",
    request_body = QuoteEmailTemplateInput,
    responses(
        (status = 201, description = "Added"),
        (status = 400, description = "No name or no subject", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create_email_template(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<QuoteEmailTemplateInput>,
) -> ApiResult<StatusCode> {
    let (name, subject, body, sequence) = template_fields(&input)?;
    let id = sqlx::query(
        "INSERT INTO quote_email_templates (company_id, user_id, name, subject, body, sequence, created_by)
         VALUES (?, NULL, ?, ?, ?, ?, ?)",
    )
    .bind(user.company_id)
    .bind(name)
    .bind(subject)
    .bind(body)
    .bind(sequence)
    .bind(user.id)
    .execute(&state.pool)
    .await?
    .last_insert_id();
    renumber(&state, user.company_id, i32::try_from(id).ok()).await?;
    Ok(StatusCode::CREATED)
}

/// Save a quote email template
///
/// Saves a company template and moves it to `sequence`.
#[utoipa::path(
    put,
    path = "/v1/quote-email-templates/{id}",
    operation_id = "update_quote_email_template",
    tag = "quotes",
    params(("id" = i32, Path, description = "Template id")),
    request_body = QuoteEmailTemplateInput,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "No name or no subject", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such company template", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update_email_template(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<QuoteEmailTemplateInput>,
) -> ApiResult<StatusCode> {
    let (name, subject, body, sequence) = template_fields(&input)?;
    company_template(&state, user.company_id, id).await?;
    sqlx::query(
        "UPDATE quote_email_templates SET name = ?, subject = ?, body = ?, sequence = ?
          WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(name)
    .bind(subject)
    .bind(body)
    .bind(sequence)
    .bind(id)
    .bind(user.company_id)
    .execute(&state.pool)
    .await?;
    renumber(&state, user.company_id, Some(id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Delete a quote email template
///
/// Deletes a company template and renumbers the others.
#[utoipa::path(
    delete,
    path = "/v1/quote-email-templates/{id}",
    operation_id = "delete_quote_email_template",
    tag = "quotes",
    params(("id" = i32, Path, description = "Template id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such company template", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_email_template(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    company_template(&state, user.company_id, id).await?;
    sqlx::query(
        "UPDATE quote_email_templates SET deleted_at = NOW() WHERE id = ? AND company_id = ?",
    )
    .bind(id)
    .bind(user.company_id)
    .execute(&state.pool)
    .await?;
    renumber(&state, user.company_id, None).await?;
    Ok(StatusCode::NO_CONTENT)
}
