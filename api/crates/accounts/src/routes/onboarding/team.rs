//! `GET /v1/onboarding/team`: the company's employees and their guided tours
//! progress, for the onboarding analytics and the users activity coverage.
//!
//! The app's tour registry decides which tours apply to whom, so this lists
//! what the registry reads: each member's role facts, the company's
//! capabilities, and the saved progress.

use super::store::{self, OnboardingState};
use super::{manifest, usage};
use crate::auth::AdminUser;
use crate::auth::sections::{company_tabs, configured_positions_held, tabs_for};
use crate::error::{ApiResult, ErrorBody};
use crate::openapi::ToSchema;
use crate::routes::company::queries::company_features;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use serde::Serialize;
use std::collections::{BTreeMap, HashSet};

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
    /// Inbound email is set up (the company has a mail domain): the Emails
    /// inbox and message composer are offered.
    pub inbound_email: bool,
    /// The company has at least one sales questionnaire (not deleted).
    pub questionnaires: bool,
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
    /// Features the member already uses: usage check id -> when they first
    /// used it (UTC, ISO 8601). Only the checks the tours name.
    pub usage: BTreeMap<String, String>,
    /// The tabs the member's positions open in this company, as the app
    /// shell's `sections` gives them to the member; null when they limit
    /// nothing.
    pub sections: Option<Vec<String>>,
    /// When the member's account was created (UTC, ISO 8601).
    pub account_created_at: Option<String>,
    /// The member has a phone line to call from: a `CloudTalk` agent and
    /// number, a `RingCentral` extension and number, or a Telnyx number of
    /// this company they may call from.
    pub phone_line: bool,
    /// The member can write messages to customers: the company has inbound
    /// email, or the member has a texting line of their own (`CloudTalk` or
    /// `RingCentral` line, or a Telnyx line when the company texts through
    /// Telnyx).
    pub message_composer: bool,
    /// The member may use e-signatures (their own `is_signature_available`
    /// flag; the company also needs a `SignWell` key).
    pub signature_access: bool,
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
    is_superuser: bool,
    has_sms_line: bool,
    signature_access: bool,
    account_created_at: Option<String>,
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
/// the company's capabilities and everyone's saved tours progress, the
/// features they already use, their sections and phone and message access.
/// While the company's guided tours are off, `enabled` is false and the list
/// empty.
#[crate::openapi::path(
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
    let capabilities = capabilities(pool, company).await?;
    let rows: Vec<MemberRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT id, name, email, COALESCE(is_admin, 0) <> 0 AS is_admin,
                COALESCE(is_superuser, 0) <> 0 AS is_superuser,
                (NULLIF(TRIM(cloudtalk_agent_id), '') IS NOT NULL
                   AND NULLIF(TRIM(cloudtalk_phone_number), '') IS NOT NULL)
                OR (NULLIF(TRIM(ringcentral_extension_id), '') IS NOT NULL
                   AND NULLIF(TRIM(ringcentral_phone_number), '') IS NOT NULL) AS has_sms_line,
                COALESCE(is_signature_available, 0) <> 0 AS signature_access,
                DATE_FORMAT(created_date, {iso}) AS account_created_at
           FROM users WHERE company_id = ? AND is_deleted = 0
          ORDER BY name, id",
        iso = store::ISO,
    )))
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
    let mut uses = usage::evaluate_team(pool, company, &ids, &manifest::usage_checks()).await?;
    // The company's sections are read once; each member's follow from the
    // sectioned positions they hold here.
    let company_tabs = company_tabs(pool, company).await?;
    // Members with a Telnyx number of the company they may call from (the
    // same rule as `lines::caller_ids`), in one query.
    let telnyx_users: Vec<i32> = sqlx::query_scalar(
        "SELECT DISTINCT nu.user_id
           FROM telnyx_number_users nu
           JOIN telnyx_numbers n ON n.id = nu.number_id AND n.company_id = ? AND n.active <> 0
           JOIN users u ON u.id = nu.user_id AND u.company_id = n.company_id AND u.is_deleted = 0
          WHERE nu.can_call_from <> 0",
    )
    .bind(company)
    .fetch_all(pool)
    .await?;
    let telnyx_users: HashSet<i32> = telnyx_users.into_iter().collect();
    let features = company_features(pool, company).await?;
    let members = rows
        .into_iter()
        .map(|row| {
            let holds = |position: i32| positions.contains(&(row.id, position));
            let record = records.remove(&row.id).unwrap_or_default();
            let here: Vec<i32> = positions
                .iter()
                .filter(|(user, _)| *user == row.id)
                .map(|(_, position)| *position)
                .collect();
            let held = configured_positions_held(row.is_superuser, &here);
            let telnyx_line = telnyx_users.contains(&row.id);
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
                usage: uses.remove(&row.id).unwrap_or_default(),
                sections: tabs_for(&held, row.is_admin, &company_tabs)
                    .map(|tabs| tabs.into_iter().map(str::to_string).collect()),
                account_created_at: row.account_created_at,
                phone_line: row.has_sms_line || telnyx_line,
                message_composer: features.inbound_email
                    || row.has_sms_line
                    || (features.telnyx_sms && telnyx_line),
                signature_access: row.signature_access,
            }
        })
        .collect();
    Ok(Json(Team {
        enabled: true,
        capabilities: TeamCapabilities {
            onboarding: true,
            ..capabilities
        },
        members,
    }))
}

/// What a company can do, as the tour registry gates on it
/// (`loadFeatureCapabilities`); `onboarding` is the company's tours switch.
pub async fn capabilities(pool: &sqlx::MySqlPool, company: i32) -> ApiResult<TeamCapabilities> {
    let features = company_features(pool, company).await?;
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
    let questionnaires: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM sales_questionnaires
                        WHERE company_id = ? AND deleted_at IS NULL)",
    )
    .bind(company)
    .fetch_one(pool)
    .await?;
    Ok(TeamCapabilities {
        cloudtalk: features.cloudtalk,
        quickbooks,
        verified_domain,
        sms_provider: features.cloudtalk || features.ringcentral,
        followup_tours: env_on("FOLLOWUP_ONBOARDING_ENABLED"),
        onboarding: features.onboarding,
        inbound_email: features.inbound_email,
        questionnaires,
    })
}
