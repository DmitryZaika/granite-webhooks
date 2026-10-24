//! Admin > Schedule: the company's own texts and emails to customers before
//! their appointments. Templates are the messages; a flow puts templates in
//! a sequence for one calendar ("1 day before: disconnect the sink; 1 hour
//! before: see you soon").
//!
//! - `GET /v1/schedule/notifications` → templates, flows, and whether texting is set up
//! - `POST /v1/schedule/notifications/templates`, `PUT`/`DELETE …/templates/{id}`
//! - `POST /v1/schedule/notifications/flows`, `PUT`/`DELETE …/flows/{id}`
//! - `GET /v1/schedule/notifications/active` → the event form's flows. The
//!   calendar's default flow goes on each of its events unless the event
//!   turns it off (`event_notification_skips`); the others only go on events
//!   that pick them (`event_notification_picks`)
//!
//! The appointment-reminders job sends the due steps (`send_due`). An event's
//! customer is the one its title names (a bare customer id), else its sale's.
//! An event with a flow on no longer gets the built-in reminders.

use crate::auth::{AdminUser, EmployeeUser};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::routes::company::queries::company_features;
use crate::routes::events::notify::{
    customer_email, customer_phone, escape_html, office_manager, sender_for,
};
use crate::routes::sms::provider::{SmsProvider, phone10};
use crate::state::AppState;
use crate::webhooks_client::{OutgoingEmail, OutgoingSms};
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use chrono_tz::America::Indiana::Indianapolis;
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use sqlx::{MySql, MySqlPool, Transaction};
use std::collections::HashMap;
use utoipa::{IntoParams, ToSchema};

/// Longest text a template may send.
const MAX_SMS: usize = 1600;
const MAX_EMAIL: usize = 20_000;
/// Most steps in one flow.
const MAX_STEPS: usize = 10;
/// Steps checked per run, and events per step.
const RULE_BATCH: i64 = 500;
const EVENT_BATCH: i64 = 200;

/// How a template reaches the customer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    Sms,
    Email,
}

impl Channel {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Sms => "sms",
            Self::Email => "email",
        }
    }

    fn parse(text: &str) -> Self {
        if text == "sms" {
            Self::Sms
        } else {
            Self::Email
        }
    }
}

/// What `offset_value` counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum OffsetUnit {
    Minutes,
    Hours,
    Days,
}

impl OffsetUnit {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Minutes => "minutes",
            Self::Hours => "hours",
            Self::Days => "days",
        }
    }

    fn parse(text: &str) -> Self {
        match text {
            "minutes" => Self::Minutes,
            "hours" => Self::Hours,
            _ => Self::Days,
        }
    }

    /// Largest `offset_value` (30 days in all).
    const fn max(self) -> i32 {
        match self {
            Self::Minutes => 30 * 24 * 60,
            Self::Hours => 30 * 24,
            Self::Days => 30,
        }
    }

    const fn minutes(self, value: i32) -> i64 {
        let value = value as i64;
        match self {
            Self::Minutes => value,
            Self::Hours => value * 60,
            Self::Days => value * 24 * 60,
        }
    }
}

/// A text or email to customers.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct NotificationTemplate {
    /// Template id.
    pub id: i32,
    /// What admins call it, e.g. "Disconnect the sink".
    pub name: String,
    /// `sms` or `email`.
    pub channel: Channel,
    /// Email subject (null for texts); may hold `{{variables}}`.
    pub subject: Option<String>,
    /// The text, plain; may hold `{{variables}}`.
    pub body: String,
}

/// A template of a flow, sent at a time before the appointment.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct FlowStep {
    /// Step id.
    pub id: i32,
    pub template_id: i32,
    /// How many `offset_unit`s before the appointment it is sent.
    pub offset_value: i32,
    /// `minutes`, `hours` or `days`.
    pub offset_unit: OffsetUnit,
    /// `days` steps: store time (`HH:MM`) it is sent that day; null for
    /// exactly `offset_value` days before the start.
    pub send_time: Option<String>,
}

/// Templates in a sequence for each appointment on a calendar.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct NotificationFlow {
    /// Flow id.
    pub id: i32,
    /// The calendar whose appointments it is for.
    pub calendar_id: i32,
    /// What admins and the event form call it.
    pub name: String,
    /// Goes on every event of the calendar; else only on events that pick it.
    pub is_default: bool,
    /// Off: kept, but nothing is sent.
    pub is_active: bool,
    /// In the order they go out. Text steps are left out while texting is
    /// not set up.
    pub steps: Vec<FlowStep>,
}

/// `GET /v1/schedule/notifications`.
#[derive(Debug, Serialize, ToSchema)]
pub struct NotificationSettings {
    /// Templates, oldest first. Texts are left out while texting is not set up.
    pub templates: Vec<NotificationTemplate>,
    /// Flows, oldest first.
    pub flows: Vec<NotificationFlow>,
    /// The company texts through `CloudTalk` or `RingCentral`.
    pub sms_enabled: bool,
}

/// Body of `POST /v1/schedule/notifications/templates` and `PUT …/templates/{id}`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct NotificationTemplateInput {
    /// At most 100 characters.
    pub name: String,
    /// `sms` needs `CloudTalk` or `RingCentral`.
    pub channel: Channel,
    /// Required for email (at most 255 characters).
    #[serde(default)]
    pub subject: Option<String>,
    /// Up to 1600 characters for a text.
    pub body: String,
}

/// A step of `NotificationFlowInput`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct FlowStepInput {
    /// The step being edited; leave out for a new one.
    #[serde(default)]
    pub id: Option<i32>,
    /// A template of the company.
    pub template_id: i32,
    /// 1 to 30 days' worth of `offset_unit`.
    pub offset_value: i32,
    pub offset_unit: OffsetUnit,
    /// `days` steps only: `HH:MM` store time.
    #[serde(default)]
    pub send_time: Option<String>,
}

/// Body of `POST /v1/schedule/notifications/flows` and `PUT …/flows/{id}`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct NotificationFlowInput {
    /// A calendar of the company.
    pub calendar_id: i32,
    /// At most 100 characters.
    pub name: String,
    /// The calendar's one default: setting it unsets the calendar's others.
    #[serde(default)]
    pub is_default: bool,
    /// Default on.
    #[serde(default = "on")]
    pub is_active: bool,
    /// 1 to 10 steps. On edit, steps left out are deleted.
    pub steps: Vec<FlowStepInput>,
}

const fn on() -> bool {
    true
}

/// `POST /v1/schedule/notifications/templates` and `…/flows`.
#[derive(Debug, Serialize, ToSchema)]
pub struct Created {
    /// The new template or flow.
    pub id: u64,
}

#[derive(sqlx::FromRow)]
struct TemplateRow {
    id: i32,
    name: String,
    channel: String,
    subject: Option<String>,
    body: String,
}

impl From<TemplateRow> for NotificationTemplate {
    fn from(row: TemplateRow) -> Self {
        Self {
            id: row.id,
            name: row.name,
            channel: Channel::parse(&row.channel),
            subject: row.subject,
            body: row.body,
        }
    }
}

#[derive(sqlx::FromRow)]
struct FlowRow {
    id: i32,
    calendar_id: i32,
    name: String,
    is_default: bool,
    is_active: bool,
}

#[derive(sqlx::FromRow)]
struct StepRow {
    id: i32,
    flow_id: i32,
    template_id: i32,
    offset_value: i32,
    offset_unit: String,
    send_time: Option<String>,
}

/// The company's templates; texts only with `sms_enabled`.
async fn load_templates(
    pool: &MySqlPool,
    company_id: i32,
    sms_enabled: bool,
) -> Result<Vec<NotificationTemplate>, sqlx::Error> {
    let rows: Vec<TemplateRow> = sqlx::query_as(
        "SELECT id, name, CAST(channel AS CHAR) AS channel, subject, body
           FROM schedule_notification_templates
          WHERE company_id = ? AND deleted_at IS NULL AND (? OR channel <> 'sms')
          ORDER BY id",
    )
    .bind(company_id)
    .bind(sms_enabled)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(NotificationTemplate::from).collect())
}

/// The company's flows with their steps; `active_only` leaves out the ones
/// turned off.
async fn load_flows(
    pool: &MySqlPool,
    company_id: i32,
    sms_enabled: bool,
    active_only: bool,
) -> Result<Vec<NotificationFlow>, sqlx::Error> {
    let flows: Vec<FlowRow> = sqlx::query_as(
        "SELECT f.id, f.calendar_id, f.name, f.is_default <> 0 AS is_default,
                f.is_active <> 0 AS is_active
           FROM schedule_notification_flows f
           JOIN calendars cal ON cal.id = f.calendar_id AND cal.deleted_at IS NULL
          WHERE f.company_id = ? AND f.deleted_at IS NULL AND (NOT ? OR f.is_active = 1)
          ORDER BY f.id",
    )
    .bind(company_id)
    .bind(active_only)
    .fetch_all(pool)
    .await?;
    let steps: Vec<StepRow> = sqlx::query_as(
        "SELECT r.id, r.flow_id, r.template_id, r.offset_value,
                CAST(r.offset_unit AS CHAR) AS offset_unit, TIME_FORMAT(r.send_time, '%H:%i') AS send_time
           FROM schedule_notification_rules r
           JOIN schedule_notification_templates m ON m.id = r.template_id AND m.deleted_at IS NULL
          WHERE r.company_id = ? AND r.deleted_at IS NULL AND (? OR m.channel <> 'sms')
          ORDER BY r.id",
    )
    .bind(company_id)
    .bind(sms_enabled)
    .fetch_all(pool)
    .await?;
    let mut by_flow: HashMap<i32, Vec<FlowStep>> = HashMap::new();
    for row in steps {
        by_flow.entry(row.flow_id).or_default().push(FlowStep {
            id: row.id,
            template_id: row.template_id,
            offset_value: row.offset_value,
            offset_unit: OffsetUnit::parse(&row.offset_unit),
            send_time: row.send_time,
        });
    }
    Ok(flows
        .into_iter()
        .map(|row| {
            let mut steps = by_flow.remove(&row.id).unwrap_or_default();
            steps.sort_by_key(|step| {
                step_order(
                    step.offset_value,
                    step.offset_unit,
                    step.send_time.as_deref(),
                )
            });
            NotificationFlow {
                id: row.id,
                calendar_id: row.calendar_id,
                name: row.name,
                is_default: row.is_default,
                is_active: row.is_active,
                steps,
            }
        })
        .collect())
}

/// Sorts steps in the order they go out: furthest ahead first, and on the
/// same day the earlier clock time first.
fn step_order(
    value: i32,
    unit: OffsetUnit,
    send_time: Option<&str>,
) -> (std::cmp::Reverse<i64>, String) {
    (
        std::cmp::Reverse(unit.minutes(value)),
        send_time.unwrap_or("99:99").to_string(),
    )
}

async fn sms_enabled(pool: &MySqlPool, company_id: i32) -> Result<bool, sqlx::Error> {
    let features = company_features(pool, company_id).await?;
    Ok(features.sms_provider().is_some() || features.telnyx_sms)
}

fn bad(message: &str) -> ApiError {
    ApiError::BadRequest(message.into())
}

fn check_name(name: &str) -> ApiResult<String> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err(bad("Name is required"));
    }
    if name.chars().count() > 100 {
        return Err(bad("Name must be 100 characters or less"));
    }
    Ok(name)
}

/// The template's name, subject and body, checked.
struct CheckedTemplate {
    name: String,
    subject: Option<String>,
    body: String,
}

async fn check_template(
    pool: &MySqlPool,
    company_id: i32,
    input: &NotificationTemplateInput,
) -> ApiResult<CheckedTemplate> {
    let name = check_name(&input.name)?;
    if input.channel == Channel::Sms && !sms_enabled(pool, company_id).await? {
        return Err(bad("Texting is not set up for this company"));
    }
    let body = input.body.trim().to_string();
    if body.is_empty() {
        return Err(bad("Message is required"));
    }
    let limit = match input.channel {
        Channel::Sms => MAX_SMS,
        Channel::Email => MAX_EMAIL,
    };
    if body.chars().count() > limit {
        return Err(ApiError::BadRequest(format!(
            "Message must be {limit} characters or less"
        )));
    }
    let subject = match input.channel {
        Channel::Sms => None,
        Channel::Email => {
            let subject = input.subject.as_deref().map(str::trim).unwrap_or_default();
            if subject.is_empty() {
                return Err(bad("Subject is required"));
            }
            if subject.chars().count() > 255 {
                return Err(bad("Subject must be 255 characters or less"));
            }
            Some(subject.to_string())
        }
    };
    Ok(CheckedTemplate {
        name,
        subject,
        body,
    })
}

/// The step's send time, checked; the template is the company's.
async fn check_step(
    pool: &MySqlPool,
    company_id: i32,
    sms_enabled: bool,
    input: &FlowStepInput,
) -> ApiResult<Option<String>> {
    let channel: Option<String> = sqlx::query_scalar(
        "SELECT CAST(channel AS CHAR) FROM schedule_notification_templates
          WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(input.template_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    let Some(channel) = channel else {
        return Err(bad("Template not found"));
    };
    if Channel::parse(&channel) == Channel::Sms && !sms_enabled {
        return Err(bad("Texting is not set up for this company"));
    }
    if input.offset_value < 1 || input.offset_value > input.offset_unit.max() {
        return Err(bad("Send it between 1 minute and 30 days before"));
    }
    match input
        .send_time
        .as_deref()
        .map(str::trim)
        .filter(|time| !time.is_empty())
    {
        None => Ok(None),
        Some(_) if input.offset_unit != OffsetUnit::Days => {
            Err(bad("A time of day only goes with days"))
        }
        Some(time) => {
            let parsed = NaiveTime::parse_from_str(time, "%H:%M")
                .map_err(|_| bad("Time of day must be HH:MM"))?;
            Ok(Some(parsed.format("%H:%M:00").to_string()))
        }
    }
}

/// The flow's name and its steps' send times, checked.
async fn check_flow(
    pool: &MySqlPool,
    company_id: i32,
    input: &NotificationFlowInput,
) -> ApiResult<(String, Vec<Option<String>>)> {
    let calendar: Option<i32> = sqlx::query_scalar(
        "SELECT id FROM calendars WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(input.calendar_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    if calendar.is_none() {
        return Err(bad("Calendar not found"));
    }
    let name = check_name(&input.name)?;
    if input.steps.is_empty() {
        return Err(bad("Add at least one template"));
    }
    if input.steps.len() > MAX_STEPS {
        return Err(ApiError::BadRequest(format!(
            "A flow has at most {MAX_STEPS} steps"
        )));
    }
    let sms_enabled = sms_enabled(pool, company_id).await?;
    let mut times = Vec::with_capacity(input.steps.len());
    for step in &input.steps {
        times.push(check_step(pool, company_id, sms_enabled, step).await?);
    }
    Ok((name, times))
}

async fn find(pool: &MySqlPool, table: &'static str, company_id: i32, id: i32) -> ApiResult<()> {
    let found: Option<i32> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT id FROM {table} WHERE id = ? AND company_id = ? AND deleted_at IS NULL"
    )))
    .bind(id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    found.map(|_| ()).ok_or(ApiError::NotFound)
}

/// Leaves the calendar without a default flow, before another becomes it.
async fn unset_defaults(
    tx: &mut Transaction<'_, MySql>,
    company_id: i32,
    calendar_id: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE schedule_notification_flows SET is_default = 0
          WHERE company_id = ? AND calendar_id = ? AND deleted_at IS NULL",
    )
    .bind(company_id)
    .bind(calendar_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Saves the flow's steps: edits the ones named by id, adds the new ones
/// and deletes the rest. A kept step is not sent again.
async fn save_steps(
    tx: &mut Transaction<'_, MySql>,
    company_id: i32,
    flow_id: i32,
    user_id: i32,
    steps: &[FlowStepInput],
    times: Vec<Option<String>>,
) -> Result<(), sqlx::Error> {
    let existing: Vec<i32> = sqlx::query_scalar(
        "SELECT id FROM schedule_notification_rules WHERE flow_id = ? AND deleted_at IS NULL",
    )
    .bind(flow_id)
    .fetch_all(&mut **tx)
    .await?;
    let mut kept = Vec::new();
    for (step, send_time) in steps.iter().zip(times) {
        match step
            .id
            .filter(|id| existing.contains(id) && !kept.contains(id))
        {
            Some(id) => {
                sqlx::query(
                    "UPDATE schedule_notification_rules
                        SET template_id = ?, offset_value = ?, offset_unit = ?, send_time = ?
                      WHERE id = ?",
                )
                .bind(step.template_id)
                .bind(step.offset_value)
                .bind(step.offset_unit.as_str())
                .bind(send_time)
                .bind(id)
                .execute(&mut **tx)
                .await?;
                kept.push(id);
            }
            None => {
                sqlx::query(
                    "INSERT INTO schedule_notification_rules
                       (company_id, flow_id, template_id, offset_value, offset_unit, send_time, created_by)
                     VALUES (?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(company_id)
                .bind(flow_id)
                .bind(step.template_id)
                .bind(step.offset_value)
                .bind(step.offset_unit.as_str())
                .bind(send_time)
                .bind(user_id)
                .execute(&mut **tx)
                .await?;
            }
        }
    }
    for id in existing.into_iter().filter(|id| !kept.contains(id)) {
        sqlx::query("UPDATE schedule_notification_rules SET deleted_at = NOW() WHERE id = ?")
            .bind(id)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

/// List the customer notification templates and flows
///
/// Texts are left out while the company has no `CloudTalk` or `RingCentral`.
#[utoipa::path(
    get,
    path = "/v1/schedule/notifications",
    operation_id = "list_schedule_notifications",
    tag = "schedule",
    responses(
        (status = 200, description = "Templates and flows", body = NotificationSettings),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
) -> ApiResult<Json<NotificationSettings>> {
    let pool = &state.pool;
    let sms_enabled = sms_enabled(pool, user.company_id).await?;
    Ok(Json(NotificationSettings {
        templates: load_templates(pool, user.company_id, sms_enabled).await?,
        flows: load_flows(pool, user.company_id, sms_enabled, false).await?,
        sms_enabled,
    }))
}

/// Add a customer notification template
///
/// A text needs `CloudTalk` or `RingCentral`; an email needs a subject.
#[utoipa::path(
    post,
    path = "/v1/schedule/notifications/templates",
    operation_id = "create_schedule_notification_template",
    tag = "schedule",
    request_body = NotificationTemplateInput,
    responses(
        (status = 201, description = "Added", body = Created),
        (status = 400, description = "Invalid field, or texting not set up", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create_template(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<NotificationTemplateInput>,
) -> ApiResult<(StatusCode, Json<Created>)> {
    let checked = check_template(&state.pool, user.company_id, &input).await?;
    let id = sqlx::query(
        "INSERT INTO schedule_notification_templates
           (company_id, name, channel, subject, body, created_by)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(user.company_id)
    .bind(checked.name)
    .bind(input.channel.as_str())
    .bind(checked.subject)
    .bind(checked.body)
    .bind(user.id)
    .execute(&state.pool)
    .await?
    .last_insert_id();
    Ok((StatusCode::CREATED, Json(Created { id })))
}

/// Edit a customer notification template
///
/// The flows using it send the new text from then on.
#[utoipa::path(
    put,
    path = "/v1/schedule/notifications/templates/{id}",
    operation_id = "update_schedule_notification_template",
    tag = "schedule",
    params(("id" = i32, Path, description = "Template id")),
    request_body = NotificationTemplateInput,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Invalid field, or texting not set up", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such template in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update_template(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<NotificationTemplateInput>,
) -> ApiResult<StatusCode> {
    find(
        &state.pool,
        "schedule_notification_templates",
        user.company_id,
        id,
    )
    .await?;
    let checked = check_template(&state.pool, user.company_id, &input).await?;
    sqlx::query(
        "UPDATE schedule_notification_templates
            SET name = ?, channel = ?, subject = ?, body = ?
          WHERE id = ?",
    )
    .bind(checked.name)
    .bind(input.channel.as_str())
    .bind(checked.subject)
    .bind(checked.body)
    .bind(id)
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Delete a customer notification template
///
/// Refused while a flow uses it.
#[utoipa::path(
    delete,
    path = "/v1/schedule/notifications/templates/{id}",
    operation_id = "delete_schedule_notification_template",
    tag = "schedule",
    params(("id" = i32, Path, description = "Template id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 400, description = "A flow uses it", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such template in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_template(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    find(
        &state.pool,
        "schedule_notification_templates",
        user.company_id,
        id,
    )
    .await?;
    let used_by: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT f.name FROM schedule_notification_rules r
           JOIN schedule_notification_flows f ON f.id = r.flow_id AND f.deleted_at IS NULL
          WHERE r.template_id = ? AND r.deleted_at IS NULL
          ORDER BY f.name",
    )
    .bind(id)
    .fetch_all(&state.pool)
    .await?;
    if !used_by.is_empty() {
        return Err(ApiError::BadRequest(format!(
            "Remove it from these flows first: {}",
            used_by.join(", ")
        )));
    }
    sqlx::query("UPDATE schedule_notification_templates SET deleted_at = NOW() WHERE id = ?")
        .bind(id)
        .execute(&state.pool)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Add a customer notification flow
///
/// Making it the default unsets the calendar's other default.
#[utoipa::path(
    post,
    path = "/v1/schedule/notifications/flows",
    operation_id = "create_schedule_notification_flow",
    tag = "schedule",
    request_body = NotificationFlowInput,
    responses(
        (status = 201, description = "Added", body = Created),
        (status = 400, description = "Invalid field, unknown calendar or template, or texting not set up", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create_flow(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<NotificationFlowInput>,
) -> ApiResult<(StatusCode, Json<Created>)> {
    let (name, times) = check_flow(&state.pool, user.company_id, &input).await?;
    let mut tx = state.pool.begin().await?;
    if input.is_default {
        unset_defaults(&mut tx, user.company_id, input.calendar_id).await?;
    }
    let id = sqlx::query(
        "INSERT INTO schedule_notification_flows
           (company_id, calendar_id, name, is_default, is_active, created_by)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(user.company_id)
    .bind(input.calendar_id)
    .bind(name)
    .bind(input.is_default)
    .bind(input.is_active)
    .bind(user.id)
    .execute(&mut *tx)
    .await?
    .last_insert_id();
    let flow_id = i32::try_from(id).map_err(|_| ApiError::Internal("flow id".into()))?;
    save_steps(
        &mut tx,
        user.company_id,
        flow_id,
        user.id,
        &input.steps,
        times,
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(Created { id })))
}

/// Edit a customer notification flow
///
/// Steps sent in `steps` with their id are kept (and not sent again); the
/// others are added, and steps left out are deleted.
#[utoipa::path(
    put,
    path = "/v1/schedule/notifications/flows/{id}",
    operation_id = "update_schedule_notification_flow",
    tag = "schedule",
    params(("id" = i32, Path, description = "Flow id")),
    request_body = NotificationFlowInput,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Invalid field, unknown calendar or template, or texting not set up", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such flow in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update_flow(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<NotificationFlowInput>,
) -> ApiResult<StatusCode> {
    find(
        &state.pool,
        "schedule_notification_flows",
        user.company_id,
        id,
    )
    .await?;
    let (name, times) = check_flow(&state.pool, user.company_id, &input).await?;
    let mut tx = state.pool.begin().await?;
    if input.is_default {
        unset_defaults(&mut tx, user.company_id, input.calendar_id).await?;
    }
    sqlx::query(
        "UPDATE schedule_notification_flows
            SET calendar_id = ?, name = ?, is_default = ?, is_active = ?
          WHERE id = ?",
    )
    .bind(input.calendar_id)
    .bind(name)
    .bind(input.is_default)
    .bind(input.is_active)
    .bind(id)
    .execute(&mut *tx)
    .await?;
    save_steps(&mut tx, user.company_id, id, user.id, &input.steps, times).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Delete a customer notification flow
///
/// Events that had it get none of its messages from then on.
#[utoipa::path(
    delete,
    path = "/v1/schedule/notifications/flows/{id}",
    operation_id = "delete_schedule_notification_flow",
    tag = "schedule",
    params(("id" = i32, Path, description = "Flow id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such flow in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_flow(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    find(
        &state.pool,
        "schedule_notification_flows",
        user.company_id,
        id,
    )
    .await?;
    sqlx::query("UPDATE schedule_notification_flows SET deleted_at = NOW() WHERE id = ?")
        .bind(id)
        .execute(&state.pool)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /v1/schedule/notifications/active`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ActiveQuery {
    /// An event of the company whose choices to include.
    #[serde(default)]
    pub event_id: Option<i32>,
}

/// `GET /v1/schedule/notifications/active`.
#[derive(Debug, Serialize, ToSchema)]
pub struct ActiveFlows {
    /// The flows turned on, of every calendar, oldest first.
    pub flows: Vec<NotificationFlow>,
    /// The templates their steps send.
    pub templates: Vec<NotificationTemplate>,
    /// Default flows turned off for `event_id`.
    pub skipped_flow_ids: Vec<i32>,
    /// Other flows picked for `event_id`.
    pub picked_flow_ids: Vec<i32>,
}

/// List the notification flows for the event form
///
/// Every calendar's flows turned on: the default an event gets, and the
/// others it can pick instead or as well; with `event_id`, whether it turned
/// the default off and which others it picked. Texts are left out while
/// texting is not set up.
#[utoipa::path(
    get,
    path = "/v1/schedule/notifications/active",
    operation_id = "list_active_schedule_notifications",
    tag = "schedule",
    params(ActiveQuery),
    responses(
        (status = 200, description = "The flows", body = ActiveFlows),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn active(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Query(query): Query<ActiveQuery>,
) -> ApiResult<Json<ActiveFlows>> {
    let pool = &state.pool;
    let sms_enabled = sms_enabled(pool, user.company_id).await?;
    let (skipped_flow_ids, picked_flow_ids) = match query.event_id {
        Some(event_id) => (
            event_flow_ids(pool, "event_notification_skips", user.company_id, event_id).await?,
            event_flow_ids(pool, "event_notification_picks", user.company_id, event_id).await?,
        ),
        None => (Vec::new(), Vec::new()),
    };
    Ok(Json(ActiveFlows {
        flows: load_flows(pool, user.company_id, sms_enabled, true).await?,
        templates: load_templates(pool, user.company_id, sms_enabled).await?,
        skipped_flow_ids,
        picked_flow_ids,
    }))
}

/// The company's flow ids an event has in `table` (skips or picks).
async fn event_flow_ids(
    pool: &MySqlPool,
    table: &'static str,
    company_id: i32,
    event_id: i32,
) -> Result<Vec<i32>, sqlx::Error> {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT k.flow_id FROM {table} k
           JOIN schedule_notification_flows f ON f.id = k.flow_id AND f.company_id = ?
          WHERE k.event_id = ? ORDER BY k.flow_id"
    )))
    .bind(company_id)
    .bind(event_id)
    .fetch_all(pool)
    .await
}

/// Replaces the default flows turned off for an event; ids not of the
/// company's flows are ignored.
pub async fn save_event_skips(
    pool: &MySqlPool,
    company_id: i32,
    event_id: i32,
    flow_ids: &[i32],
) -> Result<(), sqlx::Error> {
    save_event_flows(
        pool,
        "event_notification_skips",
        company_id,
        event_id,
        flow_ids,
    )
    .await
}

/// Replaces the other flows picked for an event; ids not of the company's
/// flows are ignored.
pub async fn save_event_picks(
    pool: &MySqlPool,
    company_id: i32,
    event_id: i32,
    flow_ids: &[i32],
) -> Result<(), sqlx::Error> {
    save_event_flows(
        pool,
        "event_notification_picks",
        company_id,
        event_id,
        flow_ids,
    )
    .await
}

async fn save_event_flows(
    pool: &MySqlPool,
    table: &'static str,
    company_id: i32,
    event_id: i32,
    flow_ids: &[i32],
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DELETE FROM {table} WHERE event_id = ?"
    )))
    .bind(event_id)
    .execute(&mut *tx)
    .await?;
    for flow_id in flow_ids {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT IGNORE INTO {table} (event_id, flow_id)
             SELECT ?, id FROM schedule_notification_flows WHERE id = ? AND company_id = ?"
        )))
        .bind(event_id)
        .bind(flow_id)
        .bind(company_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await
}

/// SQL: the flow `f` goes on the event `e`: its calendar's, turned on, and
/// the default not turned off for it, or picked for it.
pub const FLOW_APPLIES: &str =
    "f.calendar_id = e.calendar_id AND f.is_active = 1 AND f.deleted_at IS NULL
    AND IF(f.is_default = 1,
           NOT EXISTS (SELECT 1 FROM event_notification_skips k
                        WHERE k.event_id = e.id AND k.flow_id = f.id),
           EXISTS (SELECT 1 FROM event_notification_picks p
                    WHERE p.event_id = e.id AND p.flow_id = f.id))";

/// When an appointment starts: all-day ones at store midnight of their day.
fn effective_start(start: NaiveDateTime, all_day: bool) -> DateTime<Utc> {
    if all_day {
        store_time(start.date(), NaiveTime::MIN).unwrap_or_else(|| start.and_utc())
    } else {
        start.and_utc()
    }
}

fn store_time(day: NaiveDate, time: NaiveTime) -> Option<DateTime<Utc>> {
    Indianapolis
        .from_local_datetime(&day.and_time(time))
        .earliest()
        .map(|at| at.with_timezone(&Utc))
}

/// When a rule's message for an appointment at `start` goes out.
pub fn send_at(
    value: i32,
    unit: OffsetUnit,
    send_time: Option<NaiveTime>,
    start: NaiveDateTime,
    all_day: bool,
) -> Option<DateTime<Utc>> {
    let begins = effective_start(start, all_day);
    match (unit, send_time) {
        (OffsetUnit::Days, Some(time)) => {
            let day = if all_day {
                start.date()
            } else {
                begins.with_timezone(&Indianapolis).date_naive()
            };
            store_time(day - Duration::days(i64::from(value)), time)
        }
        _ => Some(begins - Duration::minutes(unit.minutes(value))),
    }
}

/// `{{appointment.*}}` filled in; the other variables are left for the
/// shared template filler.
pub fn fill_appointment(
    text: &str,
    start: NaiveDateTime,
    all_day: bool,
    location: Option<&str>,
) -> String {
    let (date, time) = if all_day {
        (
            start.date().format("%A, %B %-d").to_string(),
            "All day".to_string(),
        )
    } else {
        let start = start.and_utc().with_timezone(&Indianapolis);
        (
            start.format("%A, %B %-d").to_string(),
            start.format("%-I:%M %p").to_string(),
        )
    };
    let address = location.map(str::trim).unwrap_or_default();
    let mut out = text.to_string();
    for (key, value) in [
        ("appointment.date", date.as_str()),
        ("appointment.time", time.as_str()),
        ("appointment.address", address),
    ] {
        if value.is_empty() {
            continue;
        }
        for spelled in [format!("{{{{{key}}}}}"), format!("{{{{ {key} }}}}")] {
            out = out.replace(&spelled, value);
        }
    }
    out
}

/// Plain text as email HTML: blank lines split paragraphs.
fn email_html(text: &str) -> String {
    text.replace("\r\n", "\n")
        .split("\n\n")
        .map(str::trim)
        .filter(|paragraph| !paragraph.is_empty())
        .map(|paragraph| format!("<p>{}</p>", escape_html(paragraph).replace('\n', "<br>")))
        .collect()
}

/// What a run of the rules did.
#[derive(Debug, Default)]
pub struct RuleRun {
    pub processed: usize,
    pub sent: usize,
    pub failed: usize,
}

#[derive(sqlx::FromRow)]
struct DueRule {
    id: i32,
    company_id: i32,
    channel: String,
    offset_value: i32,
    offset_unit: String,
    send_time: Option<String>,
    subject: Option<String>,
    body: String,
    created_at: DateTime<Utc>,
}

#[derive(sqlx::FromRow)]
struct Appointment {
    id: i32,
    start_date: NaiveDateTime,
    all_day: bool,
    location: Option<String>,
    created_date: Option<DateTime<Utc>>,
    created_user_id: i32,
    customer_id: Option<i32>,
}

/// Records how the send went; `false` when another run already has it.
async fn claim(pool: &MySqlPool, rule: i32, event: &Appointment) -> Result<bool, sqlx::Error> {
    let done = sqlx::query(
        "INSERT IGNORE INTO schedule_notification_sends (rule_id, event_id, start_date, status)
         VALUES (?, ?, ?, 'sending')",
    )
    .bind(rule)
    .bind(event.id)
    .bind(event.start_date)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

async fn finish(
    pool: &MySqlPool,
    rule: i32,
    event: &Appointment,
    status: &str,
    error: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE schedule_notification_sends
            SET status = ?, error_message = ?, sent_at = IF(? = 'sent', UTC_TIMESTAMP(), NULL)
          WHERE rule_id = ? AND event_id = ? AND start_date = ?",
    )
    .bind(status)
    .bind(error)
    .bind(status)
    .bind(rule)
    .bind(event.id)
    .bind(event.start_date)
    .execute(pool)
    .await?;
    Ok(())
}

/// The rule's subject and body for this appointment, or why it cannot go.
async fn render(
    pool: &MySqlPool,
    rule: &DueRule,
    event: &Appointment,
    customer_id: i32,
    user_id: i32,
) -> Result<Result<(Option<String>, String), &'static str>, sqlx::Error> {
    let Ok(data) = common::crud::template::fetch_template_variable_data(
        pool,
        user_id,
        None,
        Some(customer_id),
        rule.company_id,
    )
    .await
    else {
        return Ok(Err("variables_failed"));
    };
    let fill = |text: &str| {
        let text = fill_appointment(
            text,
            event.start_date,
            event.all_day,
            event.location.as_deref(),
        );
        common::utils::template::replace_template_variables(&text, &data)
    };
    let body = fill(&rule.body);
    let subject = rule.subject.as_deref().map(fill);
    let unfilled = common::utils::template::placeholder_keys(&body)
        .into_iter()
        .chain(
            subject
                .as_deref()
                .map(common::utils::template::placeholder_keys)
                .unwrap_or_default(),
        )
        .next()
        .is_some();
    if unfilled {
        return Ok(Err("unfilled_variables"));
    }
    Ok(Ok((subject, body)))
}

/// Sends one rule's message for one appointment: the error code when it
/// did not go out.
async fn deliver(
    state: &AppState,
    rule: &DueRule,
    event: &Appointment,
    customer_id: i32,
) -> Result<Option<&'static str>, sqlx::Error> {
    let pool = &state.pool;
    let sales_rep: Option<Option<i32>> = sqlx::query_scalar(
        "SELECT CAST(u.id AS SIGNED) FROM customers c JOIN users u ON u.id = c.sales_rep AND u.is_deleted = 0
          WHERE c.id = ? AND c.company_id = ?",
    )
    .bind(customer_id)
    .bind(rule.company_id)
    .fetch_optional(pool)
    .await?;
    let user_id = sales_rep.flatten().unwrap_or(event.created_user_id);
    let (subject, body) = match render(pool, rule, event, customer_id, user_id).await? {
        Ok(message) => message,
        Err(code) => return Ok(Some(code)),
    };
    if Channel::parse(&rule.channel) == Channel::Sms {
        let features = company_features(pool, rule.company_id).await?;
        // Telnyx when the company texts through it and the office manager
        // has a line there; then the company's provider first; with both set
        // up, the other one when the office manager only has a line there.
        let mut providers: Vec<SmsProvider> = Vec::new();
        if features.telnyx_sms {
            providers.push(SmsProvider::Telnyx);
        }
        if features.sms_provider() == Some(SmsProvider::Ringcentral) {
            providers.push(SmsProvider::Ringcentral);
        }
        if features.cloudtalk {
            providers.push(SmsProvider::Cloudtalk);
        }
        if features.ringcentral && !providers.contains(&SmsProvider::Ringcentral) {
            providers.push(SmsProvider::Ringcentral);
        }
        if providers.is_empty() {
            return Ok(Some("sms_unavailable"));
        }
        let mut sender = None;
        for provider in providers {
            if let Some(manager) = office_manager(pool, rule.company_id, provider).await?
                && let Some(seat) = crate::routes::events::notify::manager_seat(
                    pool,
                    rule.company_id,
                    provider,
                    &manager,
                )
                .await?
            {
                sender = Some((provider, manager, seat));
                break;
            }
        }
        let Some((provider, manager, seat)) = sender else {
            return Ok(Some("no_sms_sender"));
        };
        let Some(phone_digits) = customer_phone(pool, rule.company_id, customer_id)
            .await?
            .map(|phone| phone10(&phone))
            .filter(|digits| digits.len() == 10)
        else {
            return Ok(Some("no_phone"));
        };
        let sms = OutgoingSms {
            provider: provider.as_str().to_string(),
            company_id: rule.company_id,
            user_id: manager.id,
            agent_id: manager.agent_id,
            phone_digits,
            text: body,
            attachments: Vec::new(),
            is_automated: true,
            idempotency_key: Some(format!(
                "schedule-rule-{}-{}-{}",
                rule.id,
                event.id,
                event.start_date.and_utc().timestamp()
            )),
        };
        return Ok(
            match crate::routes::sms::send::send_text(state, provider, &seat, &sms).await {
                Ok(_) => None,
                Err(failure) => {
                    tracing::warn!(error = %failure.error, rule_id = rule.id, "schedule notification text failed");
                    Some("send_error")
                }
            },
        );
    }
    let Some(email) = customer_email(pool, customer_id).await? else {
        return Ok(Some("no_email"));
    };
    let (from_name, from_address) = sender_for(pool, rule.company_id, customer_id, user_id).await?;
    let message = OutgoingEmail {
        from_name,
        from_address,
        to: vec![email],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: subject.unwrap_or_default(),
        body: email_html(&body),
        signature: None,
        reply_to: Vec::new(),
        in_reply_to: None,
        references: Vec::new(),
        attachments: Vec::new(),
        transactional: true,
    };
    Ok(match state.webhooks.send_email(&message).await {
        Ok(_) => None,
        Err(err) => {
            tracing::warn!(error = ?err, rule_id = rule.id, "schedule notification email failed");
            Some("send_error")
        }
    })
}

/// Sends every flow step's messages that are due. A message whose time
/// passed before the appointment was booked (or the step was added) is
/// skipped.
pub async fn send_due(state: &AppState) -> Result<RuleRun, sqlx::Error> {
    let pool = &state.pool;
    let mut run = RuleRun::default();
    let rules: Vec<DueRule> = sqlx::query_as(
        "SELECT r.id, r.company_id, CAST(m.channel AS CHAR) AS channel, r.offset_value,
                CAST(r.offset_unit AS CHAR) AS offset_unit, TIME_FORMAT(r.send_time, '%H:%i') AS send_time,
                m.subject, m.body, r.created_at
           FROM schedule_notification_rules r
           JOIN schedule_notification_flows f ON f.id = r.flow_id
            AND f.is_active = 1 AND f.deleted_at IS NULL
           JOIN calendars cal ON cal.id = f.calendar_id AND cal.deleted_at IS NULL
           JOIN schedule_notification_templates m ON m.id = r.template_id AND m.deleted_at IS NULL
          WHERE r.deleted_at IS NULL
          ORDER BY r.id LIMIT ?",
    )
    .bind(RULE_BATCH)
    .fetch_all(pool)
    .await?;
    let now = Utc::now();
    for rule in rules {
        let unit = OffsetUnit::parse(&rule.offset_unit);
        let time = rule
            .send_time
            .as_deref()
            .and_then(|time| NaiveTime::parse_from_str(time, "%H:%M").ok());
        // Two days more than the offset covers a time of day and all-day events.
        let window = unit.minutes(rule.offset_value) + 2 * 24 * 60;
        let events: Vec<Appointment> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT e.id, e.start_date, COALESCE(e.all_day, 0) <> 0 AS all_day,
                    e.location, e.created_date, e.created_user_id,
                    CAST(COALESCE(tc.id, sc.id) AS SIGNED) AS customer_id
               FROM schedule_notification_rules r
               JOIN schedule_notification_flows f ON f.id = r.flow_id
               JOIN events e ON {FLOW_APPLIES}
               LEFT JOIN customers tc ON TRIM(e.title) REGEXP '^[0-9]+$'
                AND tc.id = CAST(TRIM(e.title) AS UNSIGNED)
                AND tc.company_id = r.company_id AND tc.deleted_at IS NULL
               LEFT JOIN sales s ON s.id = e.sale_id AND s.company_id = r.company_id
               LEFT JOIN customers sc ON sc.id = s.customer_id
                AND sc.company_id = r.company_id AND sc.deleted_at IS NULL
              WHERE r.id = ? AND e.deleted_date IS NULL AND e.unscheduled = 0 AND NULLIF(TRIM(e.rrule), '') IS NULL
                AND e.start_date > UTC_TIMESTAMP() - INTERVAL 1 DAY
                AND e.start_date <= UTC_TIMESTAMP() + INTERVAL ? MINUTE
                AND NOT EXISTS (SELECT 1 FROM schedule_notification_sends n
                                 WHERE n.rule_id = r.id AND n.event_id = e.id AND n.start_date = e.start_date)
              ORDER BY e.start_date, e.id LIMIT ?"
        )))
        .bind(rule.id)
        .bind(window)
        .bind(EVENT_BATCH)
        .fetch_all(pool)
        .await?;
        for event in events {
            if effective_start(event.start_date, event.all_day) <= now {
                continue;
            }
            let Some(at) = send_at(
                rule.offset_value,
                unit,
                time,
                event.start_date,
                event.all_day,
            ) else {
                continue;
            };
            if at > now {
                continue;
            }
            if !claim(pool, rule.id, &event).await? {
                continue;
            }
            let booked = event
                .created_date
                .map_or(rule.created_at, |created| created.max(rule.created_at));
            let skip = if at < booked {
                Some("booked_after_send_time")
            } else if event.customer_id.is_none() {
                Some("no_customer")
            } else {
                None
            };
            if let Some(code) = skip {
                finish(pool, rule.id, &event, "skipped", Some(code)).await?;
                continue;
            }
            run.processed += 1;
            let customer_id = event.customer_id.unwrap_or_default();
            let outcome = match deliver(state, &rule, &event, customer_id).await {
                Ok(outcome) => outcome,
                Err(err) => {
                    // One failure must not hold up the messages behind it.
                    tracing::error!(error = %err, rule_id = rule.id, event_id = event.id, "schedule notification failed");
                    Some("send_error")
                }
            };
            match outcome {
                None => {
                    finish(pool, rule.id, &event, "sent", None).await?;
                    run.sent += 1;
                }
                Some(code) => {
                    finish(pool, rule.id, &event, "failed", Some(code)).await?;
                    run.failed += 1;
                }
            }
        }
    }
    Ok(run)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M").unwrap()
    }

    #[test]
    fn rules_send_before_the_start() {
        // 10:00 Indianapolis (EDT) on Oct 20 = 14:00 UTC.
        let start = at("2026-10-20 14:00");
        assert_eq!(
            send_at(1, OffsetUnit::Days, None, start, false),
            Some(at("2026-10-19 14:00").and_utc())
        );
        assert_eq!(
            send_at(2, OffsetUnit::Hours, None, start, false),
            Some(at("2026-10-20 12:00").and_utc())
        );
        // 1 day before at 9:00 AM store time = 13:00 UTC on Oct 19.
        let nine = NaiveTime::from_hms_opt(9, 0, 0);
        assert_eq!(
            send_at(1, OffsetUnit::Days, nine, start, false),
            Some(at("2026-10-19 13:00").and_utc())
        );
        // 1 AM UTC Oct 21 is still Oct 20 in Indianapolis.
        assert_eq!(
            send_at(1, OffsetUnit::Days, nine, at("2026-10-21 01:00"), false),
            Some(at("2026-10-19 13:00").and_utc())
        );
        // An all-day event's day is its date.
        assert_eq!(
            send_at(1, OffsetUnit::Days, nine, at("2026-10-20 00:00"), true),
            Some(at("2026-10-19 13:00").and_utc())
        );
    }

    #[test]
    fn appointment_variables_are_filled() {
        let text = "Hi, see you {{appointment.date}} at {{ appointment.time }} at {{appointment.address}}. {{customer.first_name}}";
        assert_eq!(
            fill_appointment(text, at("2030-01-15 15:00"), false, Some(" 1 Main St ")),
            "Hi, see you Tuesday, January 15 at 10:00 AM at 1 Main St. {{customer.first_name}}"
        );
        // No address: the variable stays, so the send is held back.
        assert!(
            fill_appointment(text, at("2030-01-15 15:00"), true, None)
                .contains("All day at {{appointment.address}}")
        );
    }

    #[test]
    fn email_paragraphs() {
        assert_eq!(
            email_html("Hi <Al>,\nline two\n\nBye"),
            "<p>Hi &lt;Al&gt;,<br>line two</p><p>Bye</p>"
        );
    }
}
