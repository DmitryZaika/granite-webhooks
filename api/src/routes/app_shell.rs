//! `GET /v1/app/shell`: what the app's frame needs for the signed-in user.
//!
//! The acting company's header, the portal a position locks them into, and
//! the features and permissions the sidebar, header and widgets switch on.
//!
//! Replaces most of the Remix root loader. The catalogs the sidebar filters
//! use come from `/v1/colors`, `/v1/stone-types`, `/v1/sink-types` and
//! `/v1/suppliers?has=`.

use crate::auth::CurrentUser;
use crate::auth::session::{SUPER_ADMIN_POSITION, SessionUser, user_positions};
use crate::error::{ApiResult, ErrorBody};
use crate::routes::company::queries::company_features;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use serde::Serialize;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(shell))
}

const SHOP_MANAGER: i32 = 3;
const MANAGER: i32 = 5;
const INSTALLER: i32 = 6;
const CHECK_IN: i32 = 8;
const MARKETING: i32 = 7;
const SHOP_WORKER: i32 = 4;

/// The company requests act in, for the header.
#[derive(Debug, Serialize, ToSchema)]
pub struct ShellCompany {
    /// Company id.
    pub id: i32,
    /// Name.
    pub name: Option<String>,
    /// Logo URL.
    pub logo_url: Option<String>,
    /// Saved logo height in the header, in pixels (the app clamps it).
    pub logo_height: Option<i32>,
}

/// Company-level switches the frame reads.
#[derive(Debug, Serialize, ToSchema)]
pub struct ShellFeatures {
    /// QuickBooks is switched on.
    pub quickbooks: bool,
    /// E-signatures are set up.
    pub signature: bool,
    /// Guided tours are on.
    pub onboarding: bool,
    /// The points leaderboard is on.
    pub leaderboard: bool,
}

/// `GET /v1/app/shell`.
#[derive(Debug, Serialize, ToSchema)]
pub struct AppShell {
    /// The signed-in user, in the company requests act in.
    pub user: SessionUser,
    /// The user holds the Super Admin position somewhere.
    pub user_is_super_admin: bool,
    /// Companies a super admin can switch to, by name.
    pub superadmin_companies: Vec<ShellCompanyRef>,
    /// The company a super admin chose to act in (null when it is their own).
    pub active_company_id: Option<i32>,
    /// The acting company.
    pub company: ShellCompany,
    /// The portal a position locks the user into (`installer`, `check-in`,
    /// `marketing`, `shop_worker`); null for everyone else.
    pub portal: Option<String>,
    /// That portal's start page.
    pub portal_home: Option<String>,
    /// The company has CloudTalk.
    pub has_cloudtalk_api: bool,
    /// The company has RingCentral.
    pub has_ringcentral_api: bool,
    /// The company receives email (it has a mail domain).
    pub has_inbound_email: bool,
    /// Pictures can be sent by text (`SMS_IMAGES_ENABLED` is not off).
    pub sms_images_enabled: bool,
    /// May edit company settings and users (super user, or Manager).
    pub can_manage_company: bool,
    /// May manage billing (Manager).
    pub can_manage_billing: bool,
    /// Holds the Shop Manager position in the company.
    pub is_shop_manager: bool,
    /// May send documents for signature.
    pub user_signature_access: bool,
    /// The acting company's switches.
    pub company_features: ShellFeatures,
    /// What the user's home company can do, as the guided tours gate on it.
    pub feature_capabilities: crate::routes::onboarding::team::TeamCapabilities,
}

/// A company of the switcher.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct ShellCompanyRef {
    /// Company id.
    pub id: i32,
    /// Name.
    pub name: String,
}

/// The portal, first match wins (`PORTAL_POSITIONS`).
fn portal(user: &SessionUser, super_admin: bool, positions: &[i32]) -> Option<(String, String)> {
    if user.is_superuser || super_admin {
        return None;
    }
    let company = user.company_id;
    [INSTALLER, CHECK_IN, MARKETING, SHOP_WORKER]
        .into_iter()
        .find(|position| positions.contains(position))
        .map(|position| match position {
            INSTALLER => (
                "installer".into(),
                format!("/installers/{company}/checklist"),
            ),
            CHECK_IN => ("check-in".into(), format!("/customer/{company}/check-in")),
            MARKETING => (
                "marketing".into(),
                format!("/external/marketing/{company}/leads"),
            ),
            _ => ("shop_worker".into(), "/shop/transactions".into()),
        })
}

/// The app frame
///
/// The signed-in user (in the company they act in), the super admin company
/// switcher, the acting company's name and logo, the portal a position locks
/// the user into, and the features and permissions the frame switches on.
#[utoipa::path(
    get,
    path = "/v1/app/shell",
    operation_id = "get_app_shell",
    tag = "auth",
    responses(
        (status = 200, description = "The frame", body = AppShell),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn shell(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> ApiResult<Json<AppShell>> {
    let pool = &state.pool;
    let rows = user_positions(pool, user.id).await?;
    let all: Vec<i32> = rows.iter().map(|row| row.position_id).collect();
    let here = |position: i32| {
        rows.iter()
            .any(|row| row.position_id == position && row.company_id == user.company_id)
    };
    let user_is_super_admin = all.contains(&SUPER_ADMIN_POSITION);
    let superadmin_companies: Vec<ShellCompanyRef> = if user_is_super_admin {
        sqlx::query_as(
            "SELECT c.id, c.name FROM company c
               JOIN users_positions up ON up.company_id = c.id
              WHERE up.user_id = ? AND up.position_id = ? AND c.deleted_at IS NULL
              ORDER BY c.name, c.id",
        )
        .bind(user.id)
        .bind(SUPER_ADMIN_POSITION)
        .fetch_all(pool)
        .await?
    } else {
        Vec::new()
    };
    let home: Option<i32> = sqlx::query_scalar("SELECT company_id FROM users WHERE id = ?")
        .bind(user.id)
        .fetch_optional(pool)
        .await?
        .flatten();
    let active_company_id =
        (user_is_super_admin && home != Some(user.company_id)).then_some(user.company_id);
    let (name, logo_url, logo_height): (Option<String>, Option<String>, Option<i32>) =
        sqlx::query_as(
            "SELECT name, NULLIF(NULLIF(TRIM(logo_url), ''), 'undefined'),
                    CAST(logo_height AS SIGNED) FROM company WHERE id = ?",
        )
        .bind(user.company_id)
        .fetch_optional(pool)
        .await?
        .map_or(
            (None, None, None),
            |(name, logo, height): (Option<String>, Option<String>, Option<i64>)| {
                (name, logo, height.and_then(|h| i32::try_from(h).ok()))
            },
        );
    let features = company_features(pool, user.company_id).await?;
    let feature_capabilities =
        crate::routes::onboarding::team::capabilities(pool, home.unwrap_or(user.company_id))
            .await?;
    let portal = portal(&user, user_is_super_admin, &all);
    let sms_images_enabled = !std::env::var("SMS_IMAGES_ENABLED").is_ok_and(|value| {
        matches!(
            value.trim().to_lowercase().as_str(),
            "false" | "0" | "no" | "off"
        )
    });
    Ok(Json(AppShell {
        user_is_super_admin,
        superadmin_companies,
        active_company_id,
        company: ShellCompany {
            id: user.company_id,
            name,
            logo_url,
            logo_height,
        },
        portal_home: portal.as_ref().map(|(_, home)| home.clone()),
        portal: portal.map(|(name, _)| name),
        has_cloudtalk_api: features.cloudtalk,
        has_ringcentral_api: features.ringcentral,
        has_inbound_email: features.inbound_email,
        sms_images_enabled,
        can_manage_company: user.is_superuser || here(MANAGER),
        can_manage_billing: here(MANAGER),
        is_shop_manager: here(SHOP_MANAGER),
        user_signature_access: user.is_signature_available,
        company_features: ShellFeatures {
            quickbooks: features.quickbooks_enabled,
            signature: features.e_signature,
            onboarding: features.onboarding,
            leaderboard: features.leaderboard,
        },
        feature_capabilities,
        user,
    }))
}
