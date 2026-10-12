//! `GET /v1/ai-usage/feature`: one AI feature's usage for the super-user
//! analytics dashboard. Replaces the Remix `api.analytics.featureDetail`.

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
    OpenApiRouter::new().routes(routes!(feature))
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
