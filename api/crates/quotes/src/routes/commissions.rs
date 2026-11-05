//! Commissions on signed quotes (Admin > Quotes > Commissions, and a rep's
//! own page).
//!
//! - `GET    /v1/commission-rules`, `POST`, `PUT|DELETE /v1/commission-rules/{id}`
//! - `GET|PUT /v1/commission-settings` → payroll weekday
//! - `GET    /v1/commission-base-pay`, `POST`, `PUT|DELETE /v1/commission-base-pay/{id}` → reps' base pay schedules
//! - `GET    /v1/commissions?scope=mine|company` → sold quotes' commissions and the paychecks
//! - `POST   /v1/commissions/sync`    → work out every sold quote's commission, payment (`QuickBooks`) and install
//! - `POST   /v1/commissions/{id}/payroll` → add a paid, installed job to the salary
//! - `PUT|DELETE /v1/commissions/{id}` → an admin's own contract, commission and payroll date; delete a row with no quote
//! - `POST   /v1/commissions/payouts` → mark commissions in the salary paid out
//!
//! Port of `crud/commissions.server.ts` and the `admin.quotes.commissions`
//! and `employee.commissions` routes.

use crate::auth::{AdminUser, EmployeeUser};
use crate::commissions::{
    BasePay, CommissionPart, CommissionRule, GoalRow, PaidRow, PayrollWeek, PositionMember,
    base_pay_by_person, feature_sales, next_week_payroll, payroll_weeks, pick_invoice,
    quote_commission, status_of,
};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::{IntoParams, ToSchema};
use crate::openapi::{OpenApiRouter, routes};
use crate::quote_pricing::pricing::price_quote;
use crate::quote_pricing::{load_company_quote, price_list_for_quote};
use crate::routes::created::CreatedId;
use crate::routes::quickbooks::invoice::InvoiceSummary;
use crate::routes::quickbooks::{InvoicesStatus, invoices_of_customer};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::MySqlPool;
use std::collections::{BTreeMap, HashMap};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_rules, create_rule))
        .routes(routes!(update_rule, delete_rule))
        .routes(routes!(get_settings, save_settings))
        .routes(routes!(list_base_pay, create_base_pay))
        .routes(routes!(update_base_pay, delete_base_pay))
        .routes(routes!(list_commissions))
        .routes(routes!(sync_commissions))
        .routes(routes!(add_to_payroll))
        .routes(routes!(update_commission, delete_commission))
        .routes(routes!(pay_out))
}

#[derive(sqlx::FromRow)]
struct RuleRow {
    id: i32,
    name: String,
    applies: String,
    rate_type: String,
    rate: f64,
    priority: i32,
    stop: i8,
    materials_json: Option<String>,
    colors_json: Option<String>,
    level_min: Option<f64>,
    level_max: Option<f64>,
    line_match: String,
    threshold: Option<f64>,
    active: i8,
}

fn strings(json: Option<&str>) -> Vec<String> {
    json.and_then(|t| serde_json::from_str::<Value>(t).ok())
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect()
}

/// `loadCommissionRules`: priority first.
pub async fn load_rules(pool: &MySqlPool, company_id: i32) -> ApiResult<Vec<CommissionRule>> {
    let rows: Vec<RuleRow> = sqlx::query_as(
        "SELECT id, name, applies, rate_type, CAST(rate AS DOUBLE) AS rate, priority, stop,
                CAST(materials_json AS CHAR) AS materials_json, CAST(colors_json AS CHAR) AS colors_json,
                CAST(level_min AS DOUBLE) AS level_min, CAST(level_max AS DOUBLE) AS level_max,
                line_match, CAST(threshold AS DOUBLE) AS threshold, active
           FROM commission_rules
          WHERE company_id = ? AND deleted_at IS NULL
          ORDER BY priority DESC, id",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| CommissionRule {
            id: i64::from(r.id),
            name: r.name,
            applies: r.applies,
            rate_type: r.rate_type,
            rate: r.rate,
            priority: i64::from(r.priority),
            stop: r.stop == 1,
            materials: strings(r.materials_json.as_deref()),
            colors: strings(r.colors_json.as_deref()),
            level_min: r.level_min,
            level_max: r.level_max,
            line_match: r.line_match,
            threshold: r.threshold,
            active: r.active == 1,
        })
        .collect())
}

/// List commission rules
///
/// The company's rules, highest priority first.
#[crate::openapi::path(
    get,
    path = "/v1/commission-rules",
    operation_id = "list_commission_rules",
    tag = "commissions",
    responses(
        (status = 200, description = "The rules", body = [CommissionRule]),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list_rules(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
) -> ApiResult<Json<Vec<CommissionRule>>> {
    Ok(Json(load_rules(&state.pool, user.company_id).await?))
}

/// A rule as an admin builds it (`commissionRuleSchema`).
#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuleInput {
    /// Name (1 to 120 characters).
    pub name: String,
    /// `material`, `quote`, `lines` or `monthly_goal`.
    pub applies: String,
    /// `percent` or `flat`.
    pub rate_type: String,
    /// Percent or dollars; not negative.
    pub rate: f64,
    /// Higher goes first.
    #[serde(default)]
    pub priority: i64,
    /// Material rules: a match keeps lower rules from paying on the area.
    #[serde(default)]
    pub stop: bool,
    /// Material rules: only these materials.
    #[serde(default)]
    pub materials: Vec<String>,
    /// Material rules: only these colors.
    #[serde(default)]
    pub colors: Vec<String>,
    /// Material rules: lowest level.
    #[serde(default)]
    pub level_min: Option<f64>,
    /// Material rules: highest level.
    #[serde(default)]
    pub level_max: Option<f64>,
    /// Line rules: text the line contains (up to 120 characters).
    #[serde(default)]
    pub line_match: String,
    /// Monthly goals: the month's paid sales that earn the bonus.
    #[serde(default)]
    pub threshold: Option<f64>,
    /// Off rules pay nothing.
    #[serde(default = "yes")]
    pub active: bool,
}

const fn yes() -> bool {
    true
}

/// The rule, checked as `commissionRuleSchema` checks it.
fn checked(input: RuleInput) -> ApiResult<RuleInput> {
    let name = input.name.trim().to_string();
    if name.is_empty() {
        return Err(ApiError::BadRequest("Name is required".into()));
    }
    if name.chars().count() > 120 || input.line_match.trim().chars().count() > 120 {
        return Err(ApiError::BadRequest("Check the rule.".into()));
    }
    if !matches!(
        input.applies.as_str(),
        "material" | "quote" | "lines" | "monthly_goal"
    ) || !matches!(input.rate_type.as_str(), "percent" | "flat")
    {
        return Err(ApiError::BadRequest("Check the rule.".into()));
    }
    if input.rate.is_nan() || input.rate < 0.0 {
        return Err(ApiError::BadRequest("Rate cannot be negative".into()));
    }
    if input.threshold.is_some_and(|t| t < 0.0) {
        return Err(ApiError::BadRequest("Check the rule.".into()));
    }
    let list = |values: Vec<String>| -> ApiResult<Vec<String>> {
        let trimmed: Vec<String> = values.into_iter().map(|v| v.trim().to_string()).collect();
        if trimmed.iter().any(String::is_empty) {
            return Err(ApiError::BadRequest("Check the rule.".into()));
        }
        Ok(trimmed)
    };
    Ok(RuleInput {
        name,
        line_match: input.line_match.trim().to_string(),
        materials: list(input.materials)?,
        colors: list(input.colors)?,
        ..input
    })
}

async fn save_rule(
    pool: &MySqlPool,
    company_id: i32,
    user_id: i32,
    id: Option<i32>,
    rule: RuleInput,
) -> ApiResult<i64> {
    let materials = serde_json::to_string(&rule.materials).expect("strings serialize");
    let colors = serde_json::to_string(&rule.colors).expect("strings serialize");
    if let Some(id) = id {
        let updated = sqlx::query(
            "UPDATE commission_rules
                SET name = ?, applies = ?, rate_type = ?, rate = ?, priority = ?, stop = ?,
                    materials_json = ?, colors_json = ?, level_min = ?, level_max = ?,
                    line_match = ?, threshold = ?, active = ?
              WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
        )
        .bind(&rule.name)
        .bind(&rule.applies)
        .bind(&rule.rate_type)
        .bind(rule.rate)
        .bind(rule.priority)
        .bind(rule.stop)
        .bind(&materials)
        .bind(&colors)
        .bind(rule.level_min)
        .bind(rule.level_max)
        .bind(&rule.line_match)
        .bind(rule.threshold)
        .bind(rule.active)
        .bind(id)
        .bind(company_id)
        .execute(pool)
        .await?
        .rows_affected();
        if updated == 0 {
            return Err(ApiError::NotFound);
        }
        return Ok(i64::from(id));
    }
    let id = sqlx::query(
        "INSERT INTO commission_rules
           (name, applies, rate_type, rate, priority, stop, materials_json, colors_json,
            level_min, level_max, line_match, threshold, active, company_id, created_by)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&rule.name)
    .bind(&rule.applies)
    .bind(&rule.rate_type)
    .bind(rule.rate)
    .bind(rule.priority)
    .bind(rule.stop)
    .bind(&materials)
    .bind(&colors)
    .bind(rule.level_min)
    .bind(rule.level_max)
    .bind(&rule.line_match)
    .bind(rule.threshold)
    .bind(rule.active)
    .bind(company_id)
    .bind(user_id)
    .execute(pool)
    .await?
    .last_insert_id();
    Ok(i64::try_from(id).unwrap_or_default())
}

/// Add a commission rule
///
/// It applies from the next sync.
#[crate::openapi::path(
    post,
    path = "/v1/commission-rules",
    operation_id = "create_commission_rule",
    tag = "commissions",
    request_body = RuleInput,
    responses(
        (status = 201, description = "Added", body = CreatedId),
        (status = 400, description = "Check the rule", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create_rule(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<RuleInput>,
) -> ApiResult<(StatusCode, Json<CreatedId>)> {
    let id = save_rule(&state.pool, user.company_id, user.id, None, checked(input)?).await?;
    Ok((StatusCode::CREATED, Json(CreatedId { id })))
}

/// Save a commission rule
///
/// Replaces the whole rule; it applies from the next sync.
#[crate::openapi::path(
    put,
    path = "/v1/commission-rules/{id}",
    operation_id = "update_commission_rule",
    tag = "commissions",
    params(("id" = i32, Path, description = "Rule id")),
    request_body = RuleInput,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Check the rule", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such rule", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update_rule(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<RuleInput>,
) -> ApiResult<StatusCode> {
    save_rule(
        &state.pool,
        user.company_id,
        user.id,
        Some(id),
        checked(input)?,
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Delete a commission rule
///
/// Commissions already worked out keep what they earned.
#[crate::openapi::path(
    delete,
    path = "/v1/commission-rules/{id}",
    operation_id = "delete_commission_rule",
    tag = "commissions",
    params(("id" = i32, Path, description = "Rule id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_rule(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    sqlx::query("UPDATE commission_rules SET deleted_at = NOW() WHERE id = ? AND company_id = ?")
        .bind(id)
        .bind(user.company_id)
        .execute(&state.pool)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The payroll week (base pay has its own schedules: `/v1/commission-base-pay`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CommissionSettings {
    /// 0 = Sunday … 6 = Saturday: the day payroll goes out.
    pub payroll_weekday: u32,
}

/// `loadCommissionSettings` (Friday payroll until saved).
pub async fn load_settings(pool: &MySqlPool, company_id: i32) -> ApiResult<CommissionSettings> {
    let weekday: Option<i32> = sqlx::query_scalar(
        "SELECT CAST(payroll_weekday AS SIGNED) FROM commission_settings WHERE company_id = ?",
    )
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    Ok(CommissionSettings {
        payroll_weekday: weekday.map_or(5, |w| u32::try_from(w.clamp(0, 6)).unwrap_or(5)),
    })
}

/// Get the commission settings
///
/// Friday payroll until saved.
#[crate::openapi::path(
    get,
    path = "/v1/commission-settings",
    operation_id = "get_commission_settings",
    tag = "commissions",
    responses(
        (status = 200, description = "The settings", body = CommissionSettings),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get_settings(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
) -> ApiResult<Json<CommissionSettings>> {
    Ok(Json(load_settings(&state.pool, user.company_id).await?))
}

/// Save the commission settings
///
/// The weekday is kept within 0 to 6.
#[crate::openapi::path(
    put,
    path = "/v1/commission-settings",
    operation_id = "save_commission_settings",
    tag = "commissions",
    request_body = CommissionSettings,
    responses(
        (status = 204, description = "Saved"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn save_settings(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<CommissionSettings>,
) -> ApiResult<StatusCode> {
    sqlx::query(
        "INSERT INTO commission_settings (company_id, payroll_weekday) VALUES (?, ?)
         ON DUPLICATE KEY UPDATE payroll_weekday = VALUES(payroll_weekday)",
    )
    .bind(user.company_id)
    .bind(input.payroll_weekday.min(6))
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(sqlx::FromRow)]
struct BasePayRow {
    id: i32,
    user_id: Option<i32>,
    rep: Option<String>,
    position_id: Option<i32>,
    amount: f64,
    frequency: String,
    start_date: String,
    end_date: Option<String>,
}

/// The company's base pay: positions' first, then people's by name, then
/// first payday.
pub async fn load_base_pay(pool: &MySqlPool, company_id: i32) -> ApiResult<Vec<BasePay>> {
    let rows: Vec<BasePayRow> = sqlx::query_as(
        "SELECT b.id, b.user_id, u.name AS rep, b.position_id,
                CAST(b.amount AS DOUBLE) AS amount, b.frequency,
                DATE_FORMAT(b.start_date, '%Y-%m-%d') AS start_date,
                DATE_FORMAT(b.end_date, '%Y-%m-%d') AS end_date
           FROM commission_base_pay b
           LEFT JOIN users u ON u.id = b.user_id
          WHERE b.company_id = ? AND b.deleted_at IS NULL
          ORDER BY b.position_id IS NULL, b.position_id, u.name, b.start_date, b.id",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| BasePay {
            id: i64::from(r.id),
            user_id: r.user_id,
            rep: r
                .user_id
                .map(|_| r.rep.unwrap_or_else(|| "Unassigned".into())),
            position_id: r.position_id,
            amount: r.amount,
            frequency: r.frequency,
            start_date: r.start_date,
            end_date: r.end_date,
        })
        .collect())
}

#[derive(sqlx::FromRow)]
struct PositionMemberRow {
    user_id: i32,
    rep: Option<String>,
    position_id: i32,
}

/// Who holds which position in the company (one person's with `user_id`);
/// deleted users hold none.
async fn load_position_members(
    pool: &MySqlPool,
    company_id: i32,
    user_id: Option<i32>,
) -> ApiResult<Vec<PositionMember>> {
    let rows: Vec<PositionMemberRow> = sqlx::query_as(
        "SELECT up.user_id, u.name AS rep, up.position_id
           FROM users_positions up
           JOIN users u ON u.id = up.user_id AND u.is_deleted = 0
          WHERE up.company_id = ? AND (? IS NULL OR up.user_id = ?)
          ORDER BY u.name, up.user_id, up.position_id",
    )
    .bind(company_id)
    .bind(user_id)
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| PositionMember {
            user_id: r.user_id,
            rep: r.rep.unwrap_or_else(|| "Unassigned".into()),
            position_id: r.position_id,
        })
        .collect())
}

/// List base pay
///
/// The base pay schedules: one person's, or everyone's with a position.
#[crate::openapi::path(
    get,
    path = "/v1/commission-base-pay",
    operation_id = "list_commission_base_pay",
    tag = "commissions",
    responses(
        (status = 200, description = "The base pay", body = [BasePay]),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list_base_pay(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
) -> ApiResult<Json<Vec<BasePay>>> {
    Ok(Json(load_base_pay(&state.pool, user.company_id).await?))
}

/// A base pay as an admin sets it: for one person or for a position.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BasePayInput {
    /// One person (an employee of the company)…
    #[serde(default)]
    pub user_id: Option<i32>,
    /// …or everyone with this position; exactly one of the two.
    #[serde(default)]
    pub position_id: Option<i32>,
    /// Paid each time; not negative.
    pub amount: f64,
    /// `weekly`, `biweekly` (every two weeks) or `monthly`.
    pub frequency: String,
    /// The first payday, YYYY-MM-DD; monthly pays on its day of the month.
    pub start_date: String,
    /// The last day it is paid, YYYY-MM-DD; none: no end.
    #[serde(default)]
    pub end_date: Option<String>,
}

fn base_pay_date(text: &str) -> Option<String> {
    let text = text.trim();
    (text.len() == 10)
        .then(|| chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d").ok())
        .flatten()
        .map(|d| d.format("%Y-%m-%d").to_string())
}

async fn save_base_pay(
    pool: &MySqlPool,
    company_id: i32,
    user_id: i32,
    id: Option<i32>,
    input: BasePayInput,
) -> ApiResult<i64> {
    if !input.amount.is_finite() || input.amount < 0.0 {
        return Err(ApiError::BadRequest(
            "The amount cannot be negative.".into(),
        ));
    }
    if !matches!(input.frequency.as_str(), "weekly" | "biweekly" | "monthly") {
        return Err(ApiError::BadRequest(
            "Pick how often: every week, every two weeks or every month.".into(),
        ));
    }
    let start = base_pay_date(&input.start_date)
        .ok_or_else(|| ApiError::BadRequest("Pick the first payday.".into()))?;
    let end = match input.end_date.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(text) => Some(
            base_pay_date(text).ok_or_else(|| ApiError::BadRequest("Pick the last day.".into()))?,
        ),
    };
    if end.as_ref().is_some_and(|end| *end < start) {
        return Err(ApiError::BadRequest(
            "The last day is before the first payday.".into(),
        ));
    }
    match (input.user_id, input.position_id) {
        (Some(person), None) => {
            let found: Option<i32> = sqlx::query_scalar(
                "SELECT id FROM users WHERE id = ? AND company_id = ? AND is_deleted = 0",
            )
            .bind(person)
            .bind(company_id)
            .fetch_optional(pool)
            .await?;
            if found.is_none() {
                return Err(ApiError::BadRequest("Pick someone of your company.".into()));
            }
        }
        (None, Some(position)) => {
            let found: Option<i32> = sqlx::query_scalar("SELECT id FROM positions WHERE id = ?")
                .bind(position)
                .fetch_optional(pool)
                .await?;
            if found.is_none() {
                return Err(ApiError::BadRequest("Pick a position.".into()));
            }
        }
        _ => {
            return Err(ApiError::BadRequest(
                "Pick a position or one person.".into(),
            ));
        }
    }
    let amount = crate::commissions::money(input.amount);
    if let Some(id) = id {
        let found: Option<i32> = sqlx::query_scalar(
            "SELECT id FROM commission_base_pay WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
        )
        .bind(id)
        .bind(company_id)
        .fetch_optional(pool)
        .await?;
        if found.is_none() {
            return Err(ApiError::NotFound);
        }
        sqlx::query(
            "UPDATE commission_base_pay
                SET user_id = ?, position_id = ?, amount = ?, frequency = ?, start_date = ?,
                    end_date = ?
              WHERE id = ? AND company_id = ?",
        )
        .bind(input.user_id)
        .bind(input.position_id)
        .bind(amount)
        .bind(&input.frequency)
        .bind(&start)
        .bind(end.as_deref())
        .bind(id)
        .bind(company_id)
        .execute(pool)
        .await?;
        return Ok(i64::from(id));
    }
    let id = sqlx::query(
        "INSERT INTO commission_base_pay
           (company_id, user_id, position_id, amount, frequency, start_date, end_date,
            created_by)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(company_id)
    .bind(input.user_id)
    .bind(input.position_id)
    .bind(amount)
    .bind(&input.frequency)
    .bind(&start)
    .bind(end.as_deref())
    .bind(user_id)
    .execute(pool)
    .await?
    .last_insert_id();
    Ok(i64::try_from(id).unwrap_or_default())
}

/// Add base pay
///
/// A base pay for one person or for everyone with a position: an amount
/// every week, two weeks or month from the first payday (monthly: on that
/// day of the month), until an optional last day. It shows in the paychecks
/// on those days; a person's own base pay replaces their position's.
#[crate::openapi::path(
    post,
    path = "/v1/commission-base-pay",
    operation_id = "create_commission_base_pay",
    tag = "commissions",
    request_body = BasePayInput,
    responses(
        (status = 201, description = "Added", body = CreatedId),
        (status = 400, description = "Check the base pay", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create_base_pay(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<BasePayInput>,
) -> ApiResult<(StatusCode, Json<CreatedId>)> {
    let id = save_base_pay(&state.pool, user.company_id, user.id, None, input).await?;
    Ok((StatusCode::CREATED, Json(CreatedId { id })))
}

/// Save base pay
///
/// Replaces the whole base pay; past paychecks follow it too.
#[crate::openapi::path(
    put,
    path = "/v1/commission-base-pay/{id}",
    operation_id = "update_commission_base_pay",
    tag = "commissions",
    params(("id" = i32, Path, description = "Base pay id")),
    request_body = BasePayInput,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Check the base pay", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such base pay", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update_base_pay(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<BasePayInput>,
) -> ApiResult<StatusCode> {
    save_base_pay(&state.pool, user.company_id, user.id, Some(id), input).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Delete base pay
///
/// Its paydays leave the paychecks, past ones too; to stop it from a day
/// on, set its last day instead.
#[crate::openapi::path(
    delete,
    path = "/v1/commission-base-pay/{id}",
    operation_id = "delete_commission_base_pay",
    tag = "commissions",
    params(("id" = i32, Path, description = "Base pay id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_base_pay(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    sqlx::query(
        "UPDATE commission_base_pay SET deleted_at = NOW() WHERE id = ? AND company_id = ?",
    )
    .bind(id)
    .bind(user.company_id)
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// A sold quote's commission, or a row with no quote (a rep's sheet, a
/// bonus) (`CommissionRow`).
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CommissionRow {
    /// Row id.
    pub id: i32,
    /// The quote (none: a row with no quote).
    pub quote_id: Option<i32>,
    /// Its name.
    pub quote_name: String,
    /// The customer: the CRM's, else the row's own name (a rep's sheet).
    pub customer_name: String,
    /// The CRM customer (none: a row with only its own name).
    pub customer_id: Option<i32>,
    /// The row's own name for the job (a rep's sheet), e.g. "Smith (cabinets)".
    pub label: Option<String>,
    /// The rep.
    pub user_id: Option<i32>,
    /// Their name ("Unassigned" when none).
    pub rep: String,
    /// When the quote was sold or signed (YYYY-MM-DD).
    pub signed_at: Option<String>,
    /// The contract: the admin's own amount, else the option's total.
    pub quote_total: f64,
    /// The option's total as calculated.
    pub calculated_total: f64,
    /// Earned: the admin's own amount, else what the rules work out.
    pub commission: f64,
    /// What the rules work out.
    pub calculated_commission: f64,
    /// An admin set the contract or the commission by hand.
    pub adjusted: bool,
    /// How.
    pub parts: Vec<CommissionPart>,
    /// Dollars of each extra sold on the quote (`waterfall`, `miter`,
    /// `phantom_seam`, `custom_edge`, `support`, `faucet`,
    /// `soap_dispenser`), as of its last sync; empty without a quote.
    pub features: BTreeMap<String, f64>,
    /// `paid`, `partial`, `unpaid` or `no_invoice`.
    pub status: String,
    /// When the customer paid (YYYY-MM-DD).
    pub paid_date: Option<String>,
    /// When the job was installed: its last Installations event, once all
    /// of them are dated and gone by (YYYY-MM-DD).
    pub installed_date: Option<String>,
    /// The payroll it is paid with, once added to the salary (YYYY-MM-DD).
    pub payroll_date: Option<String>,
    /// When it was paid out (YYYY-MM-DD).
    pub paid_out_at: Option<String>,
}

#[derive(sqlx::FromRow)]
struct CommissionDbRow {
    id: i32,
    quote_id: Option<i32>,
    quote_name: Option<String>,
    customer_name: Option<String>,
    customer_id: Option<i32>,
    label: Option<String>,
    user_id: Option<i32>,
    rep: Option<String>,
    signed_at: Option<String>,
    quote_total: f64,
    calculated_total: f64,
    commission_amount: f64,
    calculated_commission: f64,
    adjusted: i64,
    parts_json: Option<String>,
    features_json: Option<String>,
    status: String,
    paid_date: Option<String>,
    installed_date: Option<String>,
    payroll_date: Option<String>,
    paid_out_at: Option<String>,
}

/// `loadCommissionRows`: newest sold first, the rep's own with `user_id`.
async fn load_rows(
    pool: &MySqlPool,
    company_id: i32,
    user_id: Option<i32>,
) -> ApiResult<Vec<CommissionRow>> {
    let rows: Vec<CommissionDbRow> = sqlx::query_as(
        "SELECT qc.id, qc.quote_id, q.quote_name,
                TRIM(COALESCE(c.name, qc.customer_label)) AS customer_name,
                c.id AS customer_id,
                qc.customer_label AS label, qc.user_id,
                u.name AS rep, DATE_FORMAT(qc.signed_at, '%Y-%m-%d') AS signed_at,
                CAST(COALESCE(qc.total_override, qc.quote_total) AS DOUBLE) AS quote_total,
                CAST(qc.quote_total AS DOUBLE) AS calculated_total,
                CAST(COALESCE(qc.commission_override, qc.commission_amount) AS DOUBLE)
                  AS commission_amount,
                CAST(qc.commission_amount AS DOUBLE) AS calculated_commission,
                CAST(qc.total_override IS NOT NULL OR qc.commission_override IS NOT NULL
                     AS SIGNED) AS adjusted,
                CAST(qc.parts_json AS CHAR) AS parts_json,
                CAST(qc.features_json AS CHAR) AS features_json, qc.status,
                DATE_FORMAT(qc.paid_date, '%Y-%m-%d') AS paid_date,
                DATE_FORMAT(qc.installed_date, '%Y-%m-%d') AS installed_date,
                DATE_FORMAT(qc.payroll_date, '%Y-%m-%d') AS payroll_date,
                DATE_FORMAT(qc.paid_out_at, '%Y-%m-%d') AS paid_out_at
           FROM quote_commissions qc
           LEFT JOIN quotes q ON q.id = qc.quote_id
           LEFT JOIN customers c ON c.id = qc.customer_id
           LEFT JOIN users u ON u.id = qc.user_id
          WHERE qc.company_id = ? AND (qc.quote_id IS NULL OR q.deleted_at IS NULL)
            AND (? IS NULL OR qc.user_id = ?)
          ORDER BY qc.signed_at DESC, qc.id DESC",
    )
    .bind(company_id)
    .bind(user_id)
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| CommissionRow {
            id: r.id,
            quote_id: r.quote_id,
            quote_name: r.quote_name.unwrap_or_default(),
            customer_name: r.customer_name.unwrap_or_default(),
            customer_id: r.customer_id,
            label: r.label,
            user_id: r.user_id,
            rep: r.rep.unwrap_or_else(|| "Unassigned".into()),
            signed_at: r.signed_at,
            quote_total: r.quote_total,
            calculated_total: r.calculated_total,
            commission: r.commission_amount,
            calculated_commission: r.calculated_commission,
            adjusted: r.adjusted != 0,
            parts: r
                .parts_json
                .and_then(|t| serde_json::from_str(&t).ok())
                .unwrap_or_default(),
            features: r
                .features_json
                .and_then(|t| serde_json::from_str(&t).ok())
                .unwrap_or_default(),
            status: r.status,
            paid_date: r.paid_date,
            installed_date: r.installed_date,
            payroll_date: r.payroll_date,
            paid_out_at: r.paid_out_at,
        })
        .collect())
}

/// `?scope=`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CommissionScope {
    /// `mine` (default) or `company` (admins).
    #[serde(default)]
    pub scope: Option<String>,
}

/// Commissions and paychecks.
#[derive(Debug, Serialize, ToSchema)]
pub struct Commissions {
    /// Signed quotes, newest first.
    pub rows: Vec<CommissionRow>,
    /// Paychecks per payroll date, newest first.
    pub weeks: Vec<PayrollWeek>,
}

/// List commissions
///
/// The caller's (`mine`) or, for admins, everyone's (`company`) signed
/// quotes with what they earn, and the paychecks: paid commissions, monthly
/// goal bonuses (by what was sold in the CRM that month) and base pay (their
/// own, or their positions') per payroll date.
#[crate::openapi::path(
    get,
    path = "/v1/commissions",
    operation_id = "list_commissions",
    tag = "commissions",
    params(CommissionScope),
    responses(
        (status = 200, description = "The commissions", body = Commissions),
        (status = 400, description = "Unknown scope", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee, or not an admin for `company`", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list_commissions(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Query(query): Query<CommissionScope>,
) -> ApiResult<Json<Commissions>> {
    let only = match query.scope.as_deref() {
        None | Some("mine") => Some(user.id),
        Some("company") if user.is_admin || user.is_superuser => None,
        Some("company") => return Err(ApiError::Forbidden),
        Some(_) => return Err(ApiError::BadRequest("scope must be mine or company".into())),
    };
    let rows = load_rows(&state.pool, user.company_id, only).await?;
    let rules = load_rules(&state.pool, user.company_id).await?;
    let settings = load_settings(&state.pool, user.company_id).await?;
    let members = load_position_members(&state.pool, user.company_id, only).await?;
    let base_pay: Vec<BasePay> = base_pay_by_person(
        &load_base_pay(&state.pool, user.company_id).await?,
        &members,
    )
    .into_iter()
    .filter(|pay| only.is_none() || pay.user_id == only)
    .collect();
    let quickbooks = state.qbo.is_connected(&state.pool, user.company_id).await?;
    // Each row's synced QuickBooks invoice (row id → invoice id, total).
    let invoices: Vec<(i32, String, f64)> = sqlx::query_as(
        "SELECT id, qbo_invoice_id, CAST(invoice_total AS DOUBLE) FROM quote_commissions
          WHERE company_id = ? AND qbo_invoice_id IS NOT NULL AND qbo_invoice_id <> ''
            AND invoice_total IS NOT NULL",
    )
    .bind(user.company_id)
    .fetch_all(&state.pool)
    .await?;
    let paid: Vec<PaidRow> = rows
        .iter()
        .map(|r| PaidRow {
            invoice: invoices
                .iter()
                .find(|(id, _, _)| *id == r.id)
                .map(|(_, invoice, total)| (invoice.clone(), *total)),
            user_id: r.user_id,
            rep: r.rep.clone(),
            quote_total: r.quote_total,
            commission: r.commission,
            paid_date: r.paid_date.clone(),
            payroll_date: r.payroll_date.clone(),
            signed_at: r.signed_at.clone(),
            goal: GoalRow::of(
                r.quote_id.is_some(),
                r.label.as_deref(),
                r.quote_total,
                r.commission,
            ),
        })
        .collect();
    let weeks = payroll_weeks(
        &paid,
        &rules,
        &base_pay,
        settings.payroll_weekday,
        chrono::Utc::now().date_naive(),
        quickbooks,
    );
    Ok(Json(Commissions { rows, weeks }))
}

/// What a sync did.
#[derive(Debug, Serialize, ToSchema)]
pub struct SyncResult {
    /// Quotes worked out.
    pub synced: usize,
    /// `ok`, `not_connected` (payments unknown) or `error` (some lookups failed).
    pub qbo: String,
}

#[derive(sqlx::FromRow)]
struct SignedQuote {
    id: i32,
    signed_at: Option<String>,
}

/// When a job was installed: the day of its last event on the Installations
/// calendar, once every one of them is dated and gone by. The quote's own
/// events, else the customer's from the sale on.
async fn installed_on(
    pool: &MySqlPool,
    company_id: i32,
    quote_id: i32,
    customer_id: i32,
    sold: Option<&str>,
) -> ApiResult<Option<String>> {
    for by_quote in [true, false] {
        let (last, waiting, count): (Option<String>, i64, i64) = sqlx::query_as(
            "SELECT DATE_FORMAT(MAX(e.start_date), '%Y-%m-%d'),
                    CAST(COALESCE(SUM(e.unscheduled = 1 OR e.start_date > NOW()), 0) AS SIGNED),
                    COUNT(*)
               FROM events e
               JOIN calendars cal ON cal.id = e.calendar_id
              WHERE cal.company_id = ? AND cal.slug = 'installations' AND cal.deleted_at IS NULL
                AND e.deleted_date IS NULL
                AND IF(?, e.quote_id = ?,
                       e.quote_id IS NULL AND e.customer_id = ? AND e.start_date >= ?)",
        )
        .bind(company_id)
        .bind(by_quote)
        .bind(quote_id)
        .bind(customer_id)
        .bind(sold.and_then(|s| s.get(..10)).unwrap_or("9999-12-31"))
        .fetch_one(pool)
        .await?;
        if count > 0 {
            return Ok(if waiting == 0 { last } else { None });
        }
    }
    Ok(None)
}

/// Sync commissions with QuickBooks
///
/// Works out what every sold quote earns, whether its customer paid (the
/// matching `QuickBooks` invoice) and when it was installed. Commissions
/// already added to the salary are left alone; ones of quotes no longer sold
/// go (unless an admin set their amounts).
#[allow(clippy::too_many_lines)]
#[crate::openapi::path(
    post,
    path = "/v1/commissions/sync",
    operation_id = "sync_commissions",
    tag = "commissions",
    responses(
        (status = 200, description = "Synced", body = SyncResult),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn sync_commissions(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
) -> ApiResult<Json<SyncResult>> {
    let company_id = user.company_id;
    let rules = load_rules(&state.pool, company_id).await?;
    sqlx::query(
        "DELETE qc FROM quote_commissions qc
           JOIN quotes q ON q.id = qc.quote_id
          WHERE qc.company_id = ? AND qc.payroll_date IS NULL
            AND qc.total_override IS NULL AND qc.commission_override IS NULL
            AND (q.deleted_at IS NOT NULL
                 OR (q.signed_at IS NULL AND q.status NOT IN ('converted', 'accepted')))",
    )
    .bind(company_id)
    .execute(&state.pool)
    .await?;
    // Sold: signed by the customer, or marked sold (its waiting jobs were
    // added then).
    let quotes: Vec<SignedQuote> = sqlx::query_as(
        "SELECT q.id,
                DATE_FORMAT(COALESCE(q.signed_at,
                                     (SELECT MIN(e.created_date) FROM events e WHERE e.quote_id = q.id),
                                     q.updated_at, q.created_date),
                            '%Y-%m-%d %H:%i:%s') AS signed_at
           FROM quotes q
           LEFT JOIN quote_commissions qc ON qc.quote_id = q.id
          WHERE q.company_id = ? AND q.deleted_at IS NULL
            AND (q.signed_at IS NOT NULL OR q.status IN ('converted', 'accepted'))
            AND (qc.id IS NULL OR qc.payroll_date IS NULL)",
    )
    .bind(company_id)
    .fetch_all(&state.pool)
    .await?;
    let mut invoices_by_customer: HashMap<i32, Option<Vec<InvoiceSummary>>> = HashMap::new();
    let mut qbo = "ok";
    let mut synced = 0;
    for signed in quotes {
        let result: ApiResult<()> = async {
            let Some(quote) = load_company_quote(&state.pool, signed.id, company_id).await? else {
                return Ok(());
            };
            let doc = quote.doc();
            let price_list = price_list_for_quote(&state.pool, &quote).await?;
            let prices = price_quote(&doc, &price_list, quote.options());
            let accepted = usize::try_from(quote.customer_accepted_option_id.unwrap_or(0).max(0))
                .unwrap_or_default()
                .min(prices.len().saturating_sub(1));
            let total = prices.get(accepted).map_or(0.0, |p| p.total);
            let commission = quote_commission(&doc, &price_list, &rules, accepted, &prices);
            let features = prices.get(accepted).map(feature_sales).unwrap_or_default();
            if let std::collections::hash_map::Entry::Vacant(slot) =
                invoices_by_customer.entry(quote.customer_id)
            {
                let found = invoices_of_customer(&state, company_id, quote.customer_id).await;
                let invoices = if let Ok(found) = found {
                    match found.status {
                        InvoicesStatus::Ok => Some(found.invoices),
                        InvoicesStatus::NotConnected => {
                            qbo = "not_connected";
                            None
                        }
                        InvoicesStatus::Error => {
                            qbo = "error";
                            None
                        }
                        InvoicesStatus::MissingQboId => None,
                    }
                } else {
                    qbo = "error";
                    None
                };
                slot.insert(invoices);
            }
            let invoices = invoices_by_customer
                .get(&quote.customer_id)
                .cloned()
                .flatten();
            let invoice = invoices
                .as_deref()
                .and_then(|list| pick_invoice(list, &[total], signed.signed_at.as_deref()));
            let status = if invoices.is_none() {
                "no_invoice"
            } else {
                status_of(invoice)
            };
            let paid_date = (status == "paid").then(|| {
                invoice
                    .and_then(|i| i.last_updated.clone().or_else(|| i.txn_date.clone()))
                    .unwrap_or_else(|| chrono::Utc::now().format("%Y-%m-%d").to_string())
                    .chars()
                    .take(10)
                    .collect::<String>()
            });
            let installed = installed_on(
                &state.pool,
                company_id,
                quote.id,
                quote.customer_id,
                signed.signed_at.as_deref(),
            )
            .await?;
            // The payroll date stays empty until an admin adds the job to the
            // salary (POST /v1/commissions/{id}/payroll).
            sqlx::query(
                "INSERT INTO quote_commissions
                   (company_id, quote_id, user_id, customer_id, option_id, quote_total,
                    commission_amount, parts_json, features_json, qbo_invoice_id, invoice_total,
                    invoice_balance, status, paid_date, installed_date, signed_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                 ON DUPLICATE KEY UPDATE
                   user_id = VALUES(user_id), customer_id = VALUES(customer_id),
                   option_id = VALUES(option_id), quote_total = VALUES(quote_total),
                   commission_amount = VALUES(commission_amount), parts_json = VALUES(parts_json),
                   features_json = VALUES(features_json),
                   qbo_invoice_id = VALUES(qbo_invoice_id), invoice_total = VALUES(invoice_total),
                   invoice_balance = VALUES(invoice_balance), status = VALUES(status),
                   paid_date = IF(VALUES(status) = 'paid', COALESCE(paid_date, VALUES(paid_date)), NULL),
                   installed_date = VALUES(installed_date),
                   synced_at = NOW()",
            )
            .bind(company_id)
            .bind(quote.id)
            .bind(quote.salesperson_id.or(quote.created_by))
            .bind(quote.customer_id)
            .bind(prices.get(accepted).map(|p| p.option_id.clone()))
            .bind(total)
            .bind(commission.total)
            .bind(serde_json::to_string(&commission.parts).expect("parts serialize"))
            .bind(serde_json::to_string(&features).expect("features serialize"))
            .bind(invoice.map(|i| i.id.clone()))
            .bind(invoice.and_then(|i| i.total_amt))
            .bind(invoice.and_then(|i| i.balance))
            .bind(status)
            .bind(paid_date)
            .bind(installed)
            .bind(signed.signed_at.as_deref())
            .execute(&state.pool)
            .await?;
            Ok(())
        }
        .await;
        match result {
            Ok(()) => synced += 1,
            Err(error) => {
                tracing::error!(
                    ?error,
                    quote = signed.id,
                    "failed to sync a quote commission"
                );
            }
        }
    }
    Ok(Json(SyncResult {
        synced,
        qbo: qbo.into(),
    }))
}

#[derive(sqlx::FromRow)]
struct PayrollCheck {
    quote_id: Option<i32>,
    status: String,
    paid_date: Option<String>,
    installed_date: Option<String>,
    payroll_date: Option<String>,
}

/// Where a job went in the salary.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AddedToPayroll {
    /// The payroll it is paid with (YYYY-MM-DD).
    pub payroll_date: String,
}

fn ymd(text: &str) -> Option<chrono::NaiveDate> {
    chrono::NaiveDate::parse_from_str(text.get(..10)?, "%Y-%m-%d").ok()
}

/// Add a commission to the salary
///
/// A sold quote's commission once its customer paid and the job was
/// installed (a row with no quote at any time). It is paid with the payroll
/// of the week after the one it was paid and installed in, or the next
/// payroll when that one has gone by.
#[crate::openapi::path(
    post,
    path = "/v1/commissions/{id}/payroll",
    operation_id = "add_commission_to_payroll",
    tag = "commissions",
    params(("id" = i32, Path, description = "Commission row id")),
    responses(
        (status = 200, description = "Added", body = AddedToPayroll),
        (status = 400, description = "Not paid and installed yet, or already in the salary", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such commission", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn add_to_payroll(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<AddedToPayroll>> {
    let row: PayrollCheck = sqlx::query_as(
        "SELECT quote_id, status, DATE_FORMAT(paid_date, '%Y-%m-%d') AS paid_date,
                DATE_FORMAT(installed_date, '%Y-%m-%d') AS installed_date,
                DATE_FORMAT(payroll_date, '%Y-%m-%d') AS payroll_date
           FROM quote_commissions WHERE id = ? AND company_id = ?",
    )
    .bind(id)
    .bind(user.company_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or(ApiError::NotFound)?;
    if let Some(date) = row.payroll_date {
        return Err(ApiError::BadRequest(format!(
            "It is already in the salary for the payroll of {date}."
        )));
    }
    let today = chrono::Utc::now().date_naive();
    let done = if row.quote_id.is_some() {
        let paid = row.paid_date.as_deref().and_then(ymd);
        let installed = row.installed_date.as_deref().and_then(ymd);
        match (row.status.as_str(), paid, installed) {
            ("paid", Some(paid), Some(installed)) => paid.max(installed),
            ("paid", _, _) => {
                return Err(ApiError::BadRequest(
                    "The job is not installed yet: its Installations events are not all done."
                        .into(),
                ));
            }
            _ => {
                return Err(ApiError::BadRequest(
                    "The customer has not paid yet in QuickBooks. Sync with QuickBooks once they pay."
                        .into(),
                ));
            }
        }
    } else {
        today
    };
    let settings = load_settings(&state.pool, user.company_id).await?;
    let payroll = next_week_payroll(done, today, settings.payroll_weekday)
        .format("%Y-%m-%d")
        .to_string();
    sqlx::query(
        "UPDATE quote_commissions SET payroll_date = ?
          WHERE id = ? AND company_id = ? AND payroll_date IS NULL",
    )
    .bind(&payroll)
    .bind(id)
    .bind(user.company_id)
    .execute(&state.pool)
    .await?;
    Ok(Json(AddedToPayroll {
        payroll_date: payroll,
    }))
}

/// An admin's own contract, commission and payroll date for a row.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CommissionEdit {
    /// The contract (none: the calculated one).
    #[serde(default)]
    pub quote_total: Option<f64>,
    /// The commission (none: the calculated one).
    #[serde(default)]
    pub commission: Option<f64>,
    /// The payroll it is paid with, YYYY-MM-DD (none: not in the salary).
    #[serde(default)]
    pub payroll_date: Option<String>,
}

/// Change a commission
///
/// Sets the contract and commission by hand (they win over the calculated
/// ones and the sync keeps them) and the payroll date. No payroll date takes
/// it out of the salary.
#[crate::openapi::path(
    put,
    path = "/v1/commissions/{id}",
    operation_id = "update_commission",
    tag = "commissions",
    params(("id" = i32, Path, description = "Commission row id")),
    request_body = CommissionEdit,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "A negative amount or a bad date", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such commission", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update_commission(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<CommissionEdit>,
) -> ApiResult<StatusCode> {
    let amount = |value: Option<f64>| -> ApiResult<Option<f64>> {
        match value {
            Some(v) if !v.is_finite() || v < 0.0 => {
                Err(ApiError::BadRequest("Amounts cannot be negative.".into()))
            }
            other => Ok(other.map(crate::commissions::money)),
        }
    };
    let payroll = match input.payroll_date.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(text) => Some(
            ymd(text)
                .filter(|_| text.len() == 10)
                .ok_or_else(|| ApiError::BadRequest("Pick the payroll date.".into()))?
                .format("%Y-%m-%d")
                .to_string(),
        ),
    };
    let updated = sqlx::query(
        "UPDATE quote_commissions
            SET total_override = ?, commission_override = ?,
                paid_out_at = IF(? IS NULL, NULL, paid_out_at), payroll_date = ?
          WHERE id = ? AND company_id = ?",
    )
    .bind(amount(input.quote_total)?)
    .bind(amount(input.commission)?)
    .bind(payroll.as_deref())
    .bind(payroll.as_deref())
    .bind(id)
    .bind(user.company_id)
    .execute(&state.pool)
    .await?
    .rows_affected();
    if updated == 0 {
        let found: Option<i32> =
            sqlx::query_scalar("SELECT id FROM quote_commissions WHERE id = ? AND company_id = ?")
                .bind(id)
                .bind(user.company_id)
                .fetch_optional(&state.pool)
                .await?;
        if found.is_none() {
            return Err(ApiError::NotFound);
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Delete a commission
///
/// Only rows with no quote (a rep's sheet, a bonus); a sold quote's
/// commission goes when the quote is no longer sold.
#[crate::openapi::path(
    delete,
    path = "/v1/commissions/{id}",
    operation_id = "delete_commission",
    tag = "commissions",
    params(("id" = i32, Path, description = "Commission row id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such row with no quote", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_commission(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    let deleted = sqlx::query(
        "DELETE FROM quote_commissions WHERE id = ? AND company_id = ? AND quote_id IS NULL",
    )
    .bind(id)
    .bind(user.company_id)
    .execute(&state.pool)
    .await?
    .rows_affected();
    if deleted == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Commissions paid to the rep, or not after all.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Payout {
    /// Commission row ids.
    pub ids: Vec<i32>,
    /// Paid to the rep (default), or false to take that back.
    #[serde(default = "yes")]
    pub paid: bool,
}

/// Mark commissions paid to the rep
///
/// Marks them paid out today; one not in the salary yet goes into today's
/// paycheck. With `paid: false` they are not paid out any more (and keep
/// their payroll date).
#[crate::openapi::path(
    post,
    path = "/v1/commissions/payouts",
    operation_id = "pay_out_commissions",
    tag = "commissions",
    request_body = Payout,
    responses(
        (status = 204, description = "Marked"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn pay_out(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<Payout>,
) -> ApiResult<StatusCode> {
    if input.ids.is_empty() {
        return Ok(StatusCode::NO_CONTENT);
    }
    let today = chrono::Utc::now()
        .date_naive()
        .format("%Y-%m-%d")
        .to_string();
    let mut sql = sqlx::QueryBuilder::new("UPDATE quote_commissions SET ");
    if input.paid {
        sql.push("paid_out_at = NOW(), payroll_date = COALESCE(payroll_date, ")
            .push_bind(today)
            .push(") WHERE paid_out_at IS NULL");
    } else {
        sql.push("paid_out_at = NULL WHERE paid_out_at IS NOT NULL");
    }
    sql.push(" AND company_id = ")
        .push_bind(user.company_id)
        .push(" AND id IN (");
    let mut ids = sql.separated(", ");
    for id in input.ids {
        ids.push_bind(id);
    }
    sql.push(")");
    sql.build().execute(&state.pool).await?;
    Ok(StatusCode::NO_CONTENT)
}
