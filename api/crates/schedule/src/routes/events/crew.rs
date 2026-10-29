//! The crew's progress on their jobs (the installer or template maker an
//! event is assigned to): On my way, Arrived, Completed.
//!
//! - `POST /v1/events/{id}/crew-status` → the crew taps On my way (the
//!   customer gets the company's On my way message) or Completed
//! - `POST /v1/me/location` → the crew's phone says where it is: reaching a
//!   job's address marks it Arrived; getting 0.5 mi away from the shop or the
//!   job they were at marks their next job On my way, without a message
//!
//! Only Completed's button completes a job. The statuses belong to the
//! event's day: moving it to another day clears them (`clear`).

use crate::auth::CurrentUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::maps::Point;
use crate::openapi::ToSchema;
use crate::routes::events::history::{History, record};
use crate::routes::schedule::company_address;
use crate::routes::schedule::notifications::{Channel, send_on_my_way};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use chrono::{Duration, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use chrono_tz::America::Indiana::Indianapolis;
use serde::{Deserialize, Serialize};
use sqlx::MySqlPool;

/// Within this many miles of a job's address the crew has arrived (and of
/// the company address, is at the shop).
const ARRIVE_MILES: f64 = 0.1;
/// This many miles from the shop or the job they were at, they have left it.
const LEAVE_MILES: f64 = 0.5;
/// Positions less accurate than this many meters are ignored.
const MAX_ACCURACY_METERS: f64 = 200.0;
const EARTH_MILES: f64 = 3958.8;

/// The crew's progress on a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CrewStatus {
    OnMyWay,
    Arrived,
    Completed,
}

impl CrewStatus {
    fn parse(text: &str) -> Option<Self> {
        match text {
            "on_my_way" => Some(Self::OnMyWay),
            "arrived" => Some(Self::Arrived),
            "completed" => Some(Self::Completed),
            _ => None,
        }
    }
}

/// What the crew taps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CrewButton {
    OnMyWay,
    Completed,
}

/// Body of `POST /v1/events/{id}/crew-status`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CrewStatusInput {
    /// `on_my_way` (today's jobs) or `completed`.
    pub status: CrewButton,
}

/// `POST /v1/events/{id}/crew-status`.
#[derive(Debug, Serialize, ToSchema)]
pub struct CrewStatusSaved {
    /// The job's status now.
    pub status: CrewStatus,
    /// On my way: how the customer was told just now (`sms` or `email`);
    /// null when nothing went out.
    pub customer_told: Option<Channel>,
    /// On my way: why the customer was not told, for the crew to read.
    pub message_error: Option<String>,
}

/// An event as the crew's buttons and phone see it.
#[derive(sqlx::FromRow)]
struct CrewJob {
    id: i32,
    start_date: NaiveDateTime,
    end_date: NaiveDateTime,
    all_day: bool,
    unscheduled: bool,
    rrule: Option<String>,
    assigned_user_id: Option<i32>,
    crew_status: Option<String>,
    location: Option<String>,
}

const JOB_COLUMNS: &str = "e.id, e.start_date, e.end_date, COALESCE(e.all_day, 0) <> 0 AS all_day,
    e.unscheduled <> 0 AS unscheduled, NULLIF(TRIM(e.rrule), '') AS rrule,
    e.assigned_user_id, e.crew_status, NULLIF(TRIM(e.location), '') AS location";

impl CrewJob {
    fn status(&self) -> Option<CrewStatus> {
        self.crew_status.as_deref().and_then(CrewStatus::parse)
    }

    /// The store days it covers, first and last.
    fn days(&self) -> (NaiveDate, NaiveDate) {
        job_days(self.start_date, self.end_date, self.all_day)
    }
}

/// The store day of a UTC time.
fn store_day(at: NaiveDateTime) -> NaiveDate {
    at.and_utc().with_timezone(&Indianapolis).date_naive()
}

fn store_today() -> NaiveDate {
    Utc::now().with_timezone(&Indianapolis).date_naive()
}

/// The store days an event covers: an all-day one's dates (its end may be
/// the next midnight), else its start's and end's days in Indianapolis.
pub fn job_days(start: NaiveDateTime, end: NaiveDateTime, all_day: bool) -> (NaiveDate, NaiveDate) {
    if all_day {
        (
            start.date(),
            (end - Duration::seconds(1)).date().max(start.date()),
        )
    } else {
        (store_day(start), store_day(end.max(start)))
    }
}

/// Forgets the crew's progress on an event (it moved to another day).
pub async fn clear(pool: &MySqlPool, event_id: i32) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE events SET crew_status = NULL, on_my_way_at = NULL, on_my_way_sent_at = NULL,
                arrived_at = NULL, left_at = NULL, completed_at = NULL
          WHERE id = ? AND crew_status IS NOT NULL",
    )
    .bind(event_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Why the On my way message did not go out, for the crew.
fn message_error(code: &str) -> &'static str {
    match code {
        "no_message" => {
            "No On my way message is set up (Admin > Schedule > Customer notifications)"
        }
        "no_customer" => "The job has no customer",
        "sms_unavailable" => "Texting is not set up for this company",
        "no_sms_sender" => "No office manager has a texting line",
        "no_phone" => "The customer has no phone number",
        "no_email" => "The customer has no email",
        "unfilled_variables" => {
            "The message needs something the job does not have, e.g. an address"
        }
        "variables_failed" => "Could not fill in the message",
        _ => "The message failed to send",
    }
}

fn bad(message: &str) -> ApiError {
    ApiError::BadRequest(message.into())
}

/// Sets `crew_status`; `false` when the job was not in one of `from`.
async fn move_status(
    pool: &MySqlPool,
    id: i32,
    to: CrewStatus,
    from: &str,
) -> Result<bool, sqlx::Error> {
    let (status, column) = match to {
        CrewStatus::OnMyWay => ("on_my_way", "on_my_way_at"),
        CrewStatus::Arrived => ("arrived", "arrived_at"),
        CrewStatus::Completed => ("completed", "completed_at"),
    };
    let done = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE events SET crew_status = ?, {column} = UTC_TIMESTAMP()
          WHERE id = ? AND COALESCE(crew_status, '') IN ({from})"
    )))
    .bind(status)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Tap On my way or Completed
///
/// On my way (today's jobs, before arriving) marks the job and tells the
/// customer once with the company's On my way message: a text, or an email
/// when the company cannot text. The status is saved even when the message
/// cannot go; `message_error` says why. Completed marks the job done. For the
/// job's installer or template maker, or an admin.
#[crate::openapi::path(
    post,
    path = "/v1/events/{id}/crew-status",
    operation_id = "set_crew_status",
    tag = "schedule",
    params(("id" = i32, Path, description = "Event id")),
    request_body = CrewStatusInput,
    responses(
        (status = 200, description = "Saved", body = CrewStatusSaved),
        (status = 400, description = "Unscheduled, repeating, not today, already arrived, or not started yet", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Not the job's crew or an admin", body = ErrorBody),
        (status = 404, description = "No such event in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
#[allow(clippy::too_many_lines)]
pub async fn set_status(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<CrewStatusInput>,
) -> ApiResult<Json<CrewStatusSaved>> {
    let pool = &state.pool;
    let job: CrewJob = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {JOB_COLUMNS}
           FROM events e JOIN users u ON u.id = e.created_user_id
          WHERE e.id = ? AND u.company_id = ? AND e.deleted_date IS NULL"
    )))
    .bind(id)
    .bind(user.company_id)
    .fetch_optional(pool)
    .await?
    .ok_or(ApiError::NotFound)?;
    if job.assigned_user_id != Some(user.id) && !(user.is_admin || user.is_superuser) {
        return Err(ApiError::Forbidden);
    }
    if job.unscheduled {
        return Err(bad("The job is not on the calendar yet"));
    }
    if job.rrule.is_some() {
        return Err(bad("Repeating events have no On my way or Completed"));
    }
    let today = store_today();
    let (first, last) = job.days();
    let by = user.name.as_deref();
    match input.status {
        CrewButton::Completed => {
            if today < first {
                return Err(bad("The job has not started yet"));
            }
            if move_status(
                pool,
                id,
                CrewStatus::Completed,
                "'', 'on_my_way', 'arrived'",
            )
            .await?
            {
                record(
                    pool,
                    &History::new(id, user.company_id, "updated", by).message("Completed"),
                )
                .await;
            }
            Ok(Json(CrewStatusSaved {
                status: CrewStatus::Completed,
                customer_told: None,
                message_error: None,
            }))
        }
        CrewButton::OnMyWay => {
            if today < first || today > last {
                return Err(bad("On my way is only for today's jobs"));
            }
            match job.status() {
                Some(CrewStatus::Arrived) => return Err(bad("You already arrived at this job")),
                Some(CrewStatus::Completed) => return Err(bad("This job is already completed")),
                _ => {}
            }
            move_status(pool, id, CrewStatus::OnMyWay, "''").await?;
            // Tells the customer once, even when the button is tapped twice.
            let claimed = sqlx::query(
                "UPDATE events SET on_my_way_sent_at = UTC_TIMESTAMP()
                  WHERE id = ? AND on_my_way_sent_at IS NULL",
            )
            .bind(id)
            .execute(pool)
            .await?
            .rows_affected()
                == 1;
            let (customer_told, message_error, line) = if claimed {
                match send_on_my_way(&state, user.company_id, id).await? {
                    Ok(channel) => (
                        Some(channel),
                        None,
                        match channel {
                            Channel::Sms => "On my way: the customer got a text".to_string(),
                            Channel::Email => "On my way: the customer got an email".to_string(),
                        },
                    ),
                    Err(code) => {
                        sqlx::query("UPDATE events SET on_my_way_sent_at = NULL WHERE id = ?")
                            .bind(id)
                            .execute(pool)
                            .await?;
                        let why = message_error(code);
                        (
                            None,
                            Some(why.to_string()),
                            format!("On my way (no message: {why})"),
                        )
                    }
                }
            } else {
                (
                    None,
                    Some("The customer was already told".to_string()),
                    String::new(),
                )
            };
            if !line.is_empty() {
                record(
                    pool,
                    &History::new(id, user.company_id, "updated", by).message(&line),
                )
                .await;
            }
            Ok(Json(CrewStatusSaved {
                status: CrewStatus::OnMyWay,
                customer_told,
                message_error,
            }))
        }
    }
}

/// Body of `POST /v1/me/location`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct LocationInput {
    /// Degrees.
    pub latitude: f64,
    /// Degrees.
    pub longitude: f64,
    /// How far off the position may be, in meters; worse than 200 is ignored.
    #[serde(default)]
    pub accuracy_m: Option<f64>,
}

/// `POST /v1/me/location`: the jobs whose status changed.
#[derive(Debug, Default, Serialize, ToSchema)]
pub struct LocationResult {
    /// The job the crew just arrived at.
    pub arrived: Option<i32>,
    /// The job they just set off for.
    pub on_my_way: Option<i32>,
}

/// Where the crew is: the shop or one of their jobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    Shop,
    Job(i32),
}

/// One of the crew's jobs today, in their day's order.
#[derive(Debug, Clone, Copy)]
pub struct Stop {
    pub id: i32,
    /// Where its address is; `None` without one or when Google cannot find it.
    pub point: Option<Point>,
    pub status: Option<CrewStatus>,
}

/// What a position changes.
#[derive(Debug, PartialEq)]
pub struct Moves {
    /// The place the crew left.
    pub left: Option<Place>,
    /// The job they arrived at.
    pub arrived: Option<i32>,
    /// The next job they set off for.
    pub on_my_way: Option<i32>,
    /// Where they are now.
    pub place: Option<(Place, Point)>,
}

/// Miles between two points (as the crow flies).
fn miles(a: Point, b: Point) -> f64 {
    let (lat1, lat2) = (a.lat.to_radians(), b.lat.to_radians());
    let dlat = lat2 - lat1;
    let dlng = (b.lng - a.lng).to_radians();
    let h = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlng / 2.0).sin().powi(2);
    2.0 * EARTH_MILES * h.sqrt().asin()
}

/// What the crew being at `here` changes, given where they were (`current`).
/// Reaching a job (its address within 0.1 mi) arrives there; getting 0.5 mi
/// from the place they were leaves it and sets off for the first of the day's
/// jobs with no status yet.
pub fn decide(
    here: Point,
    shop: Option<Point>,
    stops: &[Stop],
    current: Option<(Place, Point)>,
) -> Moves {
    let mut moves = Moves {
        left: None,
        arrived: None,
        on_my_way: None,
        place: current,
    };
    let reached = stops
        .iter()
        .filter_map(|stop| stop.point.map(|point| (stop, point, miles(here, point))))
        .filter(|(_, _, distance)| *distance <= ARRIVE_MILES)
        .min_by(|a, b| a.2.total_cmp(&b.2));
    if let Some((stop, point, _)) = reached {
        let place = Place::Job(stop.id);
        if current.map(|(at, _)| at) != Some(place) {
            moves.left = current.map(|(at, _)| at);
            moves.place = Some((place, point));
        }
        if matches!(stop.status, None | Some(CrewStatus::OnMyWay)) {
            moves.arrived = Some(stop.id);
        }
        return moves;
    }
    if let Some((at, point)) = current
        && miles(here, point) >= LEAVE_MILES
    {
        moves.left = Some(at);
        moves.place = None;
        moves.on_my_way = stops
            .iter()
            .find(|stop| stop.status.is_none() && Place::Job(stop.id) != at)
            .map(|stop| stop.id);
    }
    if moves.place.is_none()
        && let Some(shop) = shop
        && miles(here, shop) <= ARRIVE_MILES
    {
        moves.place = Some((Place::Shop, shop));
    }
    moves
}

/// Where the crew was, as saved by their last position.
#[derive(sqlx::FromRow)]
struct SavedPlace {
    at_shop: bool,
    place_event_id: Option<i32>,
    place_latitude: Option<f64>,
    place_longitude: Option<f64>,
    place_since: Option<NaiveDateTime>,
}

/// UTC midnight-to-midnight of a store day.
fn store_day_range(day: NaiveDate) -> (NaiveDateTime, NaiveDateTime) {
    let at = |day: NaiveDate| {
        Indianapolis
            .from_local_datetime(&day.and_time(NaiveTime::MIN))
            .earliest()
            .map_or_else(|| day.and_time(NaiveTime::MIN), |at| at.naive_utc())
    };
    (at(day), at(day + Duration::days(1)))
}

/// Report the crew's position
///
/// The crew's phone says where it is. Reaching one of today's jobs (its
/// address within 0.1 mi) marks it Arrived; getting 0.5 mi away from the
/// shop or the job they were at marks the first of today's jobs with no
/// status yet On my way. Nothing is sent to customers, and no job is marked
/// Completed. Positions less accurate than 200 m are ignored.
#[crate::openapi::path(
    post,
    path = "/v1/me/location",
    operation_id = "report_crew_location",
    tag = "schedule",
    request_body = LocationInput,
    responses(
        (status = 200, description = "What changed", body = LocationResult),
        (status = 400, description = "Not a position on Earth", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
#[allow(clippy::too_many_lines)]
pub async fn location(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    JsonBody(input): JsonBody<LocationInput>,
) -> ApiResult<Json<LocationResult>> {
    if !(-90.0..=90.0).contains(&input.latitude) || !(-180.0..=180.0).contains(&input.longitude) {
        return Err(bad("Not a position on Earth"));
    }
    if input
        .accuracy_m
        .is_some_and(|meters| meters > MAX_ACCURACY_METERS)
    {
        return Ok(Json(LocationResult::default()));
    }
    let pool = &state.pool;
    let here = Point {
        lat: input.latitude,
        lng: input.longitude,
    };
    let today = store_today();
    // Wide enough for any event touching today; `days` narrows it.
    let (from, _) = store_day_range(today - Duration::days(1));
    let (_, to) = store_day_range(today + Duration::days(1));
    let mut jobs: Vec<CrewJob> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {JOB_COLUMNS}
           FROM events e JOIN users u ON u.id = e.created_user_id
          WHERE u.company_id = ? AND e.assigned_user_id = ? AND e.deleted_date IS NULL
            AND e.unscheduled = 0 AND NULLIF(TRIM(e.rrule), '') IS NULL
            AND e.start_date < ? AND e.end_date >= ?
          ORDER BY COALESCE(e.all_day, 0) = 0, e.start_date, e.id"
    )))
    .bind(user.company_id)
    .bind(user.id)
    .bind(to)
    .bind(from)
    .fetch_all(pool)
    .await?;
    jobs.retain(|job| {
        let (first, last) = job.days();
        first <= today && today <= last
    });
    let mut stops = Vec::with_capacity(jobs.len());
    for job in &jobs {
        let point = match &job.location {
            Some(address) => state.maps.geocode(address).await,
            None => None,
        };
        stops.push(Stop {
            id: job.id,
            point,
            status: job.status(),
        });
    }
    let shop = match company_address(pool, user.company_id).await? {
        Some(address) => state.maps.geocode(&address).await,
        None => None,
    };
    let saved: Option<SavedPlace> = sqlx::query_as(
        "SELECT at_shop <> 0 AS at_shop, place_event_id, place_latitude, place_longitude, place_since
           FROM crew_positions WHERE user_id = ? AND company_id = ?",
    )
    .bind(user.id)
    .bind(user.company_id)
    .fetch_optional(pool)
    .await?;
    // A place from an earlier day is not where they start today.
    let current = saved.and_then(|saved| {
        if store_day(saved.place_since?) != today {
            return None;
        }
        let point = Point {
            lat: saved.place_latitude?,
            lng: saved.place_longitude?,
        };
        match (saved.at_shop, saved.place_event_id) {
            (true, _) => Some((Place::Shop, point)),
            (false, Some(id)) => Some((Place::Job(id), point)),
            (false, None) => None,
        }
    });
    let moves = decide(here, shop, &stops, current);
    let by = user.name.as_deref();
    let mut result = LocationResult::default();
    if let Some(Place::Job(id)) = moves.left {
        sqlx::query("UPDATE events SET left_at = UTC_TIMESTAMP() WHERE id = ?")
            .bind(id)
            .execute(pool)
            .await?;
    }
    if let Some(id) = moves.on_my_way
        && move_status(pool, id, CrewStatus::OnMyWay, "''").await?
    {
        record(
            pool,
            &History::new(id, user.company_id, "updated", by).message("On my way (phone location)"),
        )
        .await;
        result.on_my_way = Some(id);
    }
    if let Some(id) = moves.arrived
        && move_status(pool, id, CrewStatus::Arrived, "'', 'on_my_way'").await?
    {
        record(
            pool,
            &History::new(id, user.company_id, "updated", by).message("Arrived (phone location)"),
        )
        .await;
        result.arrived = Some(id);
    }
    let unchanged = moves.place.map(|(at, _)| at) == current.map(|(at, _)| at);
    let (at_shop, place_event_id, place_point) = match moves.place {
        Some((Place::Shop, point)) => (true, None, Some(point)),
        Some((Place::Job(id), point)) => (false, Some(id), Some(point)),
        None => (false, None, None),
    };
    sqlx::query(
        "INSERT INTO crew_positions (user_id, company_id, latitude, longitude, recorded_at,
                at_shop, place_event_id, place_latitude, place_longitude, place_since)
         VALUES (?, ?, ?, ?, UTC_TIMESTAMP(), ?, ?, ?, ?, IF(?, NULL, UTC_TIMESTAMP()))
         ON DUPLICATE KEY UPDATE latitude = VALUES(latitude), longitude = VALUES(longitude),
                recorded_at = VALUES(recorded_at), at_shop = VALUES(at_shop),
                place_event_id = VALUES(place_event_id),
                place_latitude = VALUES(place_latitude), place_longitude = VALUES(place_longitude),
                place_since = IF(?, place_since, VALUES(place_since))",
    )
    .bind(user.id)
    .bind(user.company_id)
    .bind(here.lat)
    .bind(here.lng)
    .bind(at_shop)
    .bind(place_event_id)
    .bind(place_point.map(|point| point.lat))
    .bind(place_point.map(|point| point.lng))
    .bind(place_point.is_none())
    .bind(unchanged && place_point.is_some())
    .execute(pool)
    .await?;
    Ok(Json(result))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// About 0.07 mi per 0.001° of latitude.
    fn north(miles: f64) -> Point {
        Point {
            lat: 39.9 + miles / 69.0,
            lng: -86.1,
        }
    }

    const SHOP: Point = Point {
        lat: 39.9,
        lng: -86.1,
    };

    fn stop(id: i32, at_miles: f64, status: Option<CrewStatus>) -> Stop {
        Stop {
            id,
            point: Some(north(at_miles)),
            status,
        }
    }

    #[test]
    fn distances_are_in_miles() {
        assert!((miles(SHOP, north(1.0)) - 1.0).abs() < 0.01);
        assert!(miles(SHOP, SHOP) < 1e-9);
    }

    #[test]
    fn leaving_the_shop_sets_off_for_the_first_job() {
        let stops = [stop(1, 5.0, None), stop(2, 10.0, None)];
        // At the shop: nothing changes yet.
        let at_shop = decide(north(0.05), Some(SHOP), &stops, None);
        assert_eq!(at_shop.place, Some((Place::Shop, SHOP)));
        assert_eq!(at_shop.on_my_way, None);
        // 0.3 mi out: still near the shop.
        let near = decide(north(0.3), Some(SHOP), &stops, at_shop.place);
        assert_eq!(near.on_my_way, None);
        assert_eq!(near.place, at_shop.place);
        // 0.5 mi out: on the way to the first job, not at the shop any more.
        let gone = decide(north(0.6), Some(SHOP), &stops, at_shop.place);
        assert_eq!(gone.left, Some(Place::Shop));
        assert_eq!(gone.on_my_way, Some(1));
        assert_eq!(gone.place, None);
    }

    #[test]
    fn reaching_a_job_arrives_and_leaving_it_sets_off_for_the_next() {
        let stops = [stop(1, 5.0, Some(CrewStatus::OnMyWay)), stop(2, 10.0, None)];
        let there = decide(north(5.05), Some(SHOP), &stops, None);
        assert_eq!(there.arrived, Some(1));
        assert_eq!(there.place, Some((Place::Job(1), north(5.0))));
        // Arrived: leaving it (0.5 mi) sets off for job 2, and job 1 is not completed.
        let stops = [stop(1, 5.0, Some(CrewStatus::Arrived)), stop(2, 10.0, None)];
        let left = decide(north(5.6), Some(SHOP), &stops, there.place);
        assert_eq!(left.left, Some(Place::Job(1)));
        assert_eq!(left.on_my_way, Some(2));
        assert_eq!(left.arrived, None);
    }

    #[test]
    fn a_job_next_door_is_reached_without_leaving_half_a_mile() {
        let stops = [stop(1, 5.0, Some(CrewStatus::Arrived)), stop(2, 5.2, None)];
        let current = Some((Place::Job(1), north(5.0)));
        let next_door = decide(north(5.2), Some(SHOP), &stops, current);
        assert_eq!(next_door.left, Some(Place::Job(1)));
        assert_eq!(next_door.arrived, Some(2));
        assert_eq!(next_door.on_my_way, None);
    }

    #[test]
    fn done_jobs_and_wandering_change_nothing() {
        // Back at a completed job: they are there, but it stays completed.
        let stops = [
            stop(1, 5.0, Some(CrewStatus::Completed)),
            stop(2, 10.0, None),
        ];
        let back = decide(north(5.0), Some(SHOP), &stops, None);
        assert_eq!(back.arrived, None);
        assert_eq!(back.place, Some((Place::Job(1), north(5.0))));
        // From home (no place yet), driving around sets nothing off.
        let home = decide(north(20.0), Some(SHOP), &stops, None);
        assert_eq!(
            home,
            Moves {
                left: None,
                arrived: None,
                on_my_way: None,
                place: None,
            }
        );
        // Leaving the last job with every other job started: nothing to set off for.
        let stops = [
            stop(1, 5.0, Some(CrewStatus::Arrived)),
            stop(2, 10.0, Some(CrewStatus::OnMyWay)),
        ];
        let last = decide(
            north(6.0),
            Some(SHOP),
            &stops,
            Some((Place::Job(1), north(5.0))),
        );
        assert_eq!(last.left, Some(Place::Job(1)));
        assert_eq!(last.on_my_way, None);
    }

    #[test]
    fn all_day_jobs_cover_their_dates() {
        let at = |text: &str| NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M").unwrap();
        let day = NaiveDate::from_ymd_opt(2030, 1, 15).unwrap();
        // Ending at the next midnight still covers only its own date.
        assert_eq!(
            job_days(at("2030-01-15 00:00"), at("2030-01-16 00:00"), true),
            (day, day)
        );
        // 1 AM UTC on the 16th is still the 15th in Indianapolis.
        assert_eq!(
            job_days(at("2030-01-16 01:00"), at("2030-01-16 02:00"), false),
            (day, day)
        );
    }
}
