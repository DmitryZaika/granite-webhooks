//! The company's current offer for AI follow-ups.
//!
//! A real deal (e.g. "10% off quartz installs booked by Oct 31") the
//! follow-up writer may give as the reason to reply, up to its last day.
//! One `ai_followup_offers` row per company; clearing it keeps the row, so a
//! running follow-up can tell its copy may name an offer that is gone.

use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use chrono::{NaiveDate, NaiveDateTime};
use serde::{Deserialize, Serialize};
use sqlx::MySqlPool;

/// Longest offer, in characters, after trimming.
pub const MAX_CHARS: usize = 500;

/// The company's current offer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct FollowupOffer {
    /// The offer as the AI may state it, trimmed; empty when there is none.
    pub offer: String,
    /// The last day it may be mentioned (store time); null: until changed.
    pub ends_on: Option<NaiveDate>,
}

/// The saved offer and when it last changed.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct SavedOffer {
    pub offer: String,
    pub ends_on: Option<NaiveDate>,
    /// UTC.
    pub updated_at: NaiveDateTime,
}

impl SavedOffer {
    /// The offer when it may still be mentioned on `day` (a store day).
    pub fn standing_on(&self, day: NaiveDate) -> Option<&str> {
        let offer = self.offer.trim();
        (!offer.is_empty() && self.ends_on.is_none_or(|last| day <= last)).then_some(offer)
    }
}

/// The saved offer, if the company ever saved one.
pub async fn load(pool: &MySqlPool, company_id: i32) -> Result<Option<SavedOffer>, sqlx::Error> {
    sqlx::query_as(
        "SELECT offer, ends_on, CAST(updated_at AS DATETIME) AS updated_at
           FROM ai_followup_offers WHERE company_id = ?",
    )
    .bind(company_id)
    .fetch_optional(pool)
    .await
}

/// Get the AI follow-up offer
///
/// The company's current offer the AI may mention in follow-ups, and its last
/// day. An empty offer when none is saved.
#[crate::openapi::path(
    get,
    path = "/v1/followups/offer",
    operation_id = "get_followup_offer",
    tag = "followups",
    responses(
        (status = 200, description = "The offer", body = FollowupOffer),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<FollowupOffer>> {
    let saved = load(&state.pool, user.company_id).await?;
    Ok(Json(FollowupOffer {
        offer: saved.as_ref().map_or_else(String::new, |s| s.offer.clone()),
        ends_on: saved.and_then(|s| s.ends_on),
    }))
}

/// Save the AI follow-up offer
///
/// Replaces the company's offer with `offer`, trimmed, and its last day. An
/// empty (or blank) offer clears it. Saving the same offer again changes
/// nothing, so running follow-ups keep the copy written for it. Answers the
/// saved offer.
#[crate::openapi::path(
    put,
    path = "/v1/followups/offer",
    operation_id = "save_followup_offer",
    tag = "followups",
    request_body = FollowupOffer,
    responses(
        (status = 200, description = "The saved offer", body = FollowupOffer),
        (status = 400, description = "Longer than 500 characters", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn put(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<FollowupOffer>,
) -> ApiResult<Json<FollowupOffer>> {
    let offer = input.offer.trim().to_string();
    if offer.chars().count() > MAX_CHARS {
        return Err(ApiError::BadRequest(format!(
            "offer is longer than {MAX_CHARS} characters"
        )));
    }
    let ends_on = input.ends_on.filter(|_| !offer.is_empty());
    // The same offer again keeps its time: copy written for it still holds.
    sqlx::query(
        "INSERT INTO ai_followup_offers (company_id, offer, ends_on, updated_by, updated_at)
         VALUES (?, ?, ?, ?, UTC_TIMESTAMP())
         ON DUPLICATE KEY UPDATE
           updated_by = IF(offer <=> VALUES(offer) AND ends_on <=> VALUES(ends_on), updated_by, VALUES(updated_by)),
           updated_at = IF(offer <=> VALUES(offer) AND ends_on <=> VALUES(ends_on), updated_at, VALUES(updated_at)),
           offer = VALUES(offer),
           ends_on = VALUES(ends_on)",
    )
    .bind(user.company_id)
    .bind(&offer)
    .bind(ends_on)
    .bind(user.id)
    .execute(&state.pool)
    .await?;
    Ok(Json(FollowupOffer { offer, ends_on }))
}
