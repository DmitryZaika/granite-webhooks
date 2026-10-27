//! Commissions on signed quotes (Admin > Quotes > Commissions, and a rep's
//! own page).
//!
//! - `GET    /v1/commission-rules`, `POST`, `PUT|DELETE /v1/commission-rules/{id}`
//! - `GET|PUT /v1/commission-settings` → weekly base and payroll weekday
//! - `GET    /v1/commissions?scope=mine|company` → signed quotes' commissions and the paychecks
//! - `POST   /v1/commissions/sync`    → work out every signed quote's commission and payment (`QuickBooks`)
//! - `POST   /v1/commissions/payouts` → mark paid commissions paid out
//!
//! Port of `crud/commissions.server.ts` and the `admin.quotes.commissions`
//! and `employee.commissions` routes.

use crate::auth::{AdminUser, EmployeeUser};
use crate::commissions::{
    CommissionPart, CommissionRule, PaidRow, PayrollWeek, payroll_date_for, payroll_weeks,
    pick_invoice, quote_commission, status_of,
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
use std::collections::HashMap;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_rules, create_rule))
        .routes(routes!(update_rule, delete_rule))
        .routes(routes!(get_settings, save_settings))
        .routes(routes!(list_commissions))
        .routes(routes!(sync_commissions))
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

/// Base pay and payroll week.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CommissionSettings {
    /// Paid every payroll week on top of commissions (not negative).
    pub base_weekly: f64,
    /// 0 = Sunday … 6 = Saturday: the day payroll goes out.
    pub payroll_weekday: u32,
}

/// `loadCommissionSettings` (Friday payroll and no base until saved).
pub async fn load_settings(pool: &MySqlPool, company_id: i32) -> ApiResult<CommissionSettings> {
    let row: Option<(f64, i32)> = sqlx::query_as(
        "SELECT CAST(base_weekly AS DOUBLE), CAST(payroll_weekday AS SIGNED) FROM commission_settings WHERE company_id = ?",
    )
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map_or(
        CommissionSettings {
            base_weekly: 0.0,
            payroll_weekday: 5,
        },
        |(base, weekday)| CommissionSettings {
            base_weekly: base,
            payroll_weekday: u32::try_from(weekday.clamp(0, 6)).unwrap_or(5),
        },
    ))
}

/// Get the commission settings
///
/// Friday payroll and no base until saved.
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
/// A negative base becomes 0; the weekday is kept within 0 to 6.
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
        "INSERT INTO commission_settings (company_id, base_weekly, payroll_weekday) VALUES (?, ?, ?)
         ON DUPLICATE KEY UPDATE base_weekly = VALUES(base_weekly), payroll_weekday = VALUES(payroll_weekday)",
    )
    .bind(user.company_id)
    .bind(if input.base_weekly.is_finite() { input.base_weekly.max(0.0) } else { 0.0 })
    .bind(input.payroll_weekday.min(6))
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// A signed quote's commission (`CommissionRow`).
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CommissionRow {
    /// Row id.
    pub id: i32,
    /// The quote.
    pub quote_id: i32,
    /// Its name.
    pub quote_name: String,
    /// The customer.
    pub customer_name: String,
    /// The rep.
    pub user_id: Option<i32>,
    /// Their name ("Unassigned" when none).
    pub rep: String,
    /// When the quote was signed (YYYY-MM-DD).
    pub signed_at: Option<String>,
    /// The option's total.
    pub quote_total: f64,
    /// Earned.
    pub commission: f64,
    /// How.
    pub parts: Vec<CommissionPart>,
    /// `paid`, `partial`, `unpaid` or `no_invoice`.
    pub status: String,
    /// When the customer paid (YYYY-MM-DD).
    pub paid_date: Option<String>,
    /// The payroll it is paid with (YYYY-MM-DD).
    pub payroll_date: Option<String>,
    /// When it was paid out (YYYY-MM-DD).
    pub paid_out_at: Option<String>,
}

#[derive(sqlx::FromRow)]
struct CommissionDbRow {
    id: i32,
    quote_id: i32,
    quote_name: Option<String>,
    customer_name: Option<String>,
    user_id: Option<i32>,
    rep: Option<String>,
    signed_at: Option<String>,
    quote_total: f64,
    commission_amount: f64,
    parts_json: Option<String>,
    status: String,
    paid_date: Option<String>,
    payroll_date: Option<String>,
    paid_out_at: Option<String>,
}

/// `loadCommissionRows`: newest signed first, the rep's own with `user_id`.
async fn load_rows(
    pool: &MySqlPool,
    company_id: i32,
    user_id: Option<i32>,
) -> ApiResult<Vec<CommissionRow>> {
    let rows: Vec<CommissionDbRow> = sqlx::query_as(
        "SELECT qc.id, qc.quote_id, q.quote_name, c.name AS customer_name, qc.user_id,
                u.name AS rep, DATE_FORMAT(qc.signed_at, '%Y-%m-%d') AS signed_at,
                CAST(qc.quote_total AS DOUBLE) AS quote_total,
                CAST(qc.commission_amount AS DOUBLE) AS commission_amount,
                CAST(qc.parts_json AS CHAR) AS parts_json, qc.status,
                DATE_FORMAT(qc.paid_date, '%Y-%m-%d') AS paid_date,
                DATE_FORMAT(qc.payroll_date, '%Y-%m-%d') AS payroll_date,
                DATE_FORMAT(qc.paid_out_at, '%Y-%m-%d') AS paid_out_at
           FROM quote_commissions qc
           JOIN quotes q ON q.id = qc.quote_id
           JOIN customers c ON c.id = qc.customer_id
           LEFT JOIN users u ON u.id = qc.user_id
          WHERE qc.company_id = ? AND q.deleted_at IS NULL AND (? IS NULL OR qc.user_id = ?)
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
            user_id: r.user_id,
            rep: r.rep.unwrap_or_else(|| "Unassigned".into()),
            signed_at: r.signed_at,
            quote_total: r.quote_total,
            commission: r.commission_amount,
            parts: r
                .parts_json
                .and_then(|t| serde_json::from_str(&t).ok())
                .unwrap_or_default(),
            status: r.status,
            paid_date: r.paid_date,
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
/// goal bonuses and the weekly base per payroll date.
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
    let paid: Vec<PaidRow> = rows
        .iter()
        .map(|r| PaidRow {
            user_id: r.user_id,
            rep: r.rep.clone(),
            status: r.status.clone(),
            quote_total: r.quote_total,
            commission: r.commission,
            paid_date: r.paid_date.clone(),
            payroll_date: r.payroll_date.clone(),
        })
        .collect();
    let weeks = payroll_weeks(
        &paid,
        &rules,
        settings.base_weekly,
        settings.payroll_weekday,
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

/// Sync commissions with QuickBooks
///
/// Works out what every signed quote earns and whether its customer paid
/// (the matching `QuickBooks` invoice). Quotes paid and signed over 60 days
/// ago are left alone.
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
    let settings = load_settings(&state.pool, company_id).await?;
    let quotes: Vec<SignedQuote> = sqlx::query_as(
        "SELECT q.id, DATE_FORMAT(q.signed_at, '%Y-%m-%d %H:%i:%s') AS signed_at
           FROM quotes q
           LEFT JOIN quote_commissions qc ON qc.quote_id = q.id
          WHERE q.company_id = ? AND q.deleted_at IS NULL AND q.signed_at IS NOT NULL
            AND (qc.id IS NULL OR qc.status <> 'paid'
                 OR q.signed_at >= DATE_SUB(NOW(), INTERVAL 60 DAY))",
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
            let payroll_date = paid_date
                .as_deref()
                .and_then(|d| payroll_date_for(d, settings.payroll_weekday));
            sqlx::query(
                "INSERT INTO quote_commissions
                   (company_id, quote_id, user_id, customer_id, option_id, quote_total,
                    commission_amount, parts_json, qbo_invoice_id, invoice_total,
                    invoice_balance, status, paid_date, payroll_date, signed_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                 ON DUPLICATE KEY UPDATE
                   user_id = VALUES(user_id), option_id = VALUES(option_id),
                   quote_total = VALUES(quote_total),
                   commission_amount = VALUES(commission_amount), parts_json = VALUES(parts_json),
                   qbo_invoice_id = VALUES(qbo_invoice_id), invoice_total = VALUES(invoice_total),
                   invoice_balance = VALUES(invoice_balance), status = VALUES(status),
                   paid_date = COALESCE(paid_date, VALUES(paid_date)),
                   payroll_date = COALESCE(payroll_date, VALUES(payroll_date)),
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
            .bind(invoice.map(|i| i.id.clone()))
            .bind(invoice.and_then(|i| i.total_amt))
            .bind(invoice.and_then(|i| i.balance))
            .bind(status)
            .bind(paid_date)
            .bind(payroll_date)
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

/// Commissions to mark paid out.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Payout {
    /// Commission row ids; only paid ones are marked.
    pub ids: Vec<i32>,
}

/// Mark commissions paid out
///
/// Only commissions whose customer paid are marked.
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
    let mut sql = sqlx::QueryBuilder::new(
        "UPDATE quote_commissions SET paid_out_at = NOW() WHERE status = 'paid' AND company_id = ",
    );
    sql.push_bind(user.company_id).push(" AND id IN (");
    let mut ids = sql.separated(", ");
    for id in input.ids {
        ids.push_bind(id);
    }
    sql.push(")");
    sql.build().execute(&state.pool).await?;
    Ok(StatusCode::NO_CONTENT)
}
