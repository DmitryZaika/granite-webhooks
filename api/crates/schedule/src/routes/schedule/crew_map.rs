//! Where the crew is now, for the CRM's crew map.
//!
//! - `GET /v1/schedule/crew-positions` → each installer's and template
//!   maker's latest position, which their phone sends from 5 AM until 9 PM
//!   (`POST /v1/me/location`)

use crate::auth::AdminUser;
use crate::error::{ApiResult, ErrorBody};
use crate::openapi::ToSchema;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::HashSet;

/// `positions.id` of an installer.
const INSTALLER: i32 = 6;
/// `positions.id` of a template maker.
const TEMPLATE: i32 = 11;

/// An installer or template maker and where their phone last was.
#[derive(Debug, Serialize, ToSchema)]
pub struct CrewPosition {
    pub user_id: i32,
    pub name: String,
    /// `installer` or `template`.
    pub role: String,
    /// Degrees; null when their phone never sent a position.
    pub latitude: Option<f64>,
    /// Degrees; null when their phone never sent a position.
    pub longitude: Option<f64>,
    /// When the phone sent it.
    pub recorded_at: Option<DateTime<Utc>>,
    /// They are at the shop.
    pub at_shop: bool,
    /// The job they are at.
    pub place_event_id: Option<i32>,
    /// That job's title.
    pub place_title: Option<String>,
    /// Since when they are at the shop or that job.
    pub place_since: Option<DateTime<Utc>>,
}

#[derive(sqlx::FromRow)]
struct Row {
    user_id: i32,
    name: Option<String>,
    position_id: i32,
    latitude: Option<f64>,
    longitude: Option<f64>,
    recorded_at: Option<DateTime<Utc>>,
    at_shop: Option<bool>,
    place_event_id: Option<i32>,
    place_title: Option<String>,
    place_since: Option<DateTime<Utc>>,
}

/// Where the crew is
///
/// Every installer and template maker of the company with their phone's
/// latest position (Granite Phone sends it from 5 AM until 9 PM), and the
/// shop or job they are at. Admins only.
#[crate::openapi::path(
    get,
    path = "/v1/schedule/crew-positions",
    operation_id = "get_crew_positions",
    tag = "schedule",
    responses(
        (status = 200, description = "The crew, by name", body = Vec<CrewPosition>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn crew_positions(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
) -> ApiResult<Json<Vec<CrewPosition>>> {
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT u.id AS user_id, u.name, up.position_id, cp.latitude, cp.longitude,
                cp.recorded_at, cp.at_shop <> 0 AS at_shop, cp.place_event_id,
                e.title AS place_title, cp.place_since
           FROM users u
           JOIN users_positions up ON up.user_id = u.id
           LEFT JOIN crew_positions cp ON cp.user_id = u.id AND cp.company_id = up.company_id
           LEFT JOIN events e ON e.id = cp.place_event_id AND e.deleted_date IS NULL
          WHERE up.company_id = ? AND up.position_id IN (?, ?) AND u.is_deleted = 0
          ORDER BY u.name, u.id, up.position_id",
    )
    .bind(user.company_id)
    .bind(INSTALLER)
    .bind(TEMPLATE)
    .fetch_all(&state.pool)
    .await?;
    // Someone who is both an installer and a template maker is listed once.
    let mut seen = HashSet::new();
    let crew = rows
        .into_iter()
        .filter(|row| seen.insert(row.user_id))
        .map(|row| CrewPosition {
            user_id: row.user_id,
            name: row
                .name
                .map(|name| name.trim().to_string())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| "Unnamed".into()),
            role: if row.position_id == INSTALLER {
                "installer"
            } else {
                "template"
            }
            .into(),
            latitude: row.latitude,
            longitude: row.longitude,
            recorded_at: row.recorded_at,
            at_shop: row.at_shop.unwrap_or(false),
            place_event_id: row.place_event_id,
            place_title: row.place_title,
            place_since: row.place_since,
        })
        .collect();
    Ok(Json(crew))
}
