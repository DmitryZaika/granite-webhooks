//! Notes on the sold project behind an event (`sale_project_notes`).
//!
//! A template maker tells the installers or the sales rep what they found on
//! site. The notes belong to the sale, so its template and install events
//! show the same ones.

use crate::auth::ShopOrEmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::MySqlPool;

const AUDIENCES: [&str; 3] = ["installers", "sales_rep", "everyone"];

/// A note on the project.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct ProjectNote {
    /// The note.
    pub id: i32,
    /// `installers`, `sales_rep` or `everyone`.
    pub audience: String,
    /// The text.
    pub content: String,
    /// When it was written.
    pub created_at: DateTime<Utc>,
    /// Author's name.
    pub created_by: Option<String>,
    /// Whether the caller wrote it (only they can delete it).
    pub own: bool,
}

/// Body of `POST /v1/events/{id}/project-notes`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ProjectNoteInput {
    /// 1 to 2000 characters (trimmed).
    pub content: String,
    /// `installers`, `sales_rep` or `everyone` (the default).
    pub audience: Option<String>,
}

const DIRECTIONS: [&str; 12] = [
    "n", "s", "e", "w", "ne", "nw", "se", "sw", "north", "south", "east", "west",
];

/// The house number and street name of an address (`"13636 frenchman"`), so
/// "647 Overpeck Rd" and "647 Overpeck Road, IN" match. `None` when it has no
/// house number ("Carmel, IN 46033").
fn street_key(address: &str) -> Option<String> {
    let lower = address.to_lowercase();
    let mut words = lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty());
    let number = words.next()?;
    if !number.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let street = words.find(|word| !DIRECTIONS.contains(word))?;
    Some(format!("{number} {street}"))
}

/// A live sale of the event's customer.
#[derive(sqlx::FromRow)]
struct CustomerSale {
    id: i32,
    project_address: Option<String>,
    sale_day: Option<String>,
}

/// The project a sale belongs to: its street, else (no address) its sale day.
fn project_key(sale: &CustomerSale) -> String {
    sale.project_address
        .as_deref()
        .and_then(street_key)
        .unwrap_or_else(|| format!("day {}", sale.sale_day.as_deref().unwrap_or("")))
}

/// The sales of the event's project, newest first (`sales` comes newest
/// first). A project is every sale at one address, so a kitchen and an island
/// sold apart show together. An event without its own sale gets the sales at
/// its location; a customer with one project lends it only when the event or
/// the sales have no street to compare. Otherwise none: a builder's other
/// jobs are not this event's.
fn event_project(own_sale: Option<i32>, location: Option<&str>, sales: &[CustomerSale]) -> Vec<i32> {
    let of_project = |key: &str| -> Vec<i32> {
        sales
            .iter()
            .filter(|sale| project_key(sale) == key)
            .map(|sale| sale.id)
            .collect()
    };
    if let Some(own) = own_sale {
        return match sales.iter().find(|sale| sale.id == own) {
            Some(sale) => of_project(&project_key(sale)),
            None => vec![own],
        };
    }
    let event_key = location.and_then(street_key);
    if let Some(key) = &event_key {
        let at_location = of_project(key);
        if !at_location.is_empty() {
            return at_location;
        }
    }
    let Some(first) = sales.first() else {
        return Vec::new();
    };
    let key = project_key(first);
    let one_project = sales.iter().all(|sale| project_key(sale) == key);
    let no_street = sales
        .iter()
        .all(|sale| sale.project_address.as_deref().and_then(street_key).is_none());
    if one_project && (event_key.is_none() || no_street) {
        of_project(&key)
    } else {
        Vec::new()
    }
}

/// The event's project sales, newest first (see [`event_project`]); empty
/// when it has none, 404 when the event is not the company's.
async fn find_event_sales(
    pool: &MySqlPool,
    company_id: i32,
    event_id: i32,
) -> ApiResult<Vec<i32>> {
    let event: Option<(Option<i32>, Option<i32>, Option<String>)> = sqlx::query_as(
        "SELECT e.sale_id, e.customer_id, e.location
           FROM events e JOIN users u ON e.created_user_id = u.id
          WHERE e.id = ? AND u.company_id = ? AND e.deleted_date IS NULL",
    )
    .bind(event_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    let (own_sale, customer_id, location) = event.ok_or(ApiError::NotFound)?;
    let sales: Vec<CustomerSale> = match customer_id {
        Some(customer_id) => {
            sqlx::query_as(
                "SELECT s.id, s.project_address,
                        CAST(DATE(s.sale_date) AS CHAR) AS sale_day
                   FROM sales s
                  WHERE s.customer_id = ? AND s.company_id = ? AND s.cancelled_date IS NULL
                  ORDER BY s.sale_date DESC, s.id DESC",
            )
            .bind(customer_id)
            .bind(company_id)
            .fetch_all(pool)
            .await?
        }
        None => Vec::new(),
    };
    Ok(event_project(own_sale, location.as_deref(), &sales))
}

/// The project's newest sale, which holds its notes.
async fn event_sale(pool: &MySqlPool, company_id: i32, event_id: i32) -> ApiResult<i32> {
    find_event_sales(pool, company_id, event_id)
        .await?
        .first()
        .copied()
        .ok_or(ApiError::NotFound)
}

/// `GET /v1/events/{id}/sale`.
#[derive(Debug, Serialize, ToSchema)]
pub struct EventSale {
    /// The project's newest sale (it holds the notes); null when none.
    pub sale_id: Option<i32>,
    /// Every sale of the project, newest first: one per area when they were
    /// sold apart.
    pub sale_ids: Vec<i32>,
}

/// The event's project
///
/// The sales the event's project view shows: the event's own sale, else the
/// customer's sales at the event's address, with the other sales at that
/// address. A customer with one project lends it when there is no street to
/// compare; a builder's other jobs never show.
#[crate::openapi::path(
    get,
    path = "/v1/events/{id}/sale",
    operation_id = "get_event_sale",
    tag = "schedule",
    params(("id" = i32, Path, description = "Event id")),
    responses(
        (status = 200, description = "The sale, or null", body = EventSale),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee, admin or shop manager or worker", body = ErrorBody),
        (status = 404, description = "No such event in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn sale(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<EventSale>> {
    let sale_ids = find_event_sales(&state.pool, user.company_id, id).await?;
    Ok(Json(EventSale {
        sale_id: sale_ids.first().copied(),
        sale_ids,
    }))
}

/// List the project's notes
///
/// The notes on the event's sale, oldest first.
#[crate::openapi::path(
    get,
    path = "/v1/events/{id}/project-notes",
    operation_id = "list_project_notes",
    tag = "schedule",
    params(("id" = i32, Path, description = "Event id")),
    responses(
        (status = 200, description = "Notes", body = Vec<ProjectNote>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee, admin or shop manager or worker", body = ErrorBody),
        (status = 404, description = "No such event with a sale in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<Vec<ProjectNote>>> {
    let sale_id = event_sale(&state.pool, user.company_id, id).await?;
    let notes = sqlx::query_as(
        "SELECT n.id, n.audience, n.content, n.created_at, u.name AS created_by,
                n.created_by = ? AS own
           FROM sale_project_notes n LEFT JOIN users u ON u.id = n.created_by
          WHERE n.company_id = ? AND n.sale_id = ? AND n.deleted_at IS NULL
          ORDER BY n.created_at, n.id",
    )
    .bind(user.id)
    .bind(user.company_id)
    .bind(sale_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(notes))
}

/// Leave a note on the project
///
/// Signed with the caller; every event of the sale shows it.
#[crate::openapi::path(
    post,
    path = "/v1/events/{id}/project-notes",
    operation_id = "add_project_note",
    tag = "schedule",
    params(("id" = i32, Path, description = "Event id")),
    request_body = ProjectNoteInput,
    responses(
        (status = 201, description = "Added"),
        (status = 400, description = "Empty, too long or an unknown audience", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee, admin or shop manager or worker", body = ErrorBody),
        (status = 404, description = "No such event with a sale in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn add(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<ProjectNoteInput>,
) -> ApiResult<StatusCode> {
    let sale_id = event_sale(&state.pool, user.company_id, id).await?;
    let content = input.content.trim();
    if content.is_empty() {
        return Err(ApiError::BadRequest("Write the note first".into()));
    }
    if content.chars().count() > 2000 {
        return Err(ApiError::BadRequest(
            "A note must be 2000 characters or less".into(),
        ));
    }
    let audience = input.audience.as_deref().unwrap_or("everyone");
    if !AUDIENCES.contains(&audience) {
        return Err(ApiError::BadRequest(
            "A note is for installers, the sales rep or everyone".into(),
        ));
    }
    sqlx::query(
        "INSERT INTO sale_project_notes (company_id, sale_id, audience, content, created_by)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(user.company_id)
    .bind(sale_id)
    .bind(audience)
    .bind(content)
    .bind(user.id)
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::CREATED)
}

/// Delete my project note
///
/// Only the author can delete a note.
#[crate::openapi::path(
    delete,
    path = "/v1/events/{id}/project-notes/{note_id}",
    operation_id = "delete_project_note",
    tag = "schedule",
    params(
        ("id" = i32, Path, description = "Event id"),
        ("note_id" = i32, Path, description = "Note id"),
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee, admin or shop manager or worker", body = ErrorBody),
        (status = 404, description = "Not the caller's note on this project", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
    Path((id, note_id)): Path<(i32, i32)>,
) -> ApiResult<StatusCode> {
    let sale_id = event_sale(&state.pool, user.company_id, id).await?;
    let deleted = sqlx::query(
        "UPDATE sale_project_notes SET deleted_at = NOW()
          WHERE id = ? AND company_id = ? AND sale_id = ? AND created_by = ?
            AND deleted_at IS NULL",
    )
    .bind(note_id)
    .bind(user.company_id)
    .bind(sale_id)
    .bind(user.id)
    .execute(&state.pool)
    .await?
    .rows_affected();
    if deleted == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sale(id: i32, address: Option<&str>, day: &str) -> CustomerSale {
        CustomerSale {
            id,
            project_address: address.map(str::to_string),
            sale_day: Some(day.to_string()),
        }
    }

    #[test]
    fn street_key_ignores_format_and_direction() {
        assert_eq!(street_key("647 Overpeck Rd, Mooresville, IN 46158").as_deref(), Some("647 overpeck"));
        assert_eq!(street_key("647 Overpeck Road").as_deref(), Some("647 overpeck"));
        assert_eq!(street_key("4622 S Walcott St Indianapolis, IN").as_deref(), Some("4622 walcott"));
        assert_eq!(street_key("Carmel, IN 46033, USA"), None);
        assert_eq!(street_key("46220"), None);
    }

    #[test]
    fn builder_event_at_an_unsold_address_has_no_project() {
        let sales = [
            sale(5, Some("13636 Frenchman's Creek, Carmel, IN 46032"), "2026-10-05"),
            sale(4, Some("13636 Frenchman's Creek, Carmel, IN 46032"), "2026-10-05"),
            sale(3, Some("12792 Ford Dr, Fishers, IN 46038, USA"), "2025-12-05"),
        ];
        let location = Some("11716 Shadowwood Ct, Zionsville, IN 46077");
        assert!(event_project(None, location, &sales).is_empty());
        assert!(event_project(None, None, &sales).is_empty());
    }

    #[test]
    fn every_sale_at_the_address_is_the_project() {
        let sales = [
            sale(5, Some("13636 Frenchman's Creek, Carmel, IN 46032"), "2026-10-05"),
            sale(4, Some("13636 Frenchman's Creek, Carmel, IN 46032"), "2026-10-05"),
            sale(3, Some("12792 Ford Dr, Fishers, IN 46038, USA"), "2025-12-05"),
        ];
        let location = Some("13636 Frenchman's Creek, Carmel, IN 46032");
        assert_eq!(event_project(None, location, &sales), vec![5, 4]);
        assert_eq!(event_project(Some(4), None, &sales), vec![5, 4]);
        assert_eq!(event_project(Some(99), None, &sales), vec![99]);
    }

    #[test]
    fn one_project_lends_itself_only_without_a_street_to_compare() {
        let sold = [sale(7, Some("726 Orange St, Indianapolis"), "2026-09-01")];
        assert_eq!(event_project(None, None, &sold), vec![7]);
        assert!(event_project(None, Some("3424 Ralston Ave"), &sold).is_empty());
        let vague = [sale(8, Some("Muncie, IN"), "2026-09-01")];
        assert_eq!(event_project(None, Some("2931 Applewood Ct Muncie"), &vague), vec![8]);
    }
}
