//! The schedule: its events and people (`board`) and the tools that use
//! Google Maps.
//!
//! - `GET /v1/schedule/activity` → the latest changes to events (`activity`)
//! - `GET /v1/schedule/day-map` → a map of the company and a day's stops
//! - `POST /v1/schedule/optimize-route` → reorder a day's stops by drive time
//! - `/v1/schedule/notifications` → admins' texts and emails to customers
//!   before their appointments (`notifications`)
//! - `/v1/schedule/settings` → the day the weeks start on and 12- or 24-hour
//!   times, the company's and each user's (`settings`)
//! - `/v1/schedule/time-rules`, `job-time`, `auto-plan` → how long jobs take
//!   from their sales, and proposed times for the waiting ones (`job_time`)
//!
//! Replaces the Remix `api.schedule-day-map` and `api.schedule-optimize-route`.

pub mod activity;
pub mod board;
pub mod job_time;
pub mod notifications;
pub mod route;
pub mod settings;

use crate::auth::ShopOrEmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::{JsonBody, MultiQuery};
use crate::maps::Point;
use crate::openapi::{IntoParams, ToSchema};
use crate::openapi::{OpenApiRouter, routes};
use crate::routes::events::history::{History, record};
use crate::routes::events::reminders;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use chrono::NaiveDateTime;
use route::{TimedStop, assign_time_slots, solve_open_route};
use serde::{Deserialize, Serialize};
use sqlx::MySqlPool;

/// Most stops one route may have.
pub const MAX_ROUTE_STOPS: usize = 20;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(day_map))
        .routes(routes!(optimize_route))
        .routes(routes!(board::events))
        .routes(routes!(board::unscheduled))
        .routes(routes!(board::my_schedule))
        .routes(routes!(board::people))
        .routes(routes!(activity::activity))
        .routes(routes!(board::set_color))
        .routes(routes!(settings::settings, settings::set_settings))
        .routes(routes!(settings::set_my_settings))
        .routes(routes!(job_time::time_rules, job_time::set_time_rules))
        .routes(routes!(job_time::job_time_for))
        .routes(routes!(job_time::auto_plan))
        .routes(routes!(notifications::list))
        .routes(routes!(notifications::set_on_my_way))
        .routes(routes!(notifications::create_template))
        .routes(routes!(
            notifications::update_template,
            notifications::delete_template
        ))
        .routes(routes!(notifications::create_flow))
        .routes(routes!(
            notifications::update_flow,
            notifications::delete_flow
        ))
        .routes(routes!(notifications::active))
}

/// The company's address, trimmed; `None` when it has none.
pub async fn company_address(
    pool: &MySqlPool,
    company_id: i32,
) -> Result<Option<String>, sqlx::Error> {
    let address: Option<Option<String>> =
        sqlx::query_scalar("SELECT address FROM company WHERE id = ?")
            .bind(company_id)
            .fetch_optional(pool)
            .await?;
    Ok(address
        .flatten()
        .map(|address| address.trim().to_string())
        .filter(|address| !address.is_empty()))
}

/// Query of `GET /v1/schedule/day-map`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct DayMapQuery {
    /// The day's stops in order (repeat the parameter; at most 20 drawn).
    #[serde(default)]
    pub address: Vec<String>,
}

/// `GET /v1/schedule/day-map`.
#[derive(Debug, Serialize, ToSchema)]
pub struct DayMap {
    /// PNG as a `data:` URL; null when nothing could be placed.
    pub image: Option<String>,
    /// The company address the route starts from.
    pub home_address: Option<String>,
    /// Where the company address is.
    pub home: Option<Point>,
    /// Where each asked stop is, in order; null where it cannot be found.
    pub stops: Vec<Option<Point>>,
    /// The driving route from the company through the stops (Google encoded
    /// polyline); null when there is none.
    pub route: Option<String>,
}

/// Map a day's stops
///
/// A map with the company address and the stops numbered in order, joined by
/// a line. Null image without Google Maps or when no address is found.
#[crate::openapi::path(
    get,
    path = "/v1/schedule/day-map",
    operation_id = "get_schedule_day_map",
    tag = "schedule",
    params(DayMapQuery),
    responses(
        (status = 200, description = "Map", body = DayMap),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee, admin or shop manager or worker", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn day_map(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
    MultiQuery(query): MultiQuery<DayMapQuery>,
) -> ApiResult<Json<DayMap>> {
    let home_address = company_address(&state.pool, user.company_id).await?;
    let day = state
        .maps
        .day_map(home_address.as_deref(), &query.address)
        .await;
    Ok(Json(DayMap {
        image: day.image,
        home_address,
        home: day.home,
        stops: day.stops,
        route: day.line,
    }))
}

/// Body of `POST /v1/schedule/optimize-route`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct RouteInput {
    /// The day's events with an address, 2 to 20.
    pub event_ids: Vec<i32>,
}

/// `POST /v1/schedule/optimize-route`.
#[derive(Debug, Serialize, ToSchema)]
pub struct OptimizedRoute {
    /// Event ids in driving order from the company.
    pub order: Vec<i32>,
    /// Events that got a new time.
    pub changed: usize,
    /// Events on the route.
    pub stops: usize,
}

#[derive(sqlx::FromRow)]
struct Stop {
    id: i32,
    location: String,
    start_date: NaiveDateTime,
    end_date: NaiveDateTime,
    all_day: bool,
    rrule: Option<String>,
    created_user_id: i32,
}

/// The company's live events among `ids` that have an address, by start.
async fn load_stops(
    pool: &MySqlPool,
    company_id: i32,
    ids: &[i32],
) -> Result<Vec<Stop>, sqlx::Error> {
    let mut sql = sqlx::QueryBuilder::<sqlx::MySql>::new(
        "SELECT e.id, TRIM(e.location) AS location, e.start_date, e.end_date,
                COALESCE(e.all_day, 0) <> 0 AS all_day, e.rrule, e.created_user_id
           FROM events e JOIN users u ON e.created_user_id = u.id
          WHERE u.company_id = ",
    );
    sql.push_bind(company_id).push(
        " AND e.deleted_date IS NULL AND e.location IS NOT NULL AND TRIM(e.location) <> '' AND e.id IN (",
    );
    let mut list = sql.separated(", ");
    for id in ids {
        list.push_bind(*id);
    }
    sql.push(") ORDER BY e.start_date, e.id");
    sql.build_query_as().fetch_all(pool).await
}

fn bad(message: &str) -> ApiError {
    ApiError::BadRequest(message.into())
}

/// Optimize a day's route
///
/// Orders the events by the shortest drive from the company address and
/// hands the day's existing start times out in that order (each event keeps
/// its length). All-day and repeating events are ordered but keep their
/// times. Moved events get a history line and re-timed reminders.
#[crate::openapi::path(
    post,
    path = "/v1/schedule/optimize-route",
    operation_id = "optimize_schedule_route",
    tag = "schedule",
    request_body = RouteInput,
    responses(
        (status = 200, description = "Route", body = OptimizedRoute),
        (status = 400, description = "Fewer than 2 or more than 20 events, events without an address, or no company address", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee, admin or shop manager or worker", body = ErrorBody),
        (status = 502, description = "Google Maps could not build the route", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn optimize_route(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
    JsonBody(input): JsonBody<RouteInput>,
) -> ApiResult<Json<OptimizedRoute>> {
    let pool = &state.pool;
    let mut ids: Vec<i32> = input.event_ids.into_iter().filter(|id| *id > 0).collect();
    ids.sort_unstable();
    ids.dedup();
    if ids.len() < 2 {
        return Err(bad("At least two events with an address are needed."));
    }
    if ids.len() > MAX_ROUTE_STOPS {
        return Err(ApiError::BadRequest(format!(
            "A route can have at most {MAX_ROUTE_STOPS} stops."
        )));
    }
    let stops = load_stops(pool, user.company_id, &ids).await?;
    if stops.len() != ids.len() {
        return Err(bad(
            "Some events changed or can no longer be routed. Refresh and try again.",
        ));
    }
    let home = company_address(pool, user.company_id)
        .await?
        .ok_or_else(|| {
            bad("Add the company address in settings so the route has a starting point.")
        })?;
    let locations: Vec<String> = stops.iter().map(|stop| stop.location.clone()).collect();
    let cost = state
        .maps
        .drive_times(&home, &locations)
        .await
        .map_err(ApiError::Upstream)?;
    let order: Vec<i32> = solve_open_route(&cost)
        .into_iter()
        .map(|point| stops[point - 1].id)
        .collect();
    // Only one-off timed events get new times.
    let timed: Vec<TimedStop> = stops
        .iter()
        .filter(|stop| {
            !stop.all_day
                && stop
                    .rrule
                    .as_deref()
                    .is_none_or(|rule| rule.trim().is_empty())
        })
        .map(|stop| TimedStop {
            id: stop.id,
            start: stop.start_date,
            end: stop.end_date,
        })
        .collect();
    let timed_order: Vec<i32> = order
        .iter()
        .copied()
        .filter(|id| timed.iter().any(|stop| stop.id == *id))
        .collect();
    let changed: Vec<TimedStop> = assign_time_slots(&timed, &timed_order)
        .into_iter()
        .filter(|slot| {
            timed
                .iter()
                .any(|stop| stop.id == slot.id && stop.start != slot.start)
        })
        .collect();
    let mut tx = pool.begin().await?;
    for slot in &changed {
        sqlx::query(
            "UPDATE events SET start_date = ?, end_date = ?, updated_date = CURRENT_TIMESTAMP WHERE id = ?",
        )
        .bind(slot.start)
        .bind(slot.end)
        .bind(slot.id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    for slot in &changed {
        record(
            pool,
            &History::new(
                slot.id,
                user.company_id,
                "date_updated",
                user.name.as_deref(),
            )
            .message("Time changed by route optimization"),
        )
        .await;
        let creator = stops
            .iter()
            .find(|stop| stop.id == slot.id)
            .map_or(user.id, |stop| stop.created_user_id);
        reminders::resync_after_move(pool, slot.id, user.company_id, creator, slot.start).await?;
    }
    Ok(Json(OptimizedRoute {
        order,
        changed: changed.len(),
        stops: stops.len(),
    }))
}
