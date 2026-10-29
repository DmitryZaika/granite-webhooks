//! How long an installation or template takes, from its sale, and the
//! Auto-plan that fills each person's work days with the waiting ones.
//!
//! - `GET /v1/schedule/time-rules` → the company's job time rules
//! - `PUT /v1/schedule/time-rules` → an admin changes one kind's rules
//! - `GET /v1/schedule/job-time` → a job's length from its sale
//! - `POST /v1/schedule/auto-plan` → a proposed day, time and person for each
//!   unscheduled event of a calendar (nothing is saved: the page saves the
//!   ones the user keeps through `PUT /v1/events/{id}`)
//!
//! A job is sized from the event's sale, else the customer's latest sale not
//! yet installed: its square feet, and per room its waterfalls, backsplash,
//! tear-out, cooktop and sinks.

use crate::auth::{AdminUser, EmployeeUser};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::{JsonBody, MultiQuery};
use crate::openapi::{IntoParams, ToSchema};
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use chrono_tz::America::Indiana::Indianapolis;
use serde::{Deserialize, Serialize};
use sqlx::MySqlPool;
use std::collections::HashMap;

/// `positions.id` of an installer.
const INSTALLER: i32 = 6;
/// `positions.id` of a template maker.
const TEMPLATE: i32 = 11;
/// Job lengths are rounded up to this many minutes.
const ROUND_TO: u32 = 15;
/// Most work days one Auto-plan may fill.
const MAX_PLAN_DAYS: u32 = 30;
const MINUTES_PER_DAY: u32 = 24 * 60;

/// The two calendars whose jobs have a length rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum JobKind {
    Installations,
    Templates,
}

impl JobKind {
    const fn slug(self) -> &'static str {
        match self {
            Self::Installations => "installations",
            Self::Templates => "templates",
        }
    }

    fn from_slug(slug: &str) -> Option<Self> {
        match slug {
            "installations" => Some(Self::Installations),
            "templates" => Some(Self::Templates),
            _ => None,
        }
    }

    const fn position(self) -> i32 {
        match self {
            Self::Installations => INSTALLER,
            Self::Templates => TEMPLATE,
        }
    }
}

/// How long one kind of job takes, and the work day it fits in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct TimeRules {
    /// Minutes every job takes (setup, cleanup).
    pub base_minutes: u32,
    /// Minutes per square foot of the sale.
    pub per_sqft_minutes: f64,
    /// Minutes per waterfall.
    pub per_waterfall_minutes: u32,
    /// Minutes per sink.
    pub per_sink_minutes: u32,
    /// Minutes per room with a cooktop cutout.
    pub per_cooktop_minutes: u32,
    /// Minutes per room with a 4" backsplash.
    pub per_backsplash_minutes: u32,
    /// Minutes per room with a full-height backsplash.
    pub per_full_backsplash_minutes: u32,
    /// Minutes per room with a tear-out.
    pub per_tear_out_minutes: u32,
    /// Minutes kept free between two jobs of one person (driving).
    pub travel_minutes: u32,
    /// When the work day starts, in minutes after midnight (shop time).
    pub day_start_minute: u32,
    /// When it ends, in minutes after midnight (shop time).
    pub day_end_minute: u32,
    /// Days people work: 0 Sunday ... 6 Saturday.
    pub work_days: Vec<u8>,
}

impl Default for TimeRules {
    fn default() -> Self {
        Self {
            base_minutes: 0,
            per_sqft_minutes: 0.0,
            per_waterfall_minutes: 0,
            per_sink_minutes: 0,
            per_cooktop_minutes: 0,
            per_backsplash_minutes: 0,
            per_full_backsplash_minutes: 0,
            per_tear_out_minutes: 0,
            travel_minutes: 30,
            day_start_minute: 9 * 60,
            day_end_minute: 17 * 60,
            work_days: vec![1, 2, 3, 4, 5],
        }
    }
}

impl TimeRules {
    /// Any length is set: jobs can be sized from their sales.
    fn sizes_jobs(&self) -> bool {
        self.base_minutes > 0
            || self.per_sqft_minutes > 0.0
            || self.per_waterfall_minutes > 0
            || self.per_sink_minutes > 0
            || self.per_cooktop_minutes > 0
            || self.per_backsplash_minutes > 0
            || self.per_full_backsplash_minutes > 0
            || self.per_tear_out_minutes > 0
    }

    fn work_day_mask(&self) -> u8 {
        self.work_days
            .iter()
            .filter(|day| **day < 7)
            .fold(0, |mask, day| mask | (1 << day))
    }

    fn works_on(&self, date: NaiveDate) -> bool {
        let day = u8::try_from(date.weekday().num_days_from_sunday()).unwrap_or(0);
        self.work_day_mask() & (1 << day) != 0
    }
}

/// `GET /v1/schedule/time-rules`.
#[derive(Debug, Serialize, ToSchema)]
pub struct CompanyTimeRules {
    /// The Installations calendar's jobs.
    pub installations: TimeRules,
    /// The Templates calendar's jobs.
    pub templates: TimeRules,
}

/// Body of `PUT /v1/schedule/time-rules`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct TimeRulesInput {
    /// Which jobs these rules are for.
    pub kind: JobKind,
    /// The new rules.
    pub rules: TimeRules,
}

#[derive(sqlx::FromRow)]
struct RulesRow {
    kind: String,
    base_minutes: i64,
    per_sqft_minutes: f64,
    per_waterfall_minutes: i64,
    per_sink_minutes: i64,
    per_cooktop_minutes: i64,
    per_backsplash_minutes: i64,
    per_full_backsplash_minutes: i64,
    per_tear_out_minutes: i64,
    travel_minutes: i64,
    day_start_minute: i64,
    day_end_minute: i64,
    work_days: i64,
}

fn minutes(value: i64) -> u32 {
    u32::try_from(value).unwrap_or(0)
}

impl From<RulesRow> for TimeRules {
    fn from(row: RulesRow) -> Self {
        Self {
            base_minutes: minutes(row.base_minutes),
            per_sqft_minutes: row.per_sqft_minutes,
            per_waterfall_minutes: minutes(row.per_waterfall_minutes),
            per_sink_minutes: minutes(row.per_sink_minutes),
            per_cooktop_minutes: minutes(row.per_cooktop_minutes),
            per_backsplash_minutes: minutes(row.per_backsplash_minutes),
            per_full_backsplash_minutes: minutes(row.per_full_backsplash_minutes),
            per_tear_out_minutes: minutes(row.per_tear_out_minutes),
            travel_minutes: minutes(row.travel_minutes),
            day_start_minute: minutes(row.day_start_minute),
            day_end_minute: minutes(row.day_end_minute),
            work_days: (0..7u8)
                .filter(|day| row.work_days & (1 << day) != 0)
                .collect(),
        }
    }
}

/// The company's rules for one kind; the defaults (no lengths) when unsaved.
async fn load_rules(
    pool: &MySqlPool,
    company_id: i32,
    kind: JobKind,
) -> Result<TimeRules, sqlx::Error> {
    Ok(load_all_rules(pool, company_id)
        .await?
        .remove(kind.slug())
        .unwrap_or_default())
}

async fn load_all_rules(
    pool: &MySqlPool,
    company_id: i32,
) -> Result<HashMap<String, TimeRules>, sqlx::Error> {
    let rows: Vec<RulesRow> = sqlx::query_as(
        "SELECT CAST(kind AS CHAR) AS kind,
                CAST(base_minutes AS SIGNED) AS base_minutes,
                CAST(per_sqft_minutes AS DOUBLE) AS per_sqft_minutes,
                CAST(per_waterfall_minutes AS SIGNED) AS per_waterfall_minutes,
                CAST(per_sink_minutes AS SIGNED) AS per_sink_minutes,
                CAST(per_cooktop_minutes AS SIGNED) AS per_cooktop_minutes,
                CAST(per_backsplash_minutes AS SIGNED) AS per_backsplash_minutes,
                CAST(per_full_backsplash_minutes AS SIGNED) AS per_full_backsplash_minutes,
                CAST(per_tear_out_minutes AS SIGNED) AS per_tear_out_minutes,
                CAST(travel_minutes AS SIGNED) AS travel_minutes,
                CAST(day_start_minute AS SIGNED) AS day_start_minute,
                CAST(day_end_minute AS SIGNED) AS day_end_minute,
                CAST(work_days AS SIGNED) AS work_days
           FROM schedule_time_rules WHERE company_id = ?",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.kind.clone(), TimeRules::from(row)))
        .collect())
}

/// Get the job time rules
///
/// How long the company's installations and templates take (per square
/// foot, waterfall, sink...) and the work day the Auto-plan fills. Unsaved
/// kinds come back with no lengths, 9:00 to 17:00, Monday to Friday.
#[crate::openapi::path(
    get,
    path = "/v1/schedule/time-rules",
    operation_id = "get_schedule_time_rules",
    tag = "schedule",
    responses(
        (status = 200, description = "Rules", body = CompanyTimeRules),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn time_rules(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<CompanyTimeRules>> {
    let mut all = load_all_rules(&state.pool, user.company_id).await?;
    Ok(Json(CompanyTimeRules {
        installations: all.remove("installations").unwrap_or_default(),
        templates: all.remove("templates").unwrap_or_default(),
    }))
}

fn check_rules(rules: &TimeRules) -> Result<(), ApiError> {
    let bad = |message: &str| Err(ApiError::BadRequest(message.into()));
    let lengths = [
        rules.base_minutes,
        rules.per_waterfall_minutes,
        rules.per_sink_minutes,
        rules.per_cooktop_minutes,
        rules.per_backsplash_minutes,
        rules.per_full_backsplash_minutes,
        rules.per_tear_out_minutes,
        rules.travel_minutes,
    ];
    if lengths.iter().any(|length| *length > MINUTES_PER_DAY) {
        return bad("A length can be at most 24 hours (1440 minutes).");
    }
    if !(0.0..=60.0).contains(&rules.per_sqft_minutes) || rules.per_sqft_minutes.is_nan() {
        return bad("Minutes per square foot must be between 0 and 60.");
    }
    if rules.day_start_minute >= rules.day_end_minute || rules.day_end_minute > MINUTES_PER_DAY {
        return bad("The work day must end after it starts.");
    }
    if rules.work_day_mask() == 0 {
        return bad("Pick at least one work day.");
    }
    Ok(())
}

/// Set the job time rules
///
/// Saves how long one kind of job takes and the work day, for everyone.
#[crate::openapi::path(
    put,
    path = "/v1/schedule/time-rules",
    operation_id = "set_schedule_time_rules",
    tag = "schedule",
    request_body = TimeRulesInput,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "A length over a day, a work day ending before it starts, or no work days", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn set_time_rules(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<TimeRulesInput>,
) -> ApiResult<StatusCode> {
    let rules = input.rules;
    check_rules(&rules)?;
    sqlx::query(
        "INSERT INTO schedule_time_rules
           (company_id, kind, base_minutes, per_sqft_minutes, per_waterfall_minutes,
            per_sink_minutes, per_cooktop_minutes, per_backsplash_minutes,
            per_full_backsplash_minutes, per_tear_out_minutes, travel_minutes,
            day_start_minute, day_end_minute, work_days)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON DUPLICATE KEY UPDATE base_minutes = VALUES(base_minutes),
           per_sqft_minutes = VALUES(per_sqft_minutes),
           per_waterfall_minutes = VALUES(per_waterfall_minutes),
           per_sink_minutes = VALUES(per_sink_minutes),
           per_cooktop_minutes = VALUES(per_cooktop_minutes),
           per_backsplash_minutes = VALUES(per_backsplash_minutes),
           per_full_backsplash_minutes = VALUES(per_full_backsplash_minutes),
           per_tear_out_minutes = VALUES(per_tear_out_minutes),
           travel_minutes = VALUES(travel_minutes),
           day_start_minute = VALUES(day_start_minute),
           day_end_minute = VALUES(day_end_minute),
           work_days = VALUES(work_days)",
    )
    .bind(user.company_id)
    .bind(input.kind.slug())
    .bind(rules.base_minutes)
    .bind((rules.per_sqft_minutes * 100.0).round() / 100.0)
    .bind(rules.per_waterfall_minutes)
    .bind(rules.per_sink_minutes)
    .bind(rules.per_cooktop_minutes)
    .bind(rules.per_backsplash_minutes)
    .bind(rules.per_full_backsplash_minutes)
    .bind(rules.per_tear_out_minutes)
    .bind(rules.travel_minutes)
    .bind(rules.day_start_minute)
    .bind(rules.day_end_minute)
    .bind(rules.work_day_mask())
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- Sizing a job

/// What a sale holds that takes time to install or template.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct JobSize {
    pub sale_id: i32,
    pub square_feet: f64,
    pub waterfalls: u32,
    pub sinks: u32,
    pub cooktops: u32,
    pub backsplash_rooms: u32,
    pub full_backsplash_rooms: u32,
    pub tear_out_rooms: u32,
}

/// A part of a job's length.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct JobTimeLine {
    /// What it is for, e.g. "50 sq ft".
    pub label: String,
    /// Its minutes.
    pub minutes: u32,
}

/// `GET /v1/schedule/job-time`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct JobTime {
    /// The job's length, rounded up to 15 minutes.
    pub minutes: u32,
    /// The sale it was sized from.
    pub sale_id: i32,
    /// What the sale holds, e.g. "50 sq ft · 1 waterfall · 2 sinks".
    pub summary: String,
    /// How the length adds up.
    pub lines: Vec<JobTimeLine>,
}

fn is_none_text(text: &str) -> bool {
    matches!(text, "" | "no" | "none" | "n/a" | "na" | "0" | "false")
}

/// Waterfalls written in a room: "no" 0, "yes" 1, "X 2" or "Yes x4" the number.
fn waterfall_count(text: Option<&str>) -> u32 {
    let text = text.unwrap_or("").trim().to_lowercase();
    if is_none_text(&text) {
        return 0;
    }
    let digits: String = text
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().unwrap_or(1)
}

/// A room's backsplash: none, 4" (false) or full height (true).
fn backsplash_kind(text: Option<&str>) -> Option<bool> {
    let text = text.unwrap_or("").trim().to_lowercase();
    if is_none_text(&text) {
        return None;
    }
    Some(text.contains("full"))
}

fn has_tear_out(text: Option<&str>) -> bool {
    !is_none_text(&text.unwrap_or("").trim().to_lowercase())
}

/// A cooktop needs a cutout ("c/t"); freestanding and slide-in ranges don't.
fn has_cooktop(text: Option<&str>) -> bool {
    let text = text.unwrap_or("").trim().to_lowercase();
    text == "c/t" || text.contains("cooktop")
}

fn plural(count: u32, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// "4\" backsplash (2 rooms)".
fn in_rooms(what: &str, count: u32) -> String {
    format!("{what} ({})", plural(count, "room", "rooms"))
}

fn format_sqft(square_feet: f64) -> String {
    let rounded = (square_feet * 10.0).round() / 10.0;
    if rounded.fract() == 0.0 {
        format!("{rounded:.0} sq ft")
    } else {
        format!("{rounded:.1} sq ft")
    }
}

/// The job's length from `rules`: each line, then the total rounded up to
/// 15 minutes. `None` when the rules set no lengths.
pub fn job_time(rules: &TimeRules, size: &JobSize) -> Option<JobTime> {
    if !rules.sizes_jobs() {
        return None;
    }
    let mut lines = Vec::new();
    let mut add = |label: String, minutes: u32| {
        if minutes > 0 {
            lines.push(JobTimeLine { label, minutes });
        }
    };
    add("Setup".into(), rules.base_minutes);
    if size.square_feet > 0.0 {
        // Sales hold at most a few thousand square feet; the cast is safe.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let sqft_minutes = (size.square_feet * rules.per_sqft_minutes)
            .ceil()
            .min(f64::from(u32::MAX)) as u32;
        add(format_sqft(size.square_feet), sqft_minutes);
    }
    add(
        plural(size.waterfalls, "waterfall", "waterfalls"),
        size.waterfalls * rules.per_waterfall_minutes,
    );
    add(
        plural(size.sinks, "sink", "sinks"),
        size.sinks * rules.per_sink_minutes,
    );
    add(
        plural(size.cooktops, "cooktop", "cooktops"),
        size.cooktops * rules.per_cooktop_minutes,
    );
    let room_parts = [
        (
            "4\" backsplash",
            size.backsplash_rooms,
            rules.per_backsplash_minutes,
        ),
        (
            "Full-height backsplash",
            size.full_backsplash_rooms,
            rules.per_full_backsplash_minutes,
        ),
        ("Tear-out", size.tear_out_rooms, rules.per_tear_out_minutes),
    ];
    for (what, count, each) in room_parts {
        add(in_rooms(what, count), count * each);
    }
    let total: u32 = lines.iter().map(|line| line.minutes).sum();
    let mut parts = Vec::new();
    if size.square_feet > 0.0 {
        parts.push(format_sqft(size.square_feet));
    }
    for (count, one, many) in [
        (size.waterfalls, "waterfall", "waterfalls"),
        (size.sinks, "sink", "sinks"),
        (size.cooktops, "cooktop", "cooktops"),
    ] {
        if count > 0 {
            parts.push(plural(count, one, many));
        }
    }
    for (what, count, _) in room_parts {
        if count > 0 {
            parts.push(in_rooms(&what.to_lowercase(), count));
        }
    }
    Some(JobTime {
        minutes: total.div_ceil(ROUND_TO) * ROUND_TO,
        sale_id: size.sale_id,
        summary: parts.join(" · "),
        lines,
    })
}

#[derive(sqlx::FromRow)]
struct RoomRow {
    room_key: String,
    waterfall: Option<String>,
    backsplash: Option<String>,
    tear_out: Option<String>,
    stove: Option<String>,
}

/// What the job's sale holds: `sale_id` when it is the company's and not
/// cancelled, else the customer's latest sale not yet installed (then their
/// latest). `None` without a sale.
pub async fn job_size(
    pool: &MySqlPool,
    company_id: i32,
    sale_id: Option<i32>,
    customer_id: Option<i32>,
) -> Result<Option<JobSize>, sqlx::Error> {
    if sale_id.is_none() && customer_id.is_none() {
        return Ok(None);
    }
    let sale: Option<(i32, f64)> = sqlx::query_as(
        "SELECT s.id, CAST(COALESCE(s.square_feet, 0) AS DOUBLE)
           FROM sales s
          WHERE s.company_id = ? AND s.cancelled_date IS NULL
            AND (s.id = ? OR (? IS NULL AND s.customer_id = ?))
          ORDER BY s.installed_date IS NULL DESC, s.sale_date DESC, s.id DESC
          LIMIT 1",
    )
    .bind(company_id)
    .bind(sale_id)
    .bind(sale_id)
    .bind(customer_id)
    .fetch_optional(pool)
    .await?;
    let Some((sale_id, square_feet)) = sale else {
        return Ok(None);
    };
    // Every slab of a room repeats the room's details.
    let rows: Vec<RoomRow> = sqlx::query_as(
        "SELECT COALESCE(HEX(room_uuid), CONCAT('slab', id)) AS room_key,
                waterfall, backsplash, tear_out, stove
           FROM slab_inventory WHERE sale_id = ?",
    )
    .bind(sale_id)
    .fetch_all(pool)
    .await?;
    let mut rooms: HashMap<String, (u32, Option<bool>, bool, bool)> = HashMap::new();
    for row in rows {
        let room = rooms.entry(row.room_key).or_default();
        room.0 = room.0.max(waterfall_count(row.waterfall.as_deref()));
        room.1 = room.1.max(backsplash_kind(row.backsplash.as_deref()));
        room.2 |= has_tear_out(row.tear_out.as_deref());
        room.3 |= has_cooktop(row.stove.as_deref());
    }
    let sinks: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sinks
          WHERE is_deleted = 0
            AND (sale_id = ? OR slab_id IN (SELECT id FROM slab_inventory WHERE sale_id = ?))",
    )
    .bind(sale_id)
    .bind(sale_id)
    .fetch_one(pool)
    .await?;
    let count = |keep: fn(&(u32, Option<bool>, bool, bool)) -> bool| {
        u32::try_from(rooms.values().filter(|room| keep(room)).count()).unwrap_or(u32::MAX)
    };
    Ok(Some(JobSize {
        sale_id,
        square_feet: square_feet.max(0.0),
        waterfalls: rooms.values().map(|room| room.0).sum(),
        sinks: u32::try_from(sinks).unwrap_or(0),
        cooktops: count(|room| room.3),
        backsplash_rooms: count(|room| room.1 == Some(false)),
        full_backsplash_rooms: count(|room| room.1 == Some(true)),
        tear_out_rooms: count(|room| room.2),
    }))
}

/// The kind of jobs on a calendar of the company, if it has one.
async fn calendar_kind(
    pool: &MySqlPool,
    company_id: i32,
    calendar_id: i32,
) -> Result<Option<JobKind>, sqlx::Error> {
    let slug: Option<Option<String>> = sqlx::query_scalar(
        "SELECT slug FROM calendars WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(calendar_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    Ok(slug.flatten().as_deref().and_then(JobKind::from_slug))
}

/// Query of `GET /v1/schedule/job-time`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct JobTimeQuery {
    /// The event's calendar: Installations or Templates.
    pub calendar_id: i32,
    /// The event's sale.
    pub sale_id: Option<i32>,
    /// The event's customer: their latest sale is used without `sale_id`.
    pub customer_id: Option<i32>,
}

/// A job's length
///
/// How long an installation or template takes, from the company's job time
/// rules and the sale (else the customer's latest sale not yet installed).
/// Null when the calendar is not Installations or Templates, its rules set
/// no lengths, or there is no sale.
#[crate::openapi::path(
    get,
    path = "/v1/schedule/job-time",
    operation_id = "get_schedule_job_time",
    tag = "schedule",
    params(JobTimeQuery),
    responses(
        (status = 200, description = "Length, or null", body = Option<JobTime>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn job_time_for(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    MultiQuery(query): MultiQuery<JobTimeQuery>,
) -> ApiResult<Json<Option<JobTime>>> {
    let pool = &state.pool;
    let Some(kind) = calendar_kind(pool, user.company_id, query.calendar_id).await? else {
        return Ok(Json(None));
    };
    let rules = load_rules(pool, user.company_id, kind).await?;
    if !rules.sizes_jobs() {
        return Ok(Json(None));
    }
    let size = job_size(pool, user.company_id, query.sale_id, query.customer_id).await?;
    Ok(Json(size.and_then(|size| job_time(&rules, &size))))
}

// ---- Auto-plan

/// A job to place: its length and, when it has one, its person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanJob {
    pub id: i32,
    pub minutes: u32,
    pub person: Option<i32>,
}

/// Where a job went: the day's index, its person and its minutes of the day.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    pub day: usize,
    pub person: Option<i32>,
    pub start: u32,
    pub end: u32,
}

/// Busy minutes of the day per person and day index.
pub type Busy = HashMap<(Option<i32>, usize), Vec<(u32, u32)>>;

/// The earliest start in `window` for `minutes` around the `busy` minutes,
/// keeping `travel` minutes between jobs. A job longer than the window only
/// fits a free day, from its start.
pub fn find_start(
    busy: &[(u32, u32)],
    window: (u32, u32),
    minutes: u32,
    travel: u32,
) -> Option<u32> {
    let (open, close) = window;
    if minutes > close.saturating_sub(open) {
        return busy.is_empty().then_some(open);
    }
    let mut sorted = busy.to_vec();
    sorted.sort_unstable();
    let mut cursor = open;
    for (start, end) in sorted {
        if cursor + minutes + travel <= start && cursor + minutes <= close {
            return Some(cursor);
        }
        cursor = cursor.max(end + travel);
    }
    (cursor + minutes <= close).then_some(cursor)
}

/// Places `jobs` in order, each on the first day it fits, at the earliest
/// time; a job without a person goes to whichever of `people` is free
/// earliest (the least booked on a tie). `None` for a job with no room.
pub fn plan(
    jobs: &[PlanJob],
    people: &[Option<i32>],
    days: usize,
    window: (u32, u32),
    travel: u32,
    busy: &mut Busy,
) -> Vec<Option<Slot>> {
    jobs.iter()
        .map(|job| {
            let candidates: Vec<Option<i32>> = match job.person {
                Some(person) => vec![Some(person)],
                None if people.is_empty() => vec![None],
                None => people.to_vec(),
            };
            let slot = (0..days).find_map(|day| {
                candidates
                    .iter()
                    .filter_map(|person| {
                        let taken = busy.get(&(*person, day)).map_or(&[][..], Vec::as_slice);
                        let booked: u32 = taken.iter().map(|(start, end)| end - start).sum();
                        find_start(taken, window, job.minutes, travel)
                            .map(|start| (start, booked, *person))
                    })
                    .min_by_key(|(start, booked, _)| (*start, *booked))
                    // A job longer than the day fills it.
                    .map(|(start, _, person)| Slot {
                        day,
                        person,
                        start,
                        end: (start + job.minutes).min(window.1),
                    })
            })?;
            busy.entry((slot.person, slot.day))
                .or_default()
                .push((slot.start, slot.end));
            Some(slot)
        })
        .collect()
}

/// The first `count` work days from `from`.
fn work_days(rules: &TimeRules, from: NaiveDate, count: u32) -> Vec<NaiveDate> {
    from.iter_days()
        .take(400)
        .filter(|date| rules.works_on(*date))
        .take(count as usize)
        .collect()
}

/// A shop-time moment as UTC.
fn shop_to_utc(date: NaiveDate, minute: u32) -> DateTime<Utc> {
    let local = date.and_time(NaiveTime::MIN) + Duration::minutes(i64::from(minute));
    Indianapolis
        .from_local_datetime(&local)
        .earliest()
        .or_else(|| {
            Indianapolis
                .from_local_datetime(&(local + Duration::hours(1)))
                .earliest()
        })
        .map_or_else(|| local.and_utc(), |at| at.with_timezone(&Utc))
}

/// The minutes of `date` (shop time) that a UTC range covers.
fn minutes_of_day(date: NaiveDate, start: NaiveDateTime, end: NaiveDateTime) -> Option<(u32, u32)> {
    let day_start = shop_to_utc(date, 0).naive_utc();
    let day_end = shop_to_utc(date + Duration::days(1), 0).naive_utc();
    let from = start.max(day_start);
    let to = end.min(day_end);
    if to <= from {
        return None;
    }
    let minute = |at: NaiveDateTime| {
        u32::try_from((at - day_start).num_minutes())
            .unwrap_or(0)
            .min(MINUTES_PER_DAY)
    };
    Some((minute(from), minute(to)))
}

fn utc_text(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Body of `POST /v1/schedule/auto-plan`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct AutoPlanInput {
    /// The Installations or Templates calendar.
    pub calendar_id: i32,
    /// The first day to fill (shop time).
    pub start_date: NaiveDate,
    /// How many work days to fill, 1 to 30.
    pub days: u32,
    /// The waiting statuses to plan (`ready`, `called`, `not_ready`).
    pub statuses: Vec<String>,
}

/// A proposed time for an unscheduled event.
#[derive(Debug, Serialize, ToSchema)]
pub struct Placement {
    /// The event.
    pub event_id: i32,
    /// Its name.
    pub title: String,
    /// Its customer's name.
    pub customer_name: Option<String>,
    /// The installer or template maker; null when the company has none.
    pub assigned_user_id: Option<i32>,
    /// Start (UTC, ISO 8601).
    pub start_date: String,
    /// End (UTC, ISO 8601).
    pub end_date: String,
    /// The job's length.
    pub minutes: u32,
    /// What the sale holds when the length came from it; null when the
    /// event's own length was used.
    pub job: Option<String>,
    /// The job is longer than a work day: it fills the day (check the sale).
    pub longer_than_day: bool,
}

/// An unscheduled event the plan left out.
#[derive(Debug, Serialize, ToSchema)]
pub struct Skipped {
    /// The event.
    pub event_id: i32,
    /// Its name.
    pub title: String,
    /// Why it was left out.
    pub reason: String,
}

/// `POST /v1/schedule/auto-plan`.
#[derive(Debug, Serialize, ToSchema)]
pub struct AutoPlan {
    /// The proposed times, by start.
    pub placements: Vec<Placement>,
    /// The events left out.
    pub skipped: Vec<Skipped>,
}

#[derive(sqlx::FromRow)]
struct WaitingRow {
    id: i32,
    title: String,
    customer_name: Option<String>,
    assigned_user_id: Option<i32>,
    sale_id: Option<i32>,
    customer_id: Option<i32>,
    start_date: NaiveDateTime,
    end_date: NaiveDateTime,
    all_day: bool,
    status: Option<String>,
}

#[derive(sqlx::FromRow)]
struct BusyRow {
    assigned_user_id: Option<i32>,
    calendar_id: Option<i32>,
    start_date: NaiveDateTime,
    end_date: NaiveDateTime,
}

/// Auto-plan unscheduled jobs
///
/// Proposes a day, time and person for each unscheduled event of the
/// Installations or Templates calendar with one of the statuses, oldest
/// first. Each job's length comes from its sale and the job time rules (else
/// the event's own length); it goes on the first work day where a person has
/// room around their other events, keeping the drive time between jobs. An
/// event with a person keeps them. Nothing is saved.
#[crate::openapi::path(
    post,
    path = "/v1/schedule/auto-plan",
    operation_id = "auto_plan_schedule",
    tag = "schedule",
    request_body = AutoPlanInput,
    responses(
        (status = 200, description = "Plan", body = AutoPlan),
        (status = 400, description = "Not the Installations or Templates calendar, or days not 1 to 30", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn auto_plan(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<AutoPlanInput>,
) -> ApiResult<Json<AutoPlan>> {
    let pool = &state.pool;
    let company_id = user.company_id;
    let kind = calendar_kind(pool, company_id, input.calendar_id)
        .await?
        .ok_or_else(|| {
            ApiError::BadRequest(
                "Auto-plan works on the Installations and Templates calendars.".into(),
            )
        })?;
    if input.days == 0 || input.days > MAX_PLAN_DAYS {
        return Err(ApiError::BadRequest(format!(
            "Plan 1 to {MAX_PLAN_DAYS} work days."
        )));
    }
    let rules = load_rules(pool, company_id, kind).await?;
    let days = work_days(&rules, input.start_date, input.days);
    let window = (rules.day_start_minute, rules.day_end_minute);

    let waiting: Vec<WaitingRow> = sqlx::query_as(
        "SELECT e.id, e.title, c.name AS customer_name, e.assigned_user_id, e.sale_id,
                e.customer_id, e.start_date, e.end_date,
                COALESCE(e.all_day, 0) <> 0 AS all_day, e.status
           FROM events e
           JOIN users u ON e.created_user_id = u.id
           LEFT JOIN customers c
             ON c.id = e.customer_id AND c.company_id = u.company_id AND c.deleted_at IS NULL
          WHERE u.company_id = ? AND e.deleted_date IS NULL AND e.unscheduled = 1
            AND e.calendar_id = ?
          ORDER BY e.created_date, e.id",
    )
    .bind(company_id)
    .bind(input.calendar_id)
    .fetch_all(pool)
    .await?;
    let waiting: Vec<WaitingRow> = waiting
        .into_iter()
        .filter(|event| {
            let status = event.status.as_deref().unwrap_or("not_ready");
            input.statuses.iter().any(|wanted| wanted == status)
        })
        .collect();

    let people: Vec<i32> = sqlx::query_scalar(
        "SELECT DISTINCT u.id FROM users u JOIN users_positions up ON up.user_id = u.id
          WHERE up.company_id = ? AND up.position_id = ? AND u.is_deleted = 0
          ORDER BY u.id",
    )
    .bind(company_id)
    .bind(kind.position())
    .fetch_all(pool)
    .await?;

    let mut jobs = Vec::new();
    let mut sized: HashMap<i32, Option<JobTime>> = HashMap::new();
    let mut skipped = Vec::new();
    for event in &waiting {
        let time = if rules.sizes_jobs() {
            job_size(pool, company_id, event.sale_id, event.customer_id)
                .await?
                .and_then(|size| job_time(&rules, &size))
        } else {
            None
        };
        let own = u32::try_from((event.end_date - event.start_date).num_minutes()).unwrap_or(0);
        let minutes = match &time {
            Some(time) if time.minutes > 0 => time.minutes,
            _ if !event.all_day && (ROUND_TO..MINUTES_PER_DAY).contains(&own) => own,
            _ => {
                skipped.push(Skipped {
                    event_id: event.id,
                    title: event.title.clone(),
                    reason: if rules.sizes_jobs() {
                        "No sale to size it from, and the event has no length.".into()
                    } else {
                        "Set the job time in Admin > Schedule, or give the event a length.".into()
                    },
                });
                continue;
            }
        };
        sized.insert(event.id, time);
        jobs.push(PlanJob {
            id: event.id,
            minutes,
            person: event.assigned_user_id,
        });
    }

    let mut busy = Busy::new();
    if let (Some(first), Some(last)) = (days.first(), days.last()) {
        let range_start = shop_to_utc(*first, 0).naive_utc();
        let range_end = shop_to_utc(*last + Duration::days(1), 0).naive_utc();
        let rows: Vec<BusyRow> = sqlx::query_as(
            "SELECT e.assigned_user_id, e.calendar_id, e.start_date, e.end_date
               FROM events e JOIN users u ON e.created_user_id = u.id
              WHERE u.company_id = ? AND e.deleted_date IS NULL AND e.unscheduled = 0
                AND NULLIF(TRIM(e.rrule), '') IS NULL
                AND e.start_date < ? AND e.end_date > ?
                AND (e.assigned_user_id IS NOT NULL OR e.calendar_id = ?)",
        )
        .bind(company_id)
        .bind(range_end)
        .bind(range_start)
        .bind(input.calendar_id)
        .fetch_all(pool)
        .await?;
        for row in rows {
            // Without people, the calendar's events share one crew; with
            // people, an unassigned event belongs to no one.
            let track = match row.assigned_user_id {
                Some(person) => Some(person),
                None if people.is_empty() && row.calendar_id == Some(input.calendar_id) => None,
                None => continue,
            };
            for (index, date) in days.iter().enumerate() {
                if let Some(minutes) = minutes_of_day(*date, row.start_date, row.end_date) {
                    busy.entry((track, index)).or_default().push(minutes);
                }
            }
        }
    }

    let candidates: Vec<Option<i32>> = people.iter().copied().map(Some).collect();
    let slots = plan(
        &jobs,
        &candidates,
        days.len(),
        window,
        rules.travel_minutes,
        &mut busy,
    );
    let mut placements = Vec::new();
    for (job, slot) in jobs.iter().zip(slots) {
        let event = waiting.iter().find(|event| event.id == job.id);
        let title = event.map(|event| event.title.clone()).unwrap_or_default();
        let Some(slot) = slot else {
            skipped.push(Skipped {
                event_id: job.id,
                title,
                reason: format!("No room in the next {} work days.", days.len()),
            });
            continue;
        };
        let date = days[slot.day];
        placements.push(Placement {
            event_id: job.id,
            title,
            customer_name: event.and_then(|event| event.customer_name.clone()),
            assigned_user_id: slot.person,
            start_date: utc_text(shop_to_utc(date, slot.start)),
            end_date: utc_text(shop_to_utc(date, slot.end)),
            minutes: job.minutes,
            job: sized
                .get(&job.id)
                .and_then(Option::as_ref)
                .map(|time| time.summary.clone()),
            longer_than_day: job.minutes > window.1 - window.0,
        });
    }
    placements.sort_by(|first, second| first.start_date.cmp(&second.start_date));
    Ok(Json(AutoPlan {
        placements,
        skipped,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules() -> TimeRules {
        TimeRules {
            base_minutes: 30,
            per_sqft_minutes: 3.0,
            per_waterfall_minutes: 30,
            per_sink_minutes: 20,
            ..TimeRules::default()
        }
    }

    #[test]
    fn room_text_is_read() {
        assert_eq!(waterfall_count(Some("no")), 0);
        assert_eq!(waterfall_count(None), 0);
        assert_eq!(waterfall_count(Some("yes")), 1);
        assert_eq!(waterfall_count(Some("X 2")), 2);
        assert_eq!(waterfall_count(Some("Yes x4")), 4);
        assert_eq!(backsplash_kind(Some("no")), None);
        assert_eq!(backsplash_kind(Some("4 inch")), Some(false));
        assert_eq!(backsplash_kind(Some("full height")), Some(true));
        assert!(has_tear_out(Some("laminate_t/o")));
        assert!(!has_tear_out(Some("no")));
        assert!(has_cooktop(Some("c/t")));
        assert!(!has_cooktop(Some("f/s")));
        assert!(!has_cooktop(Some("s/i")));
    }

    #[test]
    fn a_job_adds_up_and_rounds_to_a_quarter_hour() {
        let size = JobSize {
            sale_id: 7,
            square_feet: 50.0,
            waterfalls: 1,
            sinks: 2,
            ..JobSize::default()
        };
        let time = job_time(&rules(), &size).unwrap();
        // 30 + 150 + 30 + 40 = 250, rounded up to 255.
        assert_eq!(time.minutes, 255);
        assert_eq!(time.summary, "50 sq ft · 1 waterfall · 2 sinks");
        assert_eq!(
            time.lines,
            [
                JobTimeLine {
                    label: "Setup".into(),
                    minutes: 30
                },
                JobTimeLine {
                    label: "50 sq ft".into(),
                    minutes: 150
                },
                JobTimeLine {
                    label: "1 waterfall".into(),
                    minutes: 30
                },
                JobTimeLine {
                    label: "2 sinks".into(),
                    minutes: 40
                },
            ]
        );
        assert!(job_time(&TimeRules::default(), &size).is_none());
    }

    #[test]
    fn a_start_fits_around_busy_time_with_travel() {
        let day = (540, 1020);
        assert_eq!(find_start(&[], day, 120, 30), Some(540));
        // 9:00-11:00 busy: next at 11:30.
        assert_eq!(find_start(&[(540, 660)], day, 120, 30), Some(690));
        // Room before a 13:00 job: 9:00 + 2 h + 30 min <= 13:00.
        assert_eq!(find_start(&[(780, 900)], day, 120, 30), Some(540));
        // No room left.
        assert_eq!(find_start(&[(540, 960)], day, 120, 30), None);
        // Longer than the day: only on a free day.
        assert_eq!(find_start(&[], day, 600, 30), Some(540));
        assert_eq!(find_start(&[(540, 600)], day, 600, 30), None);
    }

    #[test]
    fn jobs_go_to_the_earliest_free_person_and_keep_their_own() {
        let mut busy = Busy::new();
        busy.insert((Some(1), 0), vec![(540, 900)]);
        let jobs = [
            PlanJob {
                id: 10,
                minutes: 240,
                person: None,
            },
            PlanJob {
                id: 11,
                minutes: 240,
                person: None,
            },
            PlanJob {
                id: 12,
                minutes: 120,
                person: Some(1),
            },
        ];
        let slots = plan(&jobs, &[Some(1), Some(2)], 2, (540, 1020), 30, &mut busy);
        // Person 1 is busy until 15:00, so person 2 takes the first job.
        assert_eq!(
            slots[0],
            Some(Slot {
                day: 0,
                person: Some(2),
                start: 540,
                end: 780
            })
        );
        // Neither has 4 hours left on day 0; on day 1 both are free.
        assert_eq!(
            slots[1],
            Some(Slot {
                day: 1,
                person: Some(1),
                start: 540,
                end: 780
            })
        );
        // Person 1's own job: no room on day 0, after the other job on day 1.
        assert_eq!(
            slots[2],
            Some(Slot {
                day: 1,
                person: Some(1),
                start: 810,
                end: 930
            })
        );
        assert_eq!(
            plan(&jobs[..1], &[], 1, (540, 1020), 30, &mut Busy::new())[0]
                .unwrap()
                .person,
            None
        );
    }
}
