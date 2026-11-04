//! Booking an in-home estimate: the event on the Estimates calendar, the
//! customer's confirmation, and the appointment reminders.
//! Remix: `api.schedule-estimate.ts`.

use super::history::{History, record};
use super::notify::{Booking, Channel, NotifyKind, customer_email, customer_phone, notify_booking};
use super::reminders::{self, Appointment};
use super::writes::{EventInput, check};
use crate::auth::ShopOrEmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::routes::schedule::settings::estimates_calendar_id;
use crate::routes::sms::provider::phone10;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use chrono::Utc;
use serde::{Deserialize, Serialize};

/// The onboarding tour's sample customer: booking it saves nothing.
const SAMPLE_CUSTOMER_ID: i32 = 990_000_021;

/// Body of `POST /v1/events/estimates`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct EstimateInput {
    /// The customer the estimate is for.
    pub customer_id: i32,
    /// The deal it is for (the rep's reminders link to it; default the
    /// customer's newest deal).
    #[serde(default)]
    pub deal_id: Option<i32>,
    /// Where to email the confirmation when it cannot be texted (default the
    /// customer's primary email).
    #[serde(default)]
    pub customer_email: Option<String>,
    /// The event; `calendar_id` must be the company's estimates calendar
    /// (`GET /v1/schedule/settings`).
    #[serde(flatten)]
    pub event: EventInput,
}

/// `POST /v1/events/estimates`.
#[derive(Debug, Serialize, ToSchema)]
pub struct BookedEstimate {
    /// The new event (0 for the onboarding sample customer).
    pub id: u64,
    /// `sms` or `email`; null when the confirmation could not be sent.
    pub notification_channel: Option<&'static str>,
}

/// The customer's name and the calendar, once the customer is reachable and
/// the calendar is the company's Estimates calendar.
async fn check_booking(
    pool: &sqlx::MySqlPool,
    company_id: i32,
    input: &EstimateInput,
) -> ApiResult<(String, i32)> {
    let name: Option<Option<String>> = sqlx::query_scalar(
        "SELECT name FROM customers WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(input.customer_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    let customer_name = name.ok_or(ApiError::NotFound)?.unwrap_or_default();
    let typed_email = input
        .customer_email
        .as_deref()
        .map(str::trim)
        .filter(|email| !email.is_empty());
    let has_email =
        typed_email.is_some() || customer_email(pool, input.customer_id).await?.is_some();
    let has_phone = customer_phone(pool, company_id, input.customer_id)
        .await?
        .is_some_and(|phone| phone10(&phone).len() >= 10);
    if !has_email && !has_phone {
        return Err(bad(
            "Customer must have a phone number or email to receive the appointment confirmation.",
        ));
    }
    let calendar_id = input
        .event
        .calendar_id
        .ok_or_else(|| bad("Estimates calendar is required"))?;
    if estimates_calendar_id(pool, company_id).await? != Some(calendar_id) {
        return Err(bad("Estimates calendar not found"));
    }
    Ok((customer_name, calendar_id))
}

/// The customer's reminders and their sales rep's (else the booker's).
async fn schedule_reminders(
    pool: &sqlx::MySqlPool,
    appointment: &Appointment<'_>,
    booker: i32,
    deal_id: Option<i32>,
) -> Result<(), sqlx::Error> {
    let now = Utc::now();
    reminders::schedule_customer(pool, appointment, now).await?;
    let sales_rep: Option<Option<i32>> = sqlx::query_scalar(
        "SELECT CAST(sales_rep AS SIGNED) FROM customers WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(appointment.customer_id)
    .bind(appointment.company_id)
    .fetch_optional(pool)
    .await?;
    let rep = sales_rep.flatten().filter(|id| *id > 0).unwrap_or(booker);
    reminders::schedule_rep(pool, appointment, rep, deal_id, now).await
}

fn bad(message: &str) -> ApiError {
    ApiError::BadRequest(message.into())
}

/// Book an in-home estimate
///
/// Puts the estimate on the Estimates calendar, texts the customer from the
/// office manager's line (else emails them from their sales rep), and
/// schedules the customer's and the rep's reminders. The customer needs a
/// phone or an email.
#[crate::openapi::path(
    post,
    path = "/v1/events/estimates",
    operation_id = "book_estimate",
    tag = "schedule",
    request_body = EstimateInput,
    responses(
        (status = 201, description = "Booked", body = BookedEstimate),
        (status = 400, description = "Invalid field, not the Estimates calendar, or a customer without phone or email", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee, admin or shop manager or worker", body = ErrorBody),
        (status = 404, description = "No such customer in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn book(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
    JsonBody(input): JsonBody<EstimateInput>,
) -> ApiResult<(axum::http::StatusCode, Json<BookedEstimate>)> {
    let created = |id, channel| {
        (
            axum::http::StatusCode::CREATED,
            Json(BookedEstimate {
                id,
                notification_channel: channel,
            }),
        )
    };
    if input.customer_id == SAMPLE_CUSTOMER_ID {
        return Ok(created(0, None));
    }
    let pool = &state.pool;
    let typed_email = input
        .customer_email
        .as_deref()
        .map(str::trim)
        .filter(|email| !email.is_empty());
    let (customer_name, calendar_id) = check_booking(pool, user.company_id, &input).await?;
    let event = check(&input.event)?;
    let id = sqlx::query(
        "INSERT INTO events (title, description, location, start_date, end_date, all_day, color,
            status, notes, created_user_id, customer_id, calendar_id)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(event.title)
    .bind(event.description)
    .bind(event.location)
    .bind(event.start)
    .bind(event.end)
    .bind(input.event.all_day)
    .bind(event.color)
    .bind(event.status)
    .bind(event.notes)
    .bind(user.id)
    .bind(input.customer_id)
    .bind(calendar_id)
    .execute(pool)
    .await?
    .last_insert_id();
    let event_id = i32::try_from(id).map_err(|_| ApiError::Internal("event id".into()))?;
    record(
        pool,
        &History::new(event_id, user.company_id, "created", user.name.as_deref()),
    )
    .await;

    let customer_name = customer_name.trim();
    let channel = notify_booking(
        &state,
        &Booking {
            kind: NotifyKind::Scheduled,
            company_id: user.company_id,
            customer_id: input.customer_id,
            customer_name: if customer_name.is_empty() {
                event.title
            } else {
                customer_name
            },
            user_name: user.name.as_deref(),
            user_id: user.id,
            customer_email: typed_email,
            customer_phone: None,
            start: event.start.and_utc(),
            end: event.end.and_utc(),
            location: event.location,
        },
    )
    .await?;

    let appointment = Appointment {
        event_id,
        company_id: user.company_id,
        customer_id: input.customer_id,
        slug: "estimates",
        start: event.start.and_utc(),
    };
    schedule_reminders(pool, &appointment, user.id, input.deal_id).await?;
    Ok(created(id, channel.map(Channel::as_str)))
}
