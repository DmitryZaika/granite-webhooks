//! Admin > Quotes > Automation > Event templates: extra events a sold quote
//! adds to the schedule. Each template names a calendar and a title made of a
//! variable (the quote's or the customer's name) and the text after it, so
//! selling "Ostrom Kitchen" with a "Quote name" + "Plumbing" template adds an
//! unscheduled "Ostrom Kitchen - Plumbing" event to that calendar
//! (`quote_pricing::add_waiting_jobs`). Undo sale deletes them with the
//! sale's other jobs.
//!
//! - `GET|POST /v1/quote-sale-events`, `PUT|DELETE /v1/quote-sale-events/{id}`

use crate::auth::AdminUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::openapi::{OpenApiRouter, routes};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, MySqlPool};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_sale_events, create_sale_event))
        .routes(routes!(update_sale_event, delete_sale_event))
}

/// Longest text after the variable.
const MAX_TEXT: usize = 200;

/// What an event template's title starts with.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TitleVariable {
    /// The quote's name; a quote without one uses the customer's name.
    #[default]
    QuoteName,
    /// The customer's name; a customer without one uses the quote's name.
    CustomerName,
}

impl TitleVariable {
    const fn as_str(self) -> &'static str {
        match self {
            Self::QuoteName => "quote_name",
            Self::CustomerName => "customer_name",
        }
    }

    fn parse(text: &str) -> Self {
        if text == "customer_name" {
            Self::CustomerName
        } else {
            Self::QuoteName
        }
    }
}

/// An event a sold quote adds to the schedule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct QuoteSaleEvent {
    /// Template id.
    pub id: i32,
    /// The calendar the event goes on.
    pub calendar_id: i32,
    /// That calendar's name.
    pub calendar_name: String,
    /// What the event's title starts with.
    pub title_variable: TitleVariable,
    /// What follows the variable, e.g. "Plumbing"; empty: the variable alone.
    pub title_text: String,
}

#[derive(FromRow)]
struct SaleEventRow {
    id: i32,
    calendar_id: i32,
    calendar_name: String,
    title_variable: String,
    title_text: String,
}

impl From<SaleEventRow> for QuoteSaleEvent {
    fn from(row: SaleEventRow) -> Self {
        Self {
            id: row.id,
            calendar_id: row.calendar_id,
            calendar_name: row.calendar_name,
            title_variable: TitleVariable::parse(&row.title_variable),
            title_text: row.title_text,
        }
    }
}

/// Body of creating or saving an event template.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QuoteSaleEventInput {
    /// A calendar of the company (`GET /v1/calendars`).
    pub calendar_id: i32,
    /// What the title starts with (default `quote_name`).
    #[serde(default)]
    pub title_variable: TitleVariable,
    /// What follows the variable, e.g. "Plumbing" (at most 200 characters;
    /// blank: the variable alone).
    #[serde(default)]
    pub title_text: String,
}

/// `POST /v1/quote-sale-events`.
#[derive(Debug, Serialize, ToSchema)]
pub struct CreatedSaleEvent {
    /// The new template.
    pub id: u64,
}

/// The title a template gives a sold quote's event: the variable's name, then
/// " - " and the text. `quote_name` and `customer_name` are trimmed and
/// `None` when blank; `fallback` stands in when both are.
#[must_use]
pub fn sale_event_title(
    variable: TitleVariable,
    text: &str,
    quote_name: Option<&str>,
    customer_name: Option<&str>,
    fallback: &str,
) -> String {
    let (first, second) = match variable {
        TitleVariable::QuoteName => (quote_name, customer_name),
        TitleVariable::CustomerName => (customer_name, quote_name),
    };
    let name = first.or(second).unwrap_or(fallback);
    let text = text.trim();
    let title = if text.is_empty() {
        name.to_string()
    } else {
        format!("{name} - {text}")
    };
    title.chars().take(255).collect()
}

/// The company's event templates on calendars it still has, in the order they
/// were added.
pub async fn sale_events(
    pool: &MySqlPool,
    company_id: i32,
) -> Result<Vec<QuoteSaleEvent>, sqlx::Error> {
    let rows: Vec<SaleEventRow> = sqlx::query_as(
        "SELECT t.id, t.calendar_id, c.name AS calendar_name,
                CAST(t.title_variable AS CHAR) AS title_variable, t.title_text
           FROM quote_sale_event_templates t
           JOIN calendars c ON c.id = t.calendar_id AND c.company_id = t.company_id
                           AND c.deleted_at IS NULL
          WHERE t.company_id = ?
          ORDER BY t.id",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(QuoteSaleEvent::from).collect())
}

/// The text trimmed and checked; the calendar the company's.
async fn check(
    pool: &MySqlPool,
    company_id: i32,
    input: &QuoteSaleEventInput,
) -> ApiResult<String> {
    let text = input.title_text.trim().to_string();
    if text.chars().count() > MAX_TEXT {
        return Err(ApiError::BadRequest(format!(
            "The text after the variable can be at most {MAX_TEXT} characters"
        )));
    }
    let calendar: Option<i32> = sqlx::query_scalar(
        "SELECT id FROM calendars WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(input.calendar_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    if calendar.is_none() {
        return Err(ApiError::BadRequest("Choose a calendar".into()));
    }
    Ok(text)
}

/// 404 unless template `id` is the company's.
async fn find(pool: &MySqlPool, company_id: i32, id: i32) -> ApiResult<()> {
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT id FROM quote_sale_event_templates WHERE id = ? AND company_id = ?",
    )
    .bind(id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    found.map(|_| ()).ok_or(ApiError::NotFound)
}

/// List the events a sold quote adds to the schedule
///
/// Returns the company's event templates (Admin > Quotes > Automation), in
/// the order they were added. A template whose calendar was deleted is left
/// out, and selling a quote skips it. Change them with
/// `POST /v1/quote-sale-events` and `PUT|DELETE /v1/quote-sale-events/{id}`.
#[crate::openapi::path(
    get,
    path = "/v1/quote-sale-events",
    operation_id = "list_quote_sale_events",
    tag = "quotes",
    responses(
        (status = 200, description = "The company's event templates", body = Vec<QuoteSaleEvent>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list_sale_events(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
) -> ApiResult<Json<Vec<QuoteSaleEvent>>> {
    Ok(Json(sale_events(&state.pool, user.company_id).await?))
}

/// Add an event a sold quote adds to the schedule
///
/// From then on, marking a quote Sold adds one unscheduled, Not ready event
/// to `calendar_id`, titled with the variable and the text (e.g. "Ostrom
/// Kitchen - Plumbing"), at the customer's address, for the schedule manager
/// to give a day. Quotes sold before are not changed.
#[crate::openapi::path(
    post,
    path = "/v1/quote-sale-events",
    operation_id = "create_quote_sale_event",
    tag = "quotes",
    request_body = QuoteSaleEventInput,
    responses(
        (status = 201, description = "Added", body = CreatedSaleEvent),
        (status = 400, description = "Text too long, or no such calendar in the company", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create_sale_event(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<QuoteSaleEventInput>,
) -> ApiResult<(StatusCode, Json<CreatedSaleEvent>)> {
    let text = check(&state.pool, user.company_id, &input).await?;
    let id = sqlx::query(
        "INSERT INTO quote_sale_event_templates
           (company_id, calendar_id, title_variable, title_text, created_by)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(user.company_id)
    .bind(input.calendar_id)
    .bind(input.title_variable.as_str())
    .bind(text)
    .bind(user.id)
    .execute(&state.pool)
    .await?
    .last_insert_id();
    Ok((StatusCode::CREATED, Json(CreatedSaleEvent { id })))
}

/// Edit an event a sold quote adds to the schedule
///
/// Quotes sold from then on get the new calendar and title; events already
/// added stay as they are.
#[crate::openapi::path(
    put,
    path = "/v1/quote-sale-events/{id}",
    operation_id = "update_quote_sale_event",
    tag = "quotes",
    params(("id" = i32, Path, description = "Event template id")),
    request_body = QuoteSaleEventInput,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Text too long, or no such calendar in the company", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such template in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update_sale_event(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<QuoteSaleEventInput>,
) -> ApiResult<StatusCode> {
    find(&state.pool, user.company_id, id).await?;
    let text = check(&state.pool, user.company_id, &input).await?;
    sqlx::query(
        "UPDATE quote_sale_event_templates
            SET calendar_id = ?, title_variable = ?, title_text = ?
          WHERE id = ? AND company_id = ?",
    )
    .bind(input.calendar_id)
    .bind(input.title_variable.as_str())
    .bind(text)
    .bind(id)
    .bind(user.company_id)
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Delete an event a sold quote adds to the schedule
///
/// Quotes sold from then on no longer get it; events already added stay.
#[crate::openapi::path(
    delete,
    path = "/v1/quote-sale-events/{id}",
    operation_id = "delete_quote_sale_event",
    tag = "quotes",
    params(("id" = i32, Path, description = "Event template id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such template in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_sale_event(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    find(&state.pool, user.company_id, id).await?;
    sqlx::query("DELETE FROM quote_sale_event_templates WHERE id = ? AND company_id = ?")
        .bind(id)
        .bind(user.company_id)
        .execute(&state.pool)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_title_is_the_variable_then_the_text() {
        let title = |variable, text, quote, customer| {
            sale_event_title(variable, text, quote, customer, "Customer #7")
        };
        let q = TitleVariable::QuoteName;
        let c = TitleVariable::CustomerName;
        assert_eq!(
            title(q, " Plumbing ", Some("Ostrom Kitchen"), Some("Jo")),
            "Ostrom Kitchen - Plumbing"
        );
        assert_eq!(
            title(c, "Plumbing", Some("Ostrom Kitchen"), Some("Jo")),
            "Jo - Plumbing"
        );
        // Blank text: the variable alone; a missing name: the other one, else the fallback.
        assert_eq!(
            title(q, "  ", Some("Ostrom Kitchen"), None),
            "Ostrom Kitchen"
        );
        assert_eq!(title(q, "Sink", None, Some("Jo")), "Jo - Sink");
        assert_eq!(title(c, "Sink", Some("Ostrom"), None), "Ostrom - Sink");
        assert_eq!(title(c, "Sink", None, None), "Customer #7 - Sink");
        assert_eq!(
            title(q, &"x".repeat(200), Some(&"n".repeat(100)), None)
                .chars()
                .count(),
            255
        );
    }
}
