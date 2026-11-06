//! Employee CRM › Automations: whose automated emails and texts go out when a
//! deal of the caller lands on a list (`users.lead_automation`).
//!
//! - `GET /v1/me/lead-automation` → the caller's choice
//! - `PUT /v1/me/lead-automation` → change it
//!
//! The scheduling side is `common::crud::lead_automation`.

use crate::auth::EmployeeUser;
use crate::error::{ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use sqlx::MySqlPool;

/// Whose automated templates a sales rep's deals get.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum LeadAutomationMode {
    /// The company's automated email and SMS templates (Admin › Automations)
    /// that go to this rep's deals. Every user starts here.
    #[default]
    Company,
    /// Only the rep's own automated templates (personal templates with a
    /// trigger list); the company's do not go to their deals.
    Own,
}

impl LeadAutomationMode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Company => "company",
            Self::Own => "own",
        }
    }

    fn parse(raw: &str) -> Self {
        if raw == "own" { Self::Own } else { Self::Company }
    }
}

/// The caller's automation choice; also the body of `PUT /v1/me/lead-automation`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct LeadAutomation {
    /// `company` or `own`.
    pub mode: LeadAutomationMode,
}

async fn mode_of(pool: &MySqlPool, user_id: i32) -> Result<LeadAutomationMode, sqlx::Error> {
    let raw: Option<String> =
        sqlx::query_scalar("SELECT CAST(lead_automation AS CHAR) FROM users WHERE id = ?")
            .bind(user_id)
            .fetch_optional(pool)
            .await?;
    Ok(raw.as_deref().map_or_else(LeadAutomationMode::default, LeadAutomationMode::parse))
}

/// Pending automatic sends of the side the caller just left are called off:
/// the company's on their deals when they take their own, their own when they
/// go back to the company's. Nothing is scheduled for deals already on a
/// list; the new choice applies from the next deal that lands on one.
async fn cancel_left_side(
    pool: &MySqlPool,
    user_id: i32,
    mode: LeadAutomationMode,
) -> Result<(), sqlx::Error> {
    for (queue, templates) in [
        ("scheduled_emails", "email_templates"),
        ("scheduled_sms", "cloudtalk_sms_templates"),
    ] {
        let (condition, reason) = match mode {
            LeadAutomationMode::Own => (
                "t.user_id IS NULL AND d.user_id = ?",
                "Sales rep uses their own automations",
            ),
            LeadAutomationMode::Company => (
                "t.user_id = ?",
                "Sales rep uses the company automations",
            ),
        };
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE {queue} s
               JOIN deals d ON d.id = s.deal_id
               JOIN {templates} t ON t.id = s.template_id
                SET s.status = 'cancelled', s.error_message = ?
              WHERE s.status = 'pending' AND {condition}"
        )))
        .bind(reason)
        .bind(user_id)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// Get whose automations the caller's deals get
///
/// `company` (the default): when a deal of the caller lands on a list (e.g.
/// Not Contacted Yet), the company's automated email and SMS templates for
/// that list go out (Admin › Automations; those naming no sales reps, or
/// naming the caller). `own`: only the caller's own automated templates do —
/// personal templates with a `lead_list_id` (`GET /v1/email-templates`,
/// `GET /v1/sms-templates`, `scope: personal`).
#[crate::openapi::path(
    get,
    path = "/v1/me/lead-automation",
    operation_id = "get_lead_automation",
    tag = "templates",
    responses(
        (status = 200, description = "The caller's choice", body = LeadAutomation),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<LeadAutomation>> {
    let mode = mode_of(&state.pool, user.id).await?;
    Ok(Json(LeadAutomation { mode }))
}

/// Choose whose automations the caller's deals get
///
/// `company` or `own` (see `GET /v1/me/lead-automation`). Changing it calls
/// off the pending automatic sends of the side left behind (the company's on
/// the caller's deals, or the caller's own); deals already on a list get
/// nothing new, the choice applies to deals that land on a list from now on.
/// Returns the saved choice.
#[crate::openapi::path(
    put,
    path = "/v1/me/lead-automation",
    operation_id = "set_lead_automation",
    tag = "templates",
    request_body = LeadAutomation,
    responses(
        (status = 200, description = "Saved", body = LeadAutomation),
        (status = 400, description = "Unknown mode", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn set(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<LeadAutomation>,
) -> ApiResult<Json<LeadAutomation>> {
    let pool = &state.pool;
    let before = mode_of(pool, user.id).await?;
    if before != input.mode {
        sqlx::query("UPDATE users SET lead_automation = ? WHERE id = ?")
            .bind(input.mode.as_str())
            .bind(user.id)
            .execute(pool)
            .await?;
        cancel_left_side(pool, user.id, input.mode).await?;
    }
    Ok(Json(input))
}
