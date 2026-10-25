//! Shop maintenance: equipment (items), its checks (sub-items) with hour or
//! day conditions, and their completions. Everyone of the shop reads and
//! completes; shop managers manage the equipment and checks.
//!
//! - `GET /v1/shop/maintenance` → everything the page shows
//! - `POST /v1/shop/maintenance/items`, `PUT|DELETE .../items/{id}`
//! - `POST /v1/shop/maintenance/checks`, `PUT|DELETE .../checks/{id}`
//! - `POST /v1/shop/maintenance/checks/{id}/complete`
//! - `PUT /v1/shop/maintenance/conditions/{id}/hours`
//!
//! Replaces the Remix `shop.maintenance` loader and action. Due dates are
//! calendar days (UTC), like the server-side Remix code.

use crate::auth::ShopOrEmployeeUser;
use crate::auth::session::SessionUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::openapi::{OpenApiRouter, routes};
use crate::serde_helpers::js_date;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Days, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list))
        .routes(routes!(create_item))
        .routes(routes!(update_item, delete_item))
        .routes(routes!(create_check))
        .routes(routes!(update_check, delete_check))
        .routes(routes!(complete_check))
        .routes(routes!(update_hours))
}

/// `positions.id` of a shop manager.
const SHOP_MANAGER: i32 = 3;

async fn is_shop_manager(state: &AppState, user: &SessionUser) -> Result<bool, sqlx::Error> {
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM users_positions WHERE user_id = ? AND position_id = ? AND company_id = ? LIMIT 1",
    )
    .bind(user.id)
    .bind(SHOP_MANAGER)
    .bind(user.company_id)
    .fetch_optional(&state.pool)
    .await?;
    Ok(found.is_some())
}

async fn require_manager(state: &AppState, user: &SessionUser) -> Result<(), ApiError> {
    if is_shop_manager(state, user).await? {
        Ok(())
    } else {
        Err(ApiError::Forbidden)
    }
}

/// A piece of equipment.
#[derive(Debug, Clone, PartialEq, Serialize, FromRow, ToSchema)]
pub struct Equipment {
    /// Equipment id.
    pub id: i32,
    /// Name.
    pub name: String,
    /// Usage per working day (hours or miles).
    pub hours_per_day: Option<f64>,
    /// Working days per month.
    pub days_per_month: Option<f64>,
    /// Meter reading at the start.
    pub starting_hours: Option<f64>,
    /// `hours` or `miles`.
    pub usage_unit: String,
}

/// A check of a piece of equipment; its usage fields override the equipment's.
#[derive(Debug, Clone, PartialEq, Serialize, FromRow, ToSchema)]
pub struct Check {
    /// Check id.
    pub id: i32,
    /// Its equipment.
    pub maintenance_item_id: i32,
    /// Name.
    pub name: String,
    /// Notes.
    pub notes: Option<String>,
    /// Last done.
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub last_completed_at: Option<DateTime<Utc>>,
    /// Own usage per working day; null uses the equipment's.
    pub hours_per_day: Option<f64>,
    /// Own working days per month.
    pub days_per_month: Option<f64>,
    /// Own meter reading.
    pub starting_hours: Option<f64>,
    /// Own unit; null uses the equipment's.
    pub usage_unit: Option<String>,
}

/// When a check is due.
#[derive(Debug, Clone, PartialEq, Serialize, FromRow, ToSchema)]
pub struct Condition {
    /// Condition id.
    pub id: i32,
    /// Its check.
    pub maintenance_sub_item_id: i32,
    /// `hours` (of usage) or `days`.
    pub condition_type: String,
    /// Every this many hours or days.
    pub interval_value: f64,
    /// Next due day, `YYYY-MM-DD`.
    pub next_due_date: String,
    /// Hours used since the last time (hour conditions).
    pub hours_done: f64,
}

/// A check done.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow, ToSchema)]
pub struct Completion {
    /// Completion id.
    pub id: i32,
    /// The check.
    pub maintenance_sub_item_id: i32,
    /// When.
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub completed_at: Option<DateTime<Utc>>,
    /// Notes (with the meter reading when given).
    pub notes: Option<String>,
    /// Who did it; null when unknown.
    pub completed_by_name: Option<String>,
}

/// Everything the maintenance page shows.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct Maintenance {
    /// The caller may manage equipment and checks (a shop manager).
    pub can_edit: bool,
    /// Equipment by name.
    pub items: Vec<Equipment>,
    /// Checks by name.
    pub checks: Vec<Check>,
    /// Conditions of the checks.
    pub conditions: Vec<Condition>,
    /// Completions, newest first.
    pub completions: Vec<Completion>,
}

/// Shop maintenance
///
/// The company's equipment, checks, their conditions and completions.
#[crate::openapi::path(
    get,
    path = "/v1/shop/maintenance",
    operation_id = "get_shop_maintenance",
    tag = "shop",
    responses(
        (status = 200, description = "The maintenance page", body = Maintenance),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not of the shop or an employee", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
) -> ApiResult<Json<Maintenance>> {
    let pool = &state.pool;
    let company = user.company_id;
    let items = sqlx::query_as(
        "SELECT id, name, CAST(hours_per_day AS DOUBLE) AS hours_per_day,
                CAST(days_per_month AS DOUBLE) AS days_per_month,
                CAST(starting_hours AS DOUBLE) AS starting_hours, usage_unit
           FROM maintenance_items WHERE company_id = ? AND deleted_at IS NULL
          ORDER BY name ASC, id ASC",
    )
    .bind(company)
    .fetch_all(pool)
    .await?;
    let checks = sqlx::query_as(
        "SELECT id, maintenance_item_id, name, notes, last_completed_at,
                CAST(hours_per_day AS DOUBLE) AS hours_per_day,
                CAST(days_per_month AS DOUBLE) AS days_per_month,
                CAST(starting_hours AS DOUBLE) AS starting_hours, usage_unit
           FROM maintenance_sub_items WHERE company_id = ? AND deleted_at IS NULL
          ORDER BY name ASC, id ASC",
    )
    .bind(company)
    .fetch_all(pool)
    .await?;
    let conditions = sqlx::query_as(
        "SELECT id, maintenance_sub_item_id, CAST(condition_type AS CHAR) AS condition_type,
                CAST(interval_value AS DOUBLE) AS interval_value,
                DATE_FORMAT(next_due_date, '%Y-%m-%d') AS next_due_date,
                CAST(hours_done AS DOUBLE) AS hours_done
           FROM maintenance_conditions WHERE company_id = ? AND deleted_at IS NULL
          ORDER BY id ASC",
    )
    .bind(company)
    .fetch_all(pool)
    .await?;
    let completions = sqlx::query_as(
        "SELECT mc.id, mc.maintenance_sub_item_id, mc.completed_at, mc.notes,
                u.name AS completed_by_name
           FROM maintenance_completions mc
           LEFT JOIN users u ON u.id = mc.completed_by_user_id
          WHERE mc.company_id = ? AND mc.maintenance_sub_item_id IS NOT NULL
          ORDER BY mc.completed_at DESC, mc.id DESC",
    )
    .bind(company)
    .fetch_all(pool)
    .await?;
    Ok(Json(Maintenance {
        can_edit: is_shop_manager(&state, &user).await?,
        items,
        checks,
        conditions,
        completions,
    }))
}

/// Usage of a piece of equipment or a check.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
struct Usage {
    hours_per_day: Option<f64>,
    days_per_month: Option<f64>,
}

impl Usage {
    /// Average usage per calendar day, when both numbers are set and positive.
    fn per_calendar_day(self) -> Option<f64> {
        match (self.hours_per_day, self.days_per_month) {
            (Some(hours), Some(days)) if hours > 0.0 && days > 0.0 => Some(hours * days / 30.0),
            _ => None,
        }
    }
}

fn add_days(from: NaiveDate, days: f64) -> NaiveDate {
    // Whole days, a fraction dropped (like the CRM's `Date.setDate`).
    let days = days.max(0.0).trunc();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let days = days as u64;
    from.checked_add_days(Days::new(days)).unwrap_or(from)
}

/// The day an hour condition comes due from `today`, or `None` without usage.
fn hours_due(today: NaiveDate, interval: f64, done: f64, usage: Usage) -> Option<NaiveDate> {
    let remaining = (interval - done).max(0.0);
    if remaining <= 0.0 {
        return Some(today);
    }
    let per_day = usage.per_calendar_day()?;
    let days = (remaining / per_day).ceil();
    (days > 0.0).then(|| add_days(today, days))
}

fn today() -> NaiveDate {
    Utc::now().date_naive()
}

/// Body of creating or saving equipment.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct EquipmentInput {
    /// Name; 1 to 255 characters.
    pub name: String,
    /// Usage per working day, more than 0 (at most 24 hours).
    #[serde(default)]
    pub hours_per_day: Option<f64>,
    /// Working days per month, more than 0, at most 31.
    #[serde(default)]
    pub days_per_month: Option<f64>,
    /// Meter reading, 0 or more.
    #[serde(default)]
    pub starting_hours: Option<f64>,
    /// `hours` (default) or `miles`.
    #[serde(default)]
    pub usage_unit: Option<String>,
}

fn bad(message: &str) -> ApiError {
    ApiError::BadRequest(message.into())
}

fn valid_name(name: &str) -> Result<String, ApiError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(bad("Name is required"));
    }
    if name.chars().count() > 255 {
        return Err(bad("Name must be 255 characters or less"));
    }
    Ok(name.to_string())
}

fn valid_unit(unit: Option<&str>) -> Result<String, ApiError> {
    match unit.map(str::trim) {
        None | Some("" | "hours") => Ok("hours".into()),
        Some("miles") => Ok("miles".into()),
        Some(_) => Err(bad("Usage unit must be hours or miles")),
    }
}

/// Checks the usage numbers of a form (`unit` decides the 24-hour cap).
fn valid_usage(
    hours_per_day: Option<f64>,
    days_per_month: Option<f64>,
    starting_hours: Option<f64>,
    unit: &str,
) -> Result<(), ApiError> {
    if let Some(hours) = hours_per_day {
        if !hours.is_finite() || hours <= 0.0 {
            return Err(bad("Hours per day must be greater than 0"));
        }
        if hours > 100_000.0 {
            return Err(bad("Per-day value is too large"));
        }
        if unit == "hours" && hours > 24.0 {
            return Err(bad("Hours per day cannot exceed 24"));
        }
    }
    if let Some(days) = days_per_month {
        if !days.is_finite() || days <= 0.0 {
            return Err(bad("Days per month must be greater than 0"));
        }
        if days > 31.0 {
            return Err(bad("Days per month cannot exceed 31"));
        }
    }
    if let Some(start) = starting_hours
        && (!start.is_finite() || !(0.0..=1_000_000.0).contains(&start))
    {
        return Err(bad("Starting hours must be between 0 and 1000000"));
    }
    Ok(())
}

fn valid_equipment(input: &EquipmentInput) -> Result<(String, String), ApiError> {
    let name = valid_name(&input.name)?;
    let unit = valid_unit(input.usage_unit.as_deref())?;
    valid_usage(
        input.hours_per_day,
        input.days_per_month,
        input.starting_hours,
        &unit,
    )?;
    Ok((name, unit))
}

/// Add equipment
///
/// Adds a piece of equipment. Shop managers only.
#[crate::openapi::path(
    post,
    path = "/v1/shop/maintenance/items",
    operation_id = "create_maintenance_item",
    tag = "shop",
    request_body = EquipmentInput,
    responses(
        (status = 201, description = "Added"),
        (status = 400, description = "Invalid field", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a shop manager", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create_item(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
    JsonBody(input): JsonBody<EquipmentInput>,
) -> ApiResult<StatusCode> {
    require_manager(&state, &user).await?;
    let (name, unit) = valid_equipment(&input)?;
    sqlx::query(
        "INSERT INTO maintenance_items
           (company_id, name, hours_per_day, days_per_month, starting_hours, usage_unit, created_by_user_id)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(user.company_id)
    .bind(name)
    .bind(input.hours_per_day)
    .bind(input.days_per_month)
    .bind(input.starting_hours)
    .bind(unit)
    .bind(user.id)
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::CREATED)
}

/// Save equipment
///
/// Saves a piece of equipment. Shop managers only.
#[crate::openapi::path(
    put,
    path = "/v1/shop/maintenance/items/{id}",
    operation_id = "update_maintenance_item",
    tag = "shop",
    params(("id" = i32, Path, description = "Equipment id")),
    request_body = EquipmentInput,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Invalid field", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a shop manager", body = ErrorBody),
        (status = 404, description = "No such equipment", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update_item(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<EquipmentInput>,
) -> ApiResult<StatusCode> {
    require_manager(&state, &user).await?;
    let (name, unit) = valid_equipment(&input)?;
    item_exists(&state, user.company_id, id).await?;
    sqlx::query(
        "UPDATE maintenance_items SET name = ?, hours_per_day = ?, days_per_month = ?,
                starting_hours = ?, usage_unit = ?, updated_at = CURRENT_TIMESTAMP
          WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(name)
    .bind(input.hours_per_day)
    .bind(input.days_per_month)
    .bind(input.starting_hours)
    .bind(unit)
    .bind(id)
    .bind(user.company_id)
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn item_exists(state: &AppState, company_id: i32, id: i32) -> Result<(), ApiError> {
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM maintenance_items WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(company_id)
    .fetch_optional(&state.pool)
    .await?;
    found.map(|_| ()).ok_or(ApiError::NotFound)
}

/// Delete equipment
///
/// Deletes a piece of equipment with its checks and conditions. Shop
/// managers only.
#[crate::openapi::path(
    delete,
    path = "/v1/shop/maintenance/items/{id}",
    operation_id = "delete_maintenance_item",
    tag = "shop",
    params(("id" = i32, Path, description = "Equipment id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a shop manager", body = ErrorBody),
        (status = 404, description = "No such equipment", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_item(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    require_manager(&state, &user).await?;
    item_exists(&state, user.company_id, id).await?;
    let mut tx = state.pool.begin().await?;
    sqlx::query(
        "UPDATE maintenance_conditions mc
           JOIN maintenance_sub_items si ON si.id = mc.maintenance_sub_item_id
            SET mc.deleted_at = CURRENT_TIMESTAMP, mc.updated_at = CURRENT_TIMESTAMP
          WHERE si.maintenance_item_id = ? AND si.company_id = ? AND mc.deleted_at IS NULL",
    )
    .bind(id)
    .bind(user.company_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE maintenance_sub_items SET deleted_at = CURRENT_TIMESTAMP, updated_at = CURRENT_TIMESTAMP
          WHERE maintenance_item_id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(user.company_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE maintenance_items SET deleted_at = CURRENT_TIMESTAMP, updated_at = CURRENT_TIMESTAMP
          WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(user.company_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// A condition of a check being saved.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ConditionInput {
    /// `hours` or `days`.
    #[serde(rename = "type")]
    #[schema(rename = "type")]
    pub kind: String,
    /// Every this many hours or days; more than 0.
    pub interval_value: f64,
}

/// Body of creating or saving a check.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckInput {
    /// Its equipment (creating only; ignored when saving).
    pub maintenance_item_id: i32,
    /// Name; 1 to 255 characters.
    pub name: String,
    /// Notes; at most 5000 characters.
    #[serde(default)]
    pub notes: Option<String>,
    /// Own usage per working day; null uses the equipment's.
    #[serde(default)]
    pub hours_per_day: Option<f64>,
    /// Own working days per month.
    #[serde(default)]
    pub days_per_month: Option<f64>,
    /// Own meter reading.
    #[serde(default)]
    pub starting_hours: Option<f64>,
    /// Own unit, `hours` (default) or `miles`.
    #[serde(default)]
    pub usage_unit: Option<String>,
    /// At least one; at most one `hours` and one `days`. Saving replaces
    /// the conditions (their counts start over).
    pub conditions: Vec<ConditionInput>,
}

/// Usage numbers of a piece of equipment.
async fn equipment_usage(
    state: &AppState,
    company_id: i32,
    id: i32,
) -> Result<Option<Usage>, sqlx::Error> {
    let row: Option<(Option<f64>, Option<f64>)> = sqlx::query_as(
        "SELECT CAST(hours_per_day AS DOUBLE), CAST(days_per_month AS DOUBLE)
           FROM maintenance_items WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(company_id)
    .fetch_optional(&state.pool)
    .await?;
    Ok(row.map(|(hours_per_day, days_per_month)| Usage {
        hours_per_day,
        days_per_month,
    }))
}

/// The check's own numbers where set, else the equipment's.
const fn resolve(own: Usage, equipment: Usage) -> Usage {
    Usage {
        hours_per_day: match own.hours_per_day {
            Some(value) => Some(value),
            None => equipment.hours_per_day,
        },
        days_per_month: match own.days_per_month {
            Some(value) => Some(value),
            None => equipment.days_per_month,
        },
    }
}

const HOURS_NEED_USAGE: &str = "Set hours/day and days/month on this check or on the equipment before adding hour-based conditions";

/// Checks a check's form; returns its name, notes and unit.
fn valid_check(input: &CheckInput) -> Result<(String, Option<String>, String), ApiError> {
    let name = valid_name(&input.name)?;
    let notes = input
        .notes
        .as_deref()
        .map(str::trim)
        .filter(|notes| !notes.is_empty())
        .map(str::to_string);
    if notes
        .as_ref()
        .is_some_and(|notes| notes.chars().count() > 5000)
    {
        return Err(bad("Notes must be 5000 characters or less"));
    }
    let unit = valid_unit(input.usage_unit.as_deref())?;
    valid_usage(
        input.hours_per_day,
        input.days_per_month,
        input.starting_hours,
        &unit,
    )?;
    if input.conditions.is_empty() {
        return Err(bad("Add at least one condition"));
    }
    let mut kinds = Vec::new();
    for condition in &input.conditions {
        if condition.kind != "hours" && condition.kind != "days" {
            return Err(bad("Condition type must be hours or days"));
        }
        if !condition.interval_value.is_finite() || condition.interval_value <= 0.0 {
            return Err(bad("Interval must be greater than 0"));
        }
        if kinds.contains(&condition.kind.as_str()) {
            return Err(bad("At most one hours and one days condition"));
        }
        kinds.push(condition.kind.as_str());
    }
    Ok((name, notes, unit))
}

/// Writes the check's conditions, due from today.
async fn insert_conditions(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    company_id: i32,
    check_id: i32,
    conditions: &[ConditionInput],
    usage: Usage,
) -> Result<(), ApiError> {
    let today = today();
    for condition in conditions {
        let due = if condition.kind == "days" {
            add_days(today, condition.interval_value)
        } else {
            hours_due(today, condition.interval_value, 0.0, usage)
                .ok_or_else(|| bad("Could not calculate due date"))?
        };
        sqlx::query(
            "INSERT INTO maintenance_conditions
               (company_id, maintenance_sub_item_id, condition_type, interval_value, next_due_date, hours_done)
             VALUES (?, ?, ?, ?, ?, 0)",
        )
        .bind(company_id)
        .bind(check_id)
        .bind(&condition.kind)
        .bind(condition.interval_value)
        .bind(due)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// Response of adding a check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
pub struct CreatedCheck {
    /// Check id.
    pub id: i32,
}

/// Add a check
///
/// Adds a check to a piece of equipment with its conditions, due from
/// today. Hour conditions need usage per day and days per month (the
/// check's own or the equipment's). Shop managers only.
#[crate::openapi::path(
    post,
    path = "/v1/shop/maintenance/checks",
    operation_id = "create_maintenance_check",
    tag = "shop",
    request_body = CheckInput,
    responses(
        (status = 201, description = "Added", body = CreatedCheck),
        (status = 400, description = "Invalid field or no usage for hour conditions", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a shop manager", body = ErrorBody),
        (status = 404, description = "No such equipment", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create_check(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
    JsonBody(input): JsonBody<CheckInput>,
) -> ApiResult<(StatusCode, Json<CreatedCheck>)> {
    require_manager(&state, &user).await?;
    let (name, notes, unit) = valid_check(&input)?;
    let equipment = equipment_usage(&state, user.company_id, input.maintenance_item_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let usage = resolve(
        Usage {
            hours_per_day: input.hours_per_day,
            days_per_month: input.days_per_month,
        },
        equipment,
    );
    if input.conditions.iter().any(|c| c.kind == "hours") && usage.per_calendar_day().is_none() {
        return Err(bad(HOURS_NEED_USAGE));
    }
    let mut tx = state.pool.begin().await?;
    let id = sqlx::query(
        "INSERT INTO maintenance_sub_items
           (company_id, maintenance_item_id, name, notes, hours_per_day, days_per_month,
            starting_hours, usage_unit, created_by_user_id)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(user.company_id)
    .bind(input.maintenance_item_id)
    .bind(name)
    .bind(notes)
    .bind(input.hours_per_day)
    .bind(input.days_per_month)
    .bind(input.starting_hours)
    .bind(unit)
    .bind(user.id)
    .execute(&mut *tx)
    .await?
    .last_insert_id();
    let id = i32::try_from(id).unwrap_or(i32::MAX);
    insert_conditions(&mut tx, user.company_id, id, &input.conditions, usage).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(CreatedCheck { id })))
}

/// The check's equipment, when the check is live.
async fn check_equipment(state: &AppState, company_id: i32, id: i32) -> Result<i32, ApiError> {
    let equipment: Option<i32> = sqlx::query_scalar(
        "SELECT si.maintenance_item_id FROM maintenance_sub_items si
           JOIN maintenance_items mi ON mi.id = si.maintenance_item_id AND mi.deleted_at IS NULL
          WHERE si.id = ? AND si.company_id = ? AND si.deleted_at IS NULL",
    )
    .bind(id)
    .bind(company_id)
    .fetch_optional(&state.pool)
    .await?;
    equipment.ok_or(ApiError::NotFound)
}

/// Save a check
///
/// Saves a check and replaces its conditions (due again from today). Shop
/// managers only.
#[crate::openapi::path(
    put,
    path = "/v1/shop/maintenance/checks/{id}",
    operation_id = "update_maintenance_check",
    tag = "shop",
    params(("id" = i32, Path, description = "Check id")),
    request_body = CheckInput,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Invalid field or no usage for hour conditions", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a shop manager", body = ErrorBody),
        (status = 404, description = "No such check", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update_check(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<CheckInput>,
) -> ApiResult<StatusCode> {
    require_manager(&state, &user).await?;
    let (name, notes, unit) = valid_check(&input)?;
    let equipment_id = check_equipment(&state, user.company_id, id).await?;
    let equipment = equipment_usage(&state, user.company_id, equipment_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let usage = resolve(
        Usage {
            hours_per_day: input.hours_per_day,
            days_per_month: input.days_per_month,
        },
        equipment,
    );
    if input.conditions.iter().any(|c| c.kind == "hours") && usage.per_calendar_day().is_none() {
        return Err(bad(HOURS_NEED_USAGE));
    }
    let mut tx = state.pool.begin().await?;
    sqlx::query(
        "UPDATE maintenance_sub_items SET name = ?, notes = ?, hours_per_day = ?, days_per_month = ?,
                starting_hours = ?, usage_unit = ?, updated_at = CURRENT_TIMESTAMP
          WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(name)
    .bind(notes)
    .bind(input.hours_per_day)
    .bind(input.days_per_month)
    .bind(input.starting_hours)
    .bind(unit)
    .bind(id)
    .bind(user.company_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE maintenance_conditions SET deleted_at = CURRENT_TIMESTAMP, updated_at = CURRENT_TIMESTAMP
          WHERE maintenance_sub_item_id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(user.company_id)
    .execute(&mut *tx)
    .await?;
    insert_conditions(&mut tx, user.company_id, id, &input.conditions, usage).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Delete a check
///
/// Deletes a check and its conditions. Shop managers only.
#[crate::openapi::path(
    delete,
    path = "/v1/shop/maintenance/checks/{id}",
    operation_id = "delete_maintenance_check",
    tag = "shop",
    params(("id" = i32, Path, description = "Check id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a shop manager", body = ErrorBody),
        (status = 404, description = "No such check", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_check(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    require_manager(&state, &user).await?;
    check_equipment(&state, user.company_id, id).await?;
    let mut tx = state.pool.begin().await?;
    sqlx::query(
        "UPDATE maintenance_conditions SET deleted_at = CURRENT_TIMESTAMP, updated_at = CURRENT_TIMESTAMP
          WHERE maintenance_sub_item_id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(user.company_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE maintenance_sub_items SET deleted_at = CURRENT_TIMESTAMP, updated_at = CURRENT_TIMESTAMP
          WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(user.company_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Body of completing a check.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CompletionInput {
    /// Notes; at most 5000 characters.
    #[serde(default)]
    pub notes: Option<String>,
    /// The meter reading now (checks with an hour condition); becomes the
    /// starting reading of the check (if it has its own usage) or of the
    /// equipment.
    #[serde(default)]
    pub total_hours_of_work: Option<f64>,
}

#[derive(FromRow)]
struct CheckUsageRow {
    maintenance_item_id: i32,
    item_hours_per_day: Option<f64>,
    item_days_per_month: Option<f64>,
    item_unit: Option<String>,
    own_hours_per_day: Option<f64>,
    own_days_per_month: Option<f64>,
    own_starting_hours: Option<f64>,
    own_unit: Option<String>,
}

impl CheckUsageRow {
    const fn usage(&self) -> Usage {
        resolve(
            Usage {
                hours_per_day: self.own_hours_per_day,
                days_per_month: self.own_days_per_month,
            },
            Usage {
                hours_per_day: self.item_hours_per_day,
                days_per_month: self.item_days_per_month,
            },
        )
    }

    const fn has_own_usage(&self) -> bool {
        self.own_hours_per_day.is_some()
            || self.own_days_per_month.is_some()
            || self.own_starting_hours.is_some()
    }

    fn unit(&self) -> &str {
        self.own_unit
            .as_deref()
            .or(self.item_unit.as_deref())
            .unwrap_or("hours")
    }
}

/// A check's usage numbers and its equipment.
async fn check_usage(
    state: &AppState,
    company_id: i32,
    id: i32,
) -> Result<CheckUsageRow, ApiError> {
    sqlx::query_as(
        "SELECT si.maintenance_item_id,
                CAST(mi.hours_per_day AS DOUBLE) AS item_hours_per_day,
                CAST(mi.days_per_month AS DOUBLE) AS item_days_per_month,
                mi.usage_unit AS item_unit,
                CAST(si.hours_per_day AS DOUBLE) AS own_hours_per_day,
                CAST(si.days_per_month AS DOUBLE) AS own_days_per_month,
                CAST(si.starting_hours AS DOUBLE) AS own_starting_hours,
                si.usage_unit AS own_unit
           FROM maintenance_sub_items si
           JOIN maintenance_items mi ON mi.id = si.maintenance_item_id
          WHERE si.id = ? AND si.company_id = ? AND si.deleted_at IS NULL AND mi.deleted_at IS NULL",
    )
    .bind(id)
    .bind(company_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or(ApiError::NotFound)
}

/// The completion's notes: the meter reading, then what was written.
fn completion_notes(notes: Option<&str>, reading: Option<f64>, unit: &str) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(reading) = reading {
        let label = if unit == "miles" {
            "Total miles"
        } else {
            "Total hours of work"
        };
        parts.push(format!("{label}: {reading}"));
    }
    if let Some(notes) = notes {
        parts.push(notes.to_string());
    }
    (!parts.is_empty()).then(|| parts.join("\n"))
}

/// Records the completion and keeps the meter reading as the new start.
async fn save_completion(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    user: &SessionUser,
    id: i32,
    check: &CheckUsageRow,
    notes: Option<String>,
    reading: Option<f64>,
) -> Result<(), sqlx::Error> {
    let now = Utc::now().naive_utc();
    sqlx::query(
        "INSERT INTO maintenance_completions
           (maintenance_item_id, maintenance_sub_item_id, completed_by_user_id, completed_at, notes, company_id)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(check.maintenance_item_id)
    .bind(id)
    .bind(user.id)
    .bind(now)
    .bind(notes)
    .bind(user.company_id)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "UPDATE maintenance_sub_items SET last_completed_at = ?, updated_at = CURRENT_TIMESTAMP
          WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(now)
    .bind(id)
    .bind(user.company_id)
    .execute(&mut **tx)
    .await?;
    let Some(reading) = reading else {
        return Ok(());
    };
    let (table, target) = if check.has_own_usage() {
        ("maintenance_sub_items", id)
    } else {
        ("maintenance_items", check.maintenance_item_id)
    };
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE {table} SET starting_hours = ?, updated_at = CURRENT_TIMESTAMP
          WHERE id = ? AND company_id = ? AND deleted_at IS NULL"
    )))
    .bind(reading)
    .bind(target)
    .bind(user.company_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Mark a check done
///
/// Records a completion, starts every condition of the check over from
/// today and, with a meter reading, saves it as the new starting reading.
#[crate::openapi::path(
    post,
    path = "/v1/shop/maintenance/checks/{id}/complete",
    operation_id = "complete_maintenance_check",
    tag = "shop",
    params(("id" = i32, Path, description = "Check id")),
    request_body = CompletionInput,
    responses(
        (status = 204, description = "Recorded"),
        (status = 400, description = "Invalid field, or hour conditions without usage numbers", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not of the shop or an employee", body = ErrorBody),
        (status = 404, description = "No such check", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn complete_check(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<CompletionInput>,
) -> ApiResult<StatusCode> {
    let notes = input
        .notes
        .as_deref()
        .map(str::trim)
        .filter(|notes| !notes.is_empty());
    if notes.is_some_and(|notes| notes.chars().count() > 5000) {
        return Err(bad("Notes must be 5000 characters or less"));
    }
    if input
        .total_hours_of_work
        .is_some_and(|hours| !hours.is_finite() || !(0.0..=1_000_000.0).contains(&hours))
    {
        return Err(bad("Hours must be between 0 and 1000000"));
    }
    let check = check_usage(&state, user.company_id, id).await?;
    let usage = check.usage();
    let conditions: Vec<(i32, String, f64)> = sqlx::query_as(
        "SELECT id, CAST(condition_type AS CHAR), CAST(interval_value AS DOUBLE)
           FROM maintenance_conditions
          WHERE company_id = ? AND maintenance_sub_item_id = ? AND deleted_at IS NULL",
    )
    .bind(user.company_id)
    .bind(id)
    .fetch_all(&state.pool)
    .await?;
    let has_hours = conditions.iter().any(|(_, kind, _)| kind == "hours");
    let reading = if has_hours {
        input.total_hours_of_work
    } else {
        None
    };
    let today = today();
    let mut dues = Vec::with_capacity(conditions.len());
    for (condition_id, kind, interval) in &conditions {
        let due = if kind == "days" {
            add_days(today, *interval)
        } else {
            hours_due(today, *interval, 0.0, usage).ok_or_else(|| {
                bad("Set hours/day and days/month on this check or on the equipment before completing hour-based checks")
            })?
        };
        dues.push((*condition_id, due));
    }
    let notes = completion_notes(notes, reading, check.unit());
    let mut tx = state.pool.begin().await?;
    for (condition_id, due) in dues {
        sqlx::query(
            "UPDATE maintenance_conditions SET hours_done = 0, next_due_date = ?, updated_at = CURRENT_TIMESTAMP
              WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
        )
        .bind(due)
        .bind(condition_id)
        .bind(user.company_id)
        .execute(&mut *tx)
        .await?;
    }
    save_completion(&mut tx, &user, id, &check, notes, reading).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Body of `PUT /v1/shop/maintenance/conditions/{id}/hours`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct HoursInput {
    /// Hours used since the check was last done; 0 to 100000 (capped at
    /// the interval).
    pub hours_done: f64,
}

/// Record hours used
///
/// Sets the hours used on an hour condition and moves its due day.
#[crate::openapi::path(
    put,
    path = "/v1/shop/maintenance/conditions/{id}/hours",
    operation_id = "update_maintenance_hours",
    tag = "shop",
    params(("id" = i32, Path, description = "Condition id")),
    request_body = HoursInput,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Not an hour condition, invalid hours, or no usage numbers", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not of the shop or an employee", body = ErrorBody),
        (status = 404, description = "No such condition", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update_hours(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<HoursInput>,
) -> ApiResult<StatusCode> {
    if !input.hours_done.is_finite() || !(0.0..=100_000.0).contains(&input.hours_done) {
        return Err(bad("Hours done must be between 0 and 100000"));
    }
    let row: Option<(i32, String, f64)> = sqlx::query_as(
        "SELECT mc.maintenance_sub_item_id, CAST(mc.condition_type AS CHAR),
                CAST(mc.interval_value AS DOUBLE)
           FROM maintenance_conditions mc
          WHERE mc.id = ? AND mc.company_id = ? AND mc.deleted_at IS NULL",
    )
    .bind(id)
    .bind(user.company_id)
    .fetch_optional(&state.pool)
    .await?;
    let (check_id, kind, interval) = row.ok_or(ApiError::NotFound)?;
    if kind != "hours" {
        return Err(bad("Only hour conditions can update hours"));
    }
    let usage = check_usage(&state, user.company_id, check_id)
        .await?
        .usage();
    let done = input.hours_done.min(interval);
    let due = usage
        .per_calendar_day()
        .and_then(|_| hours_due(today(), interval, done, usage))
        .ok_or_else(|| {
            bad("Set hours/day and days/month on this check or on the equipment first")
        })?;
    sqlx::query(
        "UPDATE maintenance_conditions SET hours_done = ?, next_due_date = ?, updated_at = CURRENT_TIMESTAMP
          WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(done)
    .bind(due)
    .bind(id)
    .bind(user.company_id)
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn hours_conditions_come_due_by_average_daily_use() {
        let usage = Usage {
            hours_per_day: Some(8.0),
            days_per_month: Some(20.0),
        };
        // 8 * 20 / 30 = 5.33 hours a calendar day; 100 hours → 19 days.
        assert_eq!(
            hours_due(day(2026, 1, 1), 100.0, 0.0, usage),
            Some(day(2026, 1, 20))
        );
        assert_eq!(
            hours_due(day(2026, 1, 1), 100.0, 100.0, usage),
            Some(day(2026, 1, 1))
        );
        assert_eq!(
            hours_due(day(2026, 1, 1), 100.0, 0.0, Usage::default()),
            None
        );
    }

    #[test]
    fn day_conditions_add_whole_days() {
        assert_eq!(add_days(day(2026, 1, 30), 3.0), day(2026, 2, 2));
        assert_eq!(add_days(day(2026, 1, 30), 1.5), day(2026, 1, 31));
    }
}
