//! Admin > Schedule: the company's own texts and emails to customers before
//! their appointments ("1 day before every installation, remind them to
//! disconnect the sink 2 hours ahead").
//!
//! - `GET /v1/schedule/notifications` → the rules, and whether texting is set up
//! - `POST /v1/schedule/notifications`, `PUT`/`DELETE /v1/schedule/notifications/{id}`
//!
//! The appointment-reminders job sends the due ones (`send_due`). An event's
//! customer is the one its title names (a bare customer id), else its sale's.
//! A calendar with an active rule no longer gets the built-in reminders.

use crate::auth::AdminUser;
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
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use chrono_tz::America::Indiana::Indianapolis;
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use sqlx::MySqlPool;
use utoipa::ToSchema;

/// Longest text a rule may send.
const MAX_SMS: usize = 1600;
const MAX_EMAIL: usize = 20_000;
/// Rules checked per run, and events per rule.
const RULE_BATCH: i64 = 500;
const EVENT_BATCH: i64 = 200;

/// How a rule reaches the customer.
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
        if text == "sms" { Self::Sms } else { Self::Email }
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

/// A text or email sent before each appointment on a calendar.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct NotificationRule {
    /// Rule id.
    pub id: i32,
    /// The calendar whose appointments it is sent for.
    pub calendar_id: i32,
    /// `sms` or `email`.
    pub channel: Channel,
    /// How many `offset_unit`s before the appointment it is sent.
    pub offset_value: i32,
    /// `minutes`, `hours` or `days`.
    pub offset_unit: OffsetUnit,
    /// `days` rules: store time (`HH:MM`) it is sent that day; null for
    /// exactly `offset_value` days before the start.
    pub send_time: Option<String>,
    /// Email subject (null for texts); may hold `{{variables}}`.
    pub subject: Option<String>,
    /// The text, plain; may hold `{{variables}}`.
    pub body: String,
    /// Off: kept, but nothing is sent.
    pub is_active: bool,
}

/// `GET /v1/schedule/notifications`.
#[derive(Debug, Serialize, ToSchema)]
pub struct NotificationRules {
    /// Rules, oldest first. Text rules are left out while texting is not set up.
    pub rules: Vec<NotificationRule>,
    /// The company texts through `CloudTalk` or `RingCentral`.
    pub sms_enabled: bool,
}

/// Body of `POST /v1/schedule/notifications` and `PUT /v1/schedule/notifications/{id}`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct NotificationRuleInput {
    /// A calendar of the company.
    pub calendar_id: i32,
    /// `sms` needs `CloudTalk` or `RingCentral`.
    pub channel: Channel,
    /// 1 to 30 days' worth of `offset_unit`.
    pub offset_value: i32,
    pub offset_unit: OffsetUnit,
    /// `days` rules only: `HH:MM` store time.
    #[serde(default)]
    pub send_time: Option<String>,
    /// Required for email (at most 255 characters).
    #[serde(default)]
    pub subject: Option<String>,
    /// Up to 1600 characters for a text.
    pub body: String,
    /// Default on.
    #[serde(default = "on")]
    pub is_active: bool,
}

const fn on() -> bool {
    true
}

/// `POST /v1/schedule/notifications`.
#[derive(Debug, Serialize, ToSchema)]
pub struct CreatedRule {
    /// The new rule.
    pub id: u64,
}

#[derive(sqlx::FromRow)]
struct RuleRow {
    id: i32,
    calendar_id: i32,
    channel: String,
    offset_value: i32,
    offset_unit: String,
    send_time: Option<String>,
    subject: Option<String>,
    body: String,
    is_active: bool,
}

impl From<RuleRow> for NotificationRule {
    fn from(row: RuleRow) -> Self {
        Self {
            id: row.id,
            calendar_id: row.calendar_id,
            channel: Channel::parse(&row.channel),
            offset_value: row.offset_value,
            offset_unit: OffsetUnit::parse(&row.offset_unit),
            send_time: row.send_time,
            subject: row.subject,
            body: row.body,
            is_active: row.is_active,
        }
    }
}

const RULE_COLUMNS: &str = "r.id, r.calendar_id, CAST(r.channel AS CHAR) AS channel, r.offset_value,
    CAST(r.offset_unit AS CHAR) AS offset_unit, TIME_FORMAT(r.send_time, '%H:%i') AS send_time,
    r.subject, r.body, r.is_active <> 0 AS is_active";

async fn sms_enabled(pool: &MySqlPool, company_id: i32) -> Result<bool, sqlx::Error> {
    Ok(company_features(pool, company_id)
        .await?
        .sms_provider()
        .is_some())
}

fn bad(message: &str) -> ApiError {
    ApiError::BadRequest(message.into())
}

/// The input's subject, body and send time, checked.
struct Checked {
    subject: Option<String>,
    body: String,
    send_time: Option<String>,
}

async fn check(
    pool: &MySqlPool,
    company_id: i32,
    input: &NotificationRuleInput,
) -> ApiResult<Checked> {
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
    if input.channel == Channel::Sms && !sms_enabled(pool, company_id).await? {
        return Err(bad("Texting is not set up for this company"));
    }
    if input.offset_value < 1 || input.offset_value > input.offset_unit.max() {
        return Err(bad("Send it between 1 minute and 30 days before"));
    }
    let send_time = match input
        .send_time
        .as_deref()
        .map(str::trim)
        .filter(|time| !time.is_empty())
    {
        None => None,
        Some(_) if input.offset_unit != OffsetUnit::Days => {
            return Err(bad("A time of day only goes with days"));
        }
        Some(time) => {
            let parsed = NaiveTime::parse_from_str(time, "%H:%M")
                .map_err(|_| bad("Time of day must be HH:MM"))?;
            Some(parsed.format("%H:%M:00").to_string())
        }
    };
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
    Ok(Checked {
        subject,
        body,
        send_time,
    })
}

async fn find(pool: &MySqlPool, company_id: i32, id: i32) -> ApiResult<()> {
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT id FROM schedule_notification_rules WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    found.map(|_| ()).ok_or(ApiError::NotFound)
}

/// List the customer notification rules
///
/// Text rules are left out while the company has no `CloudTalk` or
/// `RingCentral`.
#[utoipa::path(
    get,
    path = "/v1/schedule/notifications",
    operation_id = "list_schedule_notifications",
    tag = "schedule",
    responses(
        (status = 200, description = "The rules", body = NotificationRules),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
) -> ApiResult<Json<NotificationRules>> {
    let sms_enabled = sms_enabled(&state.pool, user.company_id).await?;
    let rows: Vec<RuleRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {RULE_COLUMNS} FROM schedule_notification_rules r
           JOIN calendars cal ON cal.id = r.calendar_id AND cal.deleted_at IS NULL
          WHERE r.company_id = ? AND r.deleted_at IS NULL AND (? OR r.channel <> 'sms')
          ORDER BY r.id"
    )))
    .bind(user.company_id)
    .bind(sms_enabled)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(NotificationRules {
        rules: rows.into_iter().map(NotificationRule::from).collect(),
        sms_enabled,
    }))
}

/// Add a customer notification rule
///
/// A text needs `CloudTalk` or `RingCentral`; an email needs a subject.
#[utoipa::path(
    post,
    path = "/v1/schedule/notifications",
    operation_id = "create_schedule_notification",
    tag = "schedule",
    request_body = NotificationRuleInput,
    responses(
        (status = 201, description = "Added", body = CreatedRule),
        (status = 400, description = "Invalid field, unknown calendar, or texting not set up", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<NotificationRuleInput>,
) -> ApiResult<(StatusCode, Json<CreatedRule>)> {
    let checked = check(&state.pool, user.company_id, &input).await?;
    let id = sqlx::query(
        "INSERT INTO schedule_notification_rules
           (company_id, calendar_id, channel, offset_value, offset_unit, send_time, subject, body, is_active, created_by)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(user.company_id)
    .bind(input.calendar_id)
    .bind(input.channel.as_str())
    .bind(input.offset_value)
    .bind(input.offset_unit.as_str())
    .bind(checked.send_time)
    .bind(checked.subject)
    .bind(checked.body)
    .bind(input.is_active)
    .bind(user.id)
    .execute(&state.pool)
    .await?
    .last_insert_id();
    Ok((StatusCode::CREATED, Json(CreatedRule { id })))
}

/// Edit a customer notification rule
#[utoipa::path(
    put,
    path = "/v1/schedule/notifications/{id}",
    operation_id = "update_schedule_notification",
    tag = "schedule",
    params(("id" = i32, Path, description = "Rule id")),
    request_body = NotificationRuleInput,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Invalid field, unknown calendar, or texting not set up", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such rule in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<NotificationRuleInput>,
) -> ApiResult<StatusCode> {
    find(&state.pool, user.company_id, id).await?;
    let checked = check(&state.pool, user.company_id, &input).await?;
    sqlx::query(
        "UPDATE schedule_notification_rules
            SET calendar_id = ?, channel = ?, offset_value = ?, offset_unit = ?, send_time = ?,
                subject = ?, body = ?, is_active = ?
          WHERE id = ?",
    )
    .bind(input.calendar_id)
    .bind(input.channel.as_str())
    .bind(input.offset_value)
    .bind(input.offset_unit.as_str())
    .bind(checked.send_time)
    .bind(checked.subject)
    .bind(checked.body)
    .bind(input.is_active)
    .bind(id)
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Delete a customer notification rule
#[utoipa::path(
    delete,
    path = "/v1/schedule/notifications/{id}",
    operation_id = "delete_schedule_notification",
    tag = "schedule",
    params(("id" = i32, Path, description = "Rule id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such rule in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    find(&state.pool, user.company_id, id).await?;
    sqlx::query("UPDATE schedule_notification_rules SET deleted_at = NOW() WHERE id = ?")
        .bind(id)
        .execute(&state.pool)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

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
        // The company's provider first; with both set up, the other one when
        // the office manager only has a line there.
        let mut providers: Vec<SmsProvider> = Vec::new();
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
            if let Some(manager) = office_manager(pool, rule.company_id, provider).await? {
                sender = Some((provider, manager));
                break;
            }
        }
        let Some((provider, manager)) = sender else {
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
        return Ok(match state.webhooks.send_sms(&sms).await {
            Ok(_) => None,
            Err(failure) => {
                tracing::warn!(error = %failure.error, rule_id = rule.id, "schedule notification text failed");
                Some("send_error")
            }
        });
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

/// Sends every rule's messages that are due. A message whose time passed
/// before the appointment was booked (or the rule was made) is skipped.
pub async fn send_due(state: &AppState) -> Result<RuleRun, sqlx::Error> {
    let pool = &state.pool;
    let mut run = RuleRun::default();
    let rules: Vec<DueRule> = sqlx::query_as(
        "SELECT r.id, r.company_id, CAST(r.channel AS CHAR) AS channel, r.offset_value,
                CAST(r.offset_unit AS CHAR) AS offset_unit, TIME_FORMAT(r.send_time, '%H:%i') AS send_time,
                r.subject, r.body, r.created_at
           FROM schedule_notification_rules r
           JOIN calendars cal ON cal.id = r.calendar_id AND cal.deleted_at IS NULL
          WHERE r.is_active = 1 AND r.deleted_at IS NULL
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
        let events: Vec<Appointment> = sqlx::query_as(
            "SELECT e.id, e.start_date, COALESCE(e.all_day, 0) <> 0 AS all_day,
                    e.location, e.created_date, e.created_user_id,
                    CAST(COALESCE(tc.id, sc.id) AS SIGNED) AS customer_id
               FROM schedule_notification_rules r
               JOIN events e ON e.calendar_id = r.calendar_id
               LEFT JOIN customers tc ON TRIM(e.title) REGEXP '^[0-9]+$'
                AND tc.id = CAST(TRIM(e.title) AS UNSIGNED)
                AND tc.company_id = r.company_id AND tc.deleted_at IS NULL
               LEFT JOIN sales s ON s.id = e.sale_id AND s.company_id = r.company_id
               LEFT JOIN customers sc ON sc.id = s.customer_id
                AND sc.company_id = r.company_id AND sc.deleted_at IS NULL
              WHERE r.id = ? AND e.deleted_date IS NULL AND NULLIF(TRIM(e.rrule), '') IS NULL
                AND e.start_date > UTC_TIMESTAMP() - INTERVAL 1 DAY
                AND e.start_date <= UTC_TIMESTAMP() + INTERVAL ? MINUTE
                AND NOT EXISTS (SELECT 1 FROM schedule_notification_sends n
                                 WHERE n.rule_id = r.id AND n.event_id = e.id AND n.start_date = e.start_date)
              ORDER BY e.start_date, e.id LIMIT ?",
        )
        .bind(rule.id)
        .bind(window)
        .bind(EVENT_BATCH)
        .fetch_all(pool)
        .await?;
        for event in events {
            if effective_start(event.start_date, event.all_day) <= now {
                continue;
            }
            let Some(at) = send_at(rule.offset_value, unit, time, event.start_date, event.all_day)
            else {
                continue;
            };
            if at > now {
                continue;
            }
            if !claim(pool, rule.id, &event).await? {
                continue;
            }
            let booked = event.created_date.map_or(rule.created_at, |created| created.max(rule.created_at));
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
            fill_appointment(
                text,
                at("2030-01-15 15:00"),
                false,
                Some(" 1 Main St ")
            ),
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
