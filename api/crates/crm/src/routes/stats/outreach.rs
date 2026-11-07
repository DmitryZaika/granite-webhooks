//! The dashboard's Outreach activity widget.
//!
//! The calls, texts and emails the signed-in user made today (the company's
//! time zone), against the daily minimums a Manager or Sales Manager sets.
//!
//! - `GET /v1/me/stats/outreach`      → today's counts and the minimums
//! - `PUT /v1/company/outreach-goals` → set the minimums

use super::schemas::SmsProvider;
use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::phone::{clock, company};
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use chrono::{DateTime, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};
// Dynamic SQL here only splices fixed table and column names.
use sqlx::{AssertSqlSafe, FromRow, MySqlPool};

/// `positions.id` of a Sales Manager and a Manager.
const GOAL_SETTER_POSITIONS: [i32; 2] = [2, 5];

/// The columns are SMALLINT UNSIGNED; nobody makes 10,000 calls a day.
const MAX_GOAL: u16 = 9999;

/// The company's daily minimums; null when not set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, FromRow, ToSchema)]
pub struct OutreachGoals {
    pub calls: Option<u16>,
    pub sms: Option<u16>,
    pub emails: Option<u16>,
}

/// Today's outreach of the signed-in user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct Outreach {
    /// Outbound calls started today, on every phone provider.
    pub calls: i64,
    /// Texts sent today through the user's SMS provider.
    pub sms: i64,
    /// Emails the user sent today; automated emails left out.
    pub emails: i64,
    pub goals: OutreachGoals,
    /// The caller is a Manager or Sales Manager and may set the minimums.
    pub can_set_goals: bool,
}

async fn goals(pool: &MySqlPool, company_id: i32) -> sqlx::Result<OutreachGoals> {
    let row: Option<OutreachGoals> = sqlx::query_as(
        "SELECT daily_calls_goal AS calls, daily_sms_goal AS sms, daily_emails_goal AS emails
           FROM company WHERE id = ?",
    )
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.unwrap_or_default())
}

async fn sets_goals(pool: &MySqlPool, user_id: i32, company_id: i32) -> sqlx::Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM users_positions
                       WHERE user_id = ? AND company_id = ? AND position_id IN (?, ?))",
    )
    .bind(user_id)
    .bind(company_id)
    .bind(GOAL_SETTER_POSITIONS[0])
    .bind(GOAL_SETTER_POSITIONS[1])
    .fetch_one(pool)
    .await
}

const fn naive(at: DateTime<Utc>) -> NaiveDateTime {
    at.naive_utc()
}

/// Today in the company's time zone, as UTC instants `[from, to)`.
#[derive(Clone, Copy)]
struct Day {
    from: NaiveDateTime,
    to: NaiveDateTime,
}

async fn calls_on(pool: &MySqlPool, user_id: i32, company_id: i32, day: Day) -> sqlx::Result<i64> {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM phone_call_points
          WHERE company_id = ? AND user_id = ? AND started_at >= ? AND started_at < ?",
    )
    .bind(company_id)
    .bind(user_id)
    .bind(day.from)
    .bind(day.to)
    .fetch_one(pool)
    .await
}

async fn emails_on(pool: &MySqlPool, user_id: i32, company_id: i32, day: Day) -> sqlx::Result<i64> {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM emails e
          WHERE e.deleted_at IS NULL
            AND e.is_draft = 0
            AND e.is_automated = 0
            AND e.sender_user_id = ?
            AND (e.company_id = ? OR e.company_id IS NULL)
            AND e.sent_at >= ? AND e.sent_at < ?",
    )
    .bind(user_id)
    .bind(company_id)
    .bind(day.from)
    .bind(day.to)
    .fetch_one(pool)
    .await
}

/// Counted as `sent_sms_by_month` counts them.
async fn sms_on(
    pool: &MySqlPool,
    provider: SmsProvider,
    user_id: i32,
    company_id: i32,
    day: Day,
) -> sqlx::Result<i64> {
    let table = provider.sms_table();
    let agent = provider.agent_column().map_or_else(
        || "CAST(u.id AS CHAR)".to_string(),
        |column| format!("u.{column}"),
    );
    let sql = format!(
        "SELECT COUNT(*) FROM {table} cs
          WHERE cs.company_id = ?
            AND cs.direction = 'outbound'
            AND cs.status = 'sent'
            AND cs.created_date >= ? AND cs.created_date < ?
            AND (cs.sender_user_id = ?
                 OR EXISTS (SELECT 1 FROM users u
                             WHERE u.id = ? AND u.is_deleted = 0
                               AND {agent} IS NOT NULL
                               AND {agent} = cs.agent))"
    );
    sqlx::query_scalar(AssertSqlSafe(sql))
        .bind(company_id)
        .bind(day.from)
        .bind(day.to)
        .bind(user_id)
        .bind(user_id)
        .fetch_one(pool)
        .await
}

/// Get my outreach today
///
/// Returns how many outbound calls, texts and emails the signed-in user made
/// today (midnight to midnight in the company's time zone), and the company's
/// daily minimums for each (null when not set).
#[crate::openapi::path(
    get,
    path = "/v1/me/stats/outreach",
    operation_id = "get_my_outreach_today",
    tag = "stats",
    responses(
        (status = 200, description = "Today's calls, texts and emails", body = Outreach),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get_outreach(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<Outreach>> {
    let pool = &state.pool;
    let config = company::load(pool, user.company_id).await?;
    let tz = company::time_zone_of(config.as_ref());
    let now = Utc::now();
    let day = Day {
        from: naive(clock::day_start(tz, now)),
        to: naive(clock::next_day_start(tz, now)),
    };
    let provider = crate::routes::sms::send::user_provider(pool, user.company_id, user.id).await?;
    let sms = match provider {
        Some(provider) => sms_on(pool, provider, user.id, user.company_id, day).await?,
        None => 0,
    };
    Ok(Json(Outreach {
        calls: calls_on(pool, user.id, user.company_id, day).await?,
        sms,
        emails: emails_on(pool, user.id, user.company_id, day).await?,
        goals: goals(pool, user.company_id).await?,
        can_set_goals: user.is_superuser || sets_goals(pool, user.id, user.company_id).await?,
    }))
}

/// Set the daily outreach minimums
///
/// A Manager or Sales Manager sets the minimum calls, texts and emails each
/// rep should make a day. Null (or 0) clears one.
#[crate::openapi::path(
    put,
    path = "/v1/company/outreach-goals",
    operation_id = "set_company_outreach_goals",
    tag = "stats",
    request_body = OutreachGoals,
    responses(
        (status = 200, description = "The minimums as saved", body = OutreachGoals),
        (status = 400, description = "A minimum out of range", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a Manager or Sales Manager", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn set_goals(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(body): JsonBody<OutreachGoals>,
) -> ApiResult<Json<OutreachGoals>> {
    let pool = &state.pool;
    if !user.is_superuser && !sets_goals(pool, user.id, user.company_id).await? {
        return Err(ApiError::Forbidden);
    }
    if [body.calls, body.sms, body.emails]
        .iter()
        .flatten()
        .any(|goal| *goal > MAX_GOAL)
    {
        return Err(ApiError::BadRequest(
            "each minimum must be 9999 or less".into(),
        ));
    }
    // 0 reads as "not set".
    let clean = |goal: Option<u16>| goal.filter(|goal| *goal > 0);
    sqlx::query(
        "UPDATE company SET daily_calls_goal = ?, daily_sms_goal = ?, daily_emails_goal = ?
          WHERE id = ?",
    )
    .bind(clean(body.calls))
    .bind(clean(body.sms))
    .bind(clean(body.emails))
    .bind(user.company_id)
    .execute(pool)
    .await?;
    Ok(Json(goals(pool, user.company_id).await?))
}
