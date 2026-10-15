//! `GET /v1/followups/timeline`: the conversation the AI follow-up writer
//! reads, SMS (both providers) and email, oldest first.

use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::followups::gather::{FollowupRequest, gather_followup_context};
use crate::followups::send_window::FollowupChannel;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

/// Whose conversation.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct TimelineQuery {
    /// The customer.
    pub customer_id: Option<String>,
    /// Or the phone (5 to 15 digits) of the SMS thread.
    pub phone_digits: Option<String>,
}

/// A message of the conversation.
#[derive(Debug, Serialize, ToSchema)]
pub struct TimelineMessage {
    /// When, `YYYY-MM-DD HH:MM` (UTC).
    pub at: String,
    /// `sms` or `email`.
    pub channel: &'static str,
    /// `inbound` or `outbound`.
    pub direction: &'static str,
    /// The text (an email starts with its subject line).
    pub text: String,
}

/// The conversation.
#[derive(Debug, Serialize, ToSchema)]
pub struct FollowupTimeline {
    /// Oldest first.
    pub timeline: Vec<TimelineMessage>,
}

fn positive_id(raw: Option<&str>) -> Option<i32> {
    raw.map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse::<i32>().ok())
        .filter(|id| *id > 0)
}

/// Show the follow-up conversation
///
/// The texts and emails with the customer (by id or phone) the AI
/// follow-up writer reads, oldest first.
#[utoipa::path(
    get,
    path = "/v1/followups/timeline",
    operation_id = "get_followup_timeline",
    tag = "sms",
    params(TimelineQuery),
    responses(
        (status = 200, description = "The conversation", body = FollowupTimeline),
        (status = 400, description = "`invalid_phone` or `customer_or_phone_required`", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such customer in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn timeline(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Query(query): Query<TimelineQuery>,
) -> ApiResult<Json<FollowupTimeline>> {
    let customer_id = positive_id(query.customer_id.as_deref());
    let phone = query.phone_digits.as_deref().map_or("", str::trim);
    let phone_digits = if phone.is_empty() {
        None
    } else if (5..=15).contains(&phone.len()) && phone.bytes().all(|b| b.is_ascii_digit()) {
        Some(phone.to_string())
    } else {
        return Err(ApiError::BadRequest("invalid_phone".into()));
    };
    if customer_id.is_none() && phone_digits.is_none() {
        return Err(ApiError::BadRequest("customer_or_phone_required".into()));
    }
    if let Some(id) = customer_id {
        let found: Option<i32> = sqlx::query_scalar(
            "SELECT id FROM customers WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
        )
        .bind(id)
        .bind(user.company_id)
        .fetch_optional(&state.pool)
        .await?;
        found.ok_or(ApiError::NotFound)?;
    }
    let mut request = FollowupRequest::new(user.company_id, user.id, FollowupChannel::Sms);
    request.customer_id = customer_id;
    request.phone_digits = phone_digits;
    let gathered = gather_followup_context(&state.pool, &request, chrono::Utc::now()).await?;
    Ok(Json(FollowupTimeline {
        timeline: gathered
            .timeline
            .into_iter()
            .map(|m| TimelineMessage {
                at: m.at,
                channel: m.channel.as_str(),
                direction: m.direction.as_str(),
                text: m.text,
            })
            .collect(),
    }))
}
