//! The super-user AI usage dashboard.
//!
//! - `GET /v1/ai-usage/overview` → spend, usage by feature, unpriced models
//!   and a page of calls
//! - `GET /v1/ai-usage/feature`  → one feature's usage
//!
//! Replaces the `admin.superuser-dashboard.analytics` loader and the Remix
//! `api.analytics.featureDetail`.

use crate::auth::CurrentUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Query, State};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(overview))
        .routes(routes!(feature))
}

/// Page sizes the calls table offers.
const PAGE_SIZES: [i64; 6] = [10, 20, 50, 100, 200, 500];

/// Query of `GET /v1/ai-usage/overview`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct OverviewQuery {
    /// First day of the range.
    pub from_date: NaiveDate,
    /// Last day of the range.
    pub to_date: NaiveDate,
    /// Only this company.
    pub company_id: Option<i32>,
    /// Calls page, from 1 (past the last page: the last page).
    pub page: Option<i64>,
    /// Calls per page: 10, 20 (default), 50, 100, 200 or 500.
    pub page_size: Option<i64>,
}

/// Spend totals (USD).
#[derive(Debug, Serialize, ToSchema)]
pub struct OverviewKpis {
    /// This month so far.
    pub mtd_usd: f64,
    /// Last month up to the same day.
    pub prev_mtd_usd: f64,
    /// Ever.
    pub all_time_usd: f64,
    /// In the range.
    pub range_usd: f64,
    /// Calls in the range.
    pub range_requests: i64,
    /// Share of range calls that succeeded; null without calls.
    pub range_success_rate: Option<f64>,
}

/// A feature's usage in the range.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct OverviewFeature {
    /// Feature key.
    pub feature: String,
    /// Calls.
    pub requests: i64,
    /// Cost (USD).
    pub cost_usd: f64,
    /// Share that succeeded.
    pub success_rate: Option<f64>,
    /// Models it used, by name.
    #[sqlx(skip)]
    pub models: Vec<String>,
}

/// A company to filter by.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct OverviewCompany {
    /// Company id.
    pub id: i32,
    /// Name.
    pub name: Option<String>,
}

/// A model with free-looking usage and no price.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct UnpricedModel {
    /// Model name.
    pub model: String,
    /// Successful calls logged at $0.
    pub calls: i64,
}

/// A logged AI call.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct OverviewCall {
    /// The log row.
    pub id: i64,
    /// Feature key.
    pub feature: String,
    /// Model used.
    pub model: String,
    /// `chat`, `image` or `transcription`.
    pub api_kind: String,
    /// Who made it; null for background jobs.
    pub user_name: Option<String>,
    /// Their company.
    pub company_name: Option<String>,
    /// Input tokens.
    pub input_tokens: Option<i64>,
    /// Output tokens.
    pub output_tokens: Option<i64>,
    /// Cost (USD).
    pub cost_usd: f64,
    /// It succeeded.
    pub success: bool,
    /// How long it took.
    pub duration_ms: Option<i64>,
    /// When.
    pub created_at: DateTime<Utc>,
}

/// `GET /v1/ai-usage/overview`.
#[derive(Debug, Serialize, ToSchema)]
pub struct UsageOverview {
    /// Spend totals.
    pub kpis: OverviewKpis,
    /// Features with calls in the range, by key.
    pub features: Vec<OverviewFeature>,
    /// Live companies, by name.
    pub companies: Vec<OverviewCompany>,
    /// Models used for free without a price row.
    pub unpriced_models: Vec<UnpricedModel>,
    /// The page of calls, newest first.
    pub calls: Vec<OverviewCall>,
    /// Calls in the range.
    pub total_calls: i64,
    /// The page shown.
    pub page: i64,
    /// Calls per page.
    pub page_size: i64,
}

/// Calls in the range: `DATE(l.created_at)` within the days, optionally one company.
const RANGE: &str =
    "DATE(l.created_at) >= ? AND DATE(l.created_at) <= ? AND (? = 0 OR l.company_id = ?)";

async fn kpis(
    pool: &sqlx::MySqlPool,
    company: i32,
    from: NaiveDate,
    to: NaiveDate,
) -> sqlx::Result<OverviewKpis> {
    let range = RANGE;
    let total = |sql: &'static str| async move {
        sqlx::query_scalar::<_, Option<f64>>(sql)
            .bind(company)
            .bind(company)
            .fetch_one(pool)
            .await
            .map(Option::unwrap_or_default)
    };
    let mtd_usd = total(
        "SELECT CAST(SUM(cost_usd) AS DOUBLE) FROM ai_usage_log
          WHERE created_at >= DATE_FORMAT(CURDATE(), '%Y-%m-01') AND (? = 0 OR company_id = ?)",
    )
    .await?;
    let prev_mtd_usd = total(
        "SELECT CAST(SUM(cost_usd) AS DOUBLE) FROM ai_usage_log
          WHERE created_at >= DATE_FORMAT(CURDATE() - INTERVAL 1 MONTH, '%Y-%m-01')
            AND created_at <= NOW() - INTERVAL 1 MONTH AND (? = 0 OR company_id = ?)",
    )
    .await?;
    let all_time_usd = total(
        "SELECT CAST(SUM(cost_usd) AS DOUBLE) FROM ai_usage_log WHERE (? = 0 OR company_id = ?)",
    )
    .await?;
    let (range_requests, range_usd, range_success): (i64, Option<f64>, Option<f64>) =
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT COUNT(*), CAST(SUM(l.cost_usd) AS DOUBLE), CAST(AVG(l.success) AS DOUBLE)
               FROM ai_usage_log l WHERE {range}"
        )))
        .bind(from)
        .bind(to)
        .bind(company)
        .bind(company)
        .fetch_one(pool)
        .await?;
    Ok(OverviewKpis {
        mtd_usd,
        prev_mtd_usd,
        all_time_usd,
        range_usd: range_usd.unwrap_or_default(),
        range_requests,
        range_success_rate: (range_requests > 0).then(|| range_success.unwrap_or(1.0)),
    })
}

async fn features(
    pool: &sqlx::MySqlPool,
    company: i32,
    from: NaiveDate,
    to: NaiveDate,
) -> sqlx::Result<Vec<OverviewFeature>> {
    let range = RANGE;
    let mut features: Vec<OverviewFeature> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT l.feature, COUNT(*) AS requests, CAST(COALESCE(SUM(l.cost_usd), 0) AS DOUBLE) AS cost_usd,
                CAST(AVG(l.success) AS DOUBLE) AS success_rate
           FROM ai_usage_log l WHERE {range} GROUP BY l.feature ORDER BY l.feature"
    )))
    .bind(from)
    .bind(to)
    .bind(company)
    .bind(company)
    .fetch_all(pool)
    .await?;
    let models: Vec<(String, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT DISTINCT l.feature, l.model FROM ai_usage_log l WHERE {range} ORDER BY l.model"
    )))
    .bind(from)
    .bind(to)
    .bind(company)
    .bind(company)
    .fetch_all(pool)
    .await?;
    for (feature, model) in models {
        if let Some(row) = features.iter_mut().find(|row| row.feature == feature) {
            row.models.push(model);
        }
    }
    Ok(features)
}

/// AI usage overview
///
/// Spend this month, last month to date, ever and in the range; calls, cost,
/// success and models by feature; models logged at $0 without a price; and a
/// page of the range's calls. Optionally one company. Super users only.
#[utoipa::path(
    get,
    path = "/v1/ai-usage/overview",
    operation_id = "get_ai_usage_overview",
    tag = "ai-usage",
    params(OverviewQuery),
    responses(
        (status = 200, description = "Usage", body = UsageOverview),
        (status = 400, description = "Missing or invalid days", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a super user", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn overview(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(query): Query<OverviewQuery>,
) -> ApiResult<Json<UsageOverview>> {
    if !user.is_superuser {
        return Err(ApiError::Forbidden);
    }
    let pool = &state.pool;
    let company = query.company_id.unwrap_or(0);
    let (from, to) = (query.from_date, query.to_date);
    let range = RANGE;
    let kpis = kpis(pool, company, from, to).await?;
    let features = features(pool, company, from, to).await?;
    let companies: Vec<OverviewCompany> =
        sqlx::query_as("SELECT id, name FROM company WHERE deleted_at IS NULL ORDER BY name, id")
            .fetch_all(pool)
            .await?;
    // Advisory: a missing pricing table only hides the list.
    let unpriced_models: Vec<UnpricedModel> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT l.model, COUNT(*) AS calls
           FROM ai_usage_log l
           LEFT JOIN (SELECT DISTINCT model FROM ai_model_pricing WHERE effective_from <= CURDATE()) p
             ON p.model = l.model
          WHERE {range} AND l.success = 1 AND l.cost_usd = 0 AND p.model IS NULL
            AND (COALESCE(l.input_tokens, 0) > 0 OR COALESCE(l.output_tokens, 0) > 0
              OR COALESCE(l.audio_seconds, 0) > 0 OR COALESCE(l.image_count, 0) > 0)
          GROUP BY l.model ORDER BY l.model"
    )))
    .bind(from)
    .bind(to)
    .bind(company)
    .bind(company)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let page_size = query
        .page_size
        .filter(|size| PAGE_SIZES.contains(size))
        .unwrap_or(20);
    let pages = ((kpis.range_requests + page_size - 1) / page_size).max(1);
    let page = query.page.filter(|page| *page >= 1).unwrap_or(1).min(pages);
    let calls: Vec<OverviewCall> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT CAST(l.id AS SIGNED) AS id, l.feature, l.model, CAST(l.api_kind AS CHAR) AS api_kind,
                u.name AS user_name, c.name AS company_name,
                CAST(l.input_tokens AS SIGNED) AS input_tokens, CAST(l.output_tokens AS SIGNED) AS output_tokens,
                CAST(l.cost_usd AS DOUBLE) AS cost_usd, l.success <> 0 AS success,
                CAST(l.duration_ms AS SIGNED) AS duration_ms, l.created_at
           FROM ai_usage_log l
           LEFT JOIN users u ON u.id = l.user_id
           LEFT JOIN company c ON c.id = l.company_id
          WHERE {range} ORDER BY l.id DESC LIMIT ? OFFSET ?"
    )))
    .bind(from)
    .bind(to)
    .bind(company)
    .bind(company)
    .bind(page_size)
    .bind((page - 1) * page_size)
    .fetch_all(pool)
    .await?;
    Ok(Json(UsageOverview {
        total_calls: kpis.range_requests,
        kpis,
        features,
        companies,
        unpriced_models,
        calls,
        page,
        page_size,
    }))
}

/// Query of `GET /v1/ai-usage/feature`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct FeatureQuery {
    /// Feature key, up to 50 of `a-z 0-9 _ . : -`.
    pub feature: String,
    /// First day (default: all time).
    pub from_date: Option<NaiveDate>,
    /// Last day.
    pub to_date: Option<NaiveDate>,
    /// Only this company.
    pub company_id: Option<i32>,
    /// Recent calls shown, 1 to 200 (default 20).
    pub call_limit: Option<i64>,
}

/// Totals of the feature in the range.
#[derive(Debug, Serialize, ToSchema)]
pub struct FeatureMetrics {
    /// Calls.
    pub requests: i64,
    /// Share that succeeded, 0 to 1; null without calls.
    pub success_rate: Option<f64>,
    /// Average input tokens.
    pub avg_input_tokens: Option<i64>,
    /// Average output tokens.
    pub avg_output_tokens: Option<i64>,
    /// Average cost per call (USD).
    pub avg_cost_usd: f64,
    /// Total cost (USD).
    pub total_cost_usd: f64,
}

/// One of the five costliest users.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct FeatureUser {
    /// The user; null for background jobs.
    pub user_id: Option<i32>,
    /// Their name.
    pub name: Option<String>,
    /// Calls.
    pub requests: i64,
    /// Cost (USD).
    pub cost_usd: f64,
}

/// A recent call.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct FeatureCall {
    /// The log row.
    pub id: i64,
    /// Model used.
    pub model: String,
    /// `chat`, `image` or `transcription`.
    pub api_kind: String,
    /// Who made it; null for background jobs.
    pub user_name: Option<String>,
    /// Input tokens.
    pub input_tokens: Option<i64>,
    /// Output tokens.
    pub output_tokens: Option<i64>,
    /// Cost (USD).
    pub cost_usd: f64,
    /// It succeeded.
    pub success: bool,
    /// How long it took.
    pub duration_ms: Option<i64>,
    /// When.
    pub created_at: DateTime<Utc>,
}

/// `GET /v1/ai-usage/feature`.
#[derive(Debug, Serialize, ToSchema)]
pub struct FeatureUsage {
    /// Totals.
    pub metrics: FeatureMetrics,
    /// The five costliest users.
    pub top_users: Vec<FeatureUser>,
    /// Newest calls first.
    pub recent_calls: Vec<FeatureCall>,
    /// More calls than `call_limit` (under 200).
    pub has_more_recent_calls: bool,
}

#[derive(sqlx::FromRow)]
struct MetricsRow {
    requests: i64,
    success_rate: Option<f64>,
    avg_in: Option<f64>,
    avg_out: Option<f64>,
    avg_cost: Option<f64>,
    total_cost: Option<f64>,
}

/// Token averages are far inside `i64`.
#[allow(clippy::cast_possible_truncation)]
const fn rounded(value: f64) -> i64 {
    value.round() as i64
}

fn valid_key(key: &str) -> bool {
    (1..=50).contains(&key.len())
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'))
}

/// Get one AI feature's usage
///
/// Totals, the five costliest users and the latest calls of a feature in
/// a date range (optionally one company). Super users only.
#[utoipa::path(
    get,
    path = "/v1/ai-usage/feature",
    operation_id = "get_ai_feature_usage",
    tag = "ai-usage",
    params(FeatureQuery),
    responses(
        (status = 200, description = "Usage", body = FeatureUsage),
        (status = 400, description = "Not a feature key", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a super user", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn feature(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(query): Query<FeatureQuery>,
) -> ApiResult<Json<FeatureUsage>> {
    if !user.is_superuser {
        return Err(ApiError::Forbidden);
    }
    if !valid_key(&query.feature) {
        return Err(ApiError::BadRequest("Unknown feature".into()));
    }
    let limit = query
        .call_limit
        .filter(|limit| (1..=200).contains(limit))
        .unwrap_or(20);
    let from = query
        .from_date
        .unwrap_or_else(|| NaiveDate::from_ymd_opt(1970, 1, 1).unwrap_or_default());
    let to = query
        .to_date
        .unwrap_or_else(|| NaiveDate::from_ymd_opt(9999, 12, 31).unwrap_or_default());
    let company = query.company_id.unwrap_or(0);
    let filter = "l.feature = ? AND DATE(l.created_at) >= ? AND DATE(l.created_at) <= ? AND (? = 0 OR l.company_id = ?)";
    let metrics: MetricsRow = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT COUNT(*) AS requests, CAST(AVG(l.success) AS DOUBLE) AS success_rate,
                CAST(AVG(l.input_tokens) AS DOUBLE) AS avg_in, CAST(AVG(l.output_tokens) AS DOUBLE) AS avg_out,
                CAST(AVG(l.cost_usd) AS DOUBLE) AS avg_cost, CAST(SUM(l.cost_usd) AS DOUBLE) AS total_cost
           FROM ai_usage_log l WHERE {filter}"
    )))
    .bind(&query.feature)
    .bind(from)
    .bind(to)
    .bind(company)
    .bind(company)
    .fetch_one(&state.pool)
    .await?;
    let top_users: Vec<FeatureUser> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT l.user_id, u.name, COUNT(*) AS requests, CAST(COALESCE(SUM(l.cost_usd), 0) AS DOUBLE) AS cost_usd
           FROM ai_usage_log l LEFT JOIN users u ON u.id = l.user_id
          WHERE {filter} GROUP BY l.user_id, u.name ORDER BY SUM(l.cost_usd) DESC LIMIT 5"
    )))
    .bind(&query.feature)
    .bind(from)
    .bind(to)
    .bind(company)
    .bind(company)
    .fetch_all(&state.pool)
    .await?;
    let mut recent_calls: Vec<FeatureCall> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT CAST(l.id AS SIGNED) AS id, l.model, CAST(l.api_kind AS CHAR) AS api_kind, u.name AS user_name,
                CAST(l.input_tokens AS SIGNED) AS input_tokens, CAST(l.output_tokens AS SIGNED) AS output_tokens,
                CAST(l.cost_usd AS DOUBLE) AS cost_usd, l.success <> 0 AS success,
                CAST(l.duration_ms AS SIGNED) AS duration_ms, l.created_at
           FROM ai_usage_log l LEFT JOIN users u ON u.id = l.user_id
          WHERE {filter} ORDER BY l.id DESC LIMIT ?"
    )))
    .bind(&query.feature)
    .bind(from)
    .bind(to)
    .bind(company)
    .bind(company)
    .bind(limit + 1)
    .fetch_all(&state.pool)
    .await?;
    let has_more = limit < 200 && i64::try_from(recent_calls.len()).unwrap_or(0) > limit;
    recent_calls.truncate(usize::try_from(limit).unwrap_or(20));
    let round = |value: Option<f64>| value.map(rounded);
    Ok(Json(FeatureUsage {
        metrics: FeatureMetrics {
            requests: metrics.requests,
            success_rate: (metrics.requests > 0).then(|| metrics.success_rate.unwrap_or(1.0)),
            avg_input_tokens: round(metrics.avg_in)
                .filter(|_| metrics.avg_in.is_some_and(|v| v != 0.0)),
            avg_output_tokens: round(metrics.avg_out)
                .filter(|_| metrics.avg_out.is_some_and(|v| v != 0.0)),
            avg_cost_usd: metrics.avg_cost.unwrap_or(0.0),
            total_cost_usd: metrics.total_cost.unwrap_or(0.0),
        },
        top_users,
        recent_calls,
        has_more_recent_calls: has_more,
    }))
}
