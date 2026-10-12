//! `GET /v1/onboarding/team`: the company's employees and their guided tours
//! progress, for the onboarding analytics and the users activity coverage.
//!
//! The app's tour registry decides which tours apply to whom, so this lists
//! what the registry reads: each member's role facts, the company's
//! capabilities, and the saved progress.

use super::store::{self, OnboardingState};
use crate::auth::AdminUser;
use crate::error::{ApiResult, ErrorBody};
use crate::routes::company::queries::company_features;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use serde::Serialize;
use std::collections::HashSet;
use utoipa::ToSchema;

const SHOP_MANAGER: i32 = 3;
const SHOP_WORKER: i32 = 4;
const MANAGER: i32 = 5;

/// What the company can do, as the tour registry gates on it.
#[derive(Debug, Default, Serialize, ToSchema)]
pub struct TeamCapabilities {
    /// `CloudTalk` is connected.
    pub cloudtalk: bool,
    /// A `QuickBooks` company is linked.
    pub quickbooks: bool,
    /// SES has verified the company's mail domain.
    pub verified_domain: bool,
    /// `CloudTalk` or `RingCentral` is connected.
    pub sms_provider: bool,
    /// The follow-up tours are switched on (`FOLLOWUP_ONBOARDING_ENABLED`).
    pub followup_tours: bool,
    /// The company's guided tours are on.
    pub onboarding: bool,
}

/// One employee.
#[derive(Debug, Serialize, ToSchema)]
pub struct TeamMember {
    /// User id.
    pub user_id: i32,
    /// Name.
    pub name: String,
    /// Email.
    pub email: String,
    /// Admin access.
    pub is_admin: bool,
    /// Holds the Manager position (may manage users).
    pub can_manage_users: bool,
    /// Holds the Shop Worker position.
    pub is_shop_worker: bool,
    /// Holds the Shop Manager position.
    pub is_shop_manager: bool,
    /// Has a texting line of their own (`CloudTalk` agent and number, or a
    /// `RingCentral` extension and number).
    pub has_sms_line: bool,
    /// Saved progress (empty before any).
    pub progress: OnboardingState,
}

/// `GET /v1/onboarding/team`.
#[derive(Debug, Serialize, ToSchema)]
pub struct Team {
    /// The company's guided tours are on; off, `members` is empty.
    pub enabled: bool,
    /// The company's capabilities.
    pub capabilities: TeamCapabilities,
    /// The company's employees, by name.
    pub members: Vec<TeamMember>,
}

#[derive(sqlx::FromRow)]
struct MemberRow {
    id: i32,
    name: Option<String>,
    email: Option<String>,
    is_admin: bool,
    has_sms_line: bool,
}

fn env_on(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| {
        matches!(
            value.trim().to_lowercase().as_str(),
            "true" | "1" | "yes" | "on"
        )
    })
}

/// The onboarding team
///
/// The employees of the company the request acts in, with the facts the
/// tour registry reads (admin, manager and shop positions, a texting line),
/// the company's capabilities and everyone's saved tours progress. While
/// the company's guided tours are off, `enabled` is false and the list empty.
#[utoipa::path(
    get,
    path = "/v1/onboarding/team",
    operation_id = "get_onboarding_team",
    tag = "onboarding",
    responses(
        (status = 200, description = "The team", body = Team),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn team(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
) -> ApiResult<Json<Team>> {
    let pool = &state.pool;
    let company = user.company_id;
    let features = company_features(pool, company).await?;
    if !features.onboarding {
        return Ok(Json(Team {
            enabled: false,
            capabilities: TeamCapabilities::default(),
            members: Vec::new(),
        }));
    }
    let (domain, quickbooks): (Option<String>, bool) = sqlx::query_as(
        "SELECT NULLIF(TRIM(domain), ''), NULLIF(TRIM(qbo_realm_id), '') IS NOT NULL
           FROM company WHERE id = ?",
    )
    .bind(company)
    .fetch_optional(pool)
    .await?
    .unwrap_or((None, false));
    let verified_domain = match domain {
        Some(domain) => common::amazon::email::domain_verified(&domain)
            .await
            .unwrap_or(false),
        None => false,
    };
    let rows: Vec<MemberRow> = sqlx::query_as(
        "SELECT id, name, email, COALESCE(is_admin, 0) <> 0 AS is_admin,
                (NULLIF(TRIM(cloudtalk_agent_id), '') IS NOT NULL
                   AND NULLIF(TRIM(cloudtalk_phone_number), '') IS NOT NULL)
                OR (NULLIF(TRIM(ringcentral_extension_id), '') IS NOT NULL
                   AND NULLIF(TRIM(ringcentral_phone_number), '') IS NOT NULL) AS has_sms_line
           FROM users WHERE company_id = ? AND is_deleted = 0
          ORDER BY name, id",
    )
    .bind(company)
    .fetch_all(pool)
    .await?;
    let positions: Vec<(i32, i32)> =
        sqlx::query_as("SELECT user_id, position_id FROM users_positions WHERE company_id = ?")
            .bind(company)
            .fetch_all(pool)
            .await?;
    let positions: HashSet<(i32, i32)> = positions.into_iter().collect();
    let ids: Vec<i32> = rows.iter().map(|row| row.id).collect();
    let mut records = store::load_records(pool, &ids).await?;
    let members = rows
        .into_iter()
        .map(|row| {
            let holds = |position: i32| positions.contains(&(row.id, position));
            let record = records.remove(&row.id).unwrap_or_default();
            TeamMember {
                user_id: row.id,
                name: row.name.unwrap_or_default(),
                email: row.email.unwrap_or_default(),
                is_admin: row.is_admin,
                can_manage_users: holds(MANAGER),
                is_shop_worker: holds(SHOP_WORKER),
                is_shop_manager: holds(SHOP_MANAGER),
                has_sms_line: row.has_sms_line,
                progress: record.state,
            }
        })
        .collect();
    Ok(Json(Team {
        enabled: true,
        capabilities: TeamCapabilities {
            cloudtalk: features.cloudtalk,
            quickbooks,
            verified_domain,
            sms_provider: features.cloudtalk || features.ringcentral,
            followup_tours: env_on("FOLLOWUP_ONBOARDING_ENABLED"),
            onboarding: true,
        },
        members,
    }))
}
