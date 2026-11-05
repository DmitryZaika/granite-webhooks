//! Super admin: the companies of the platform.
//!
//! - `GET    /v1/superuser/companies`                         → every company with users, tours and switches
//! - `POST   /v1/superuser/companies`                         → create a company (seeded)
//! - `GET    /v1/superuser/companies/{id}`                    → its settings and keys
//! - `PUT    /v1/superuser/companies/{id}`                    → save them
//! - `DELETE /v1/superuser/companies/{id}?confirm_name=`      → delete it
//! - `GET    /v1/superuser/companies/{id}/users`              → its users and their tours progress
//! - `GET    /v1/superuser/companies/{id}/domain-trust`       → email deliverability of its domain
//! - `GET    /v1/superuser/companies/{id}/ses-inbound`        → can it receive email through SES
//! - `GET    /v1/superuser/companies/{id}/setup-checklist`    → the setup checklist
//! - `PUT    /v1/superuser/companies/{id}/setup-checklist/items/{key}` → tick an item by hand
//! - `PUT    /v1/superuser/companies/{id}/features/{feature}` → switch a feature
//! - `PUT    /v1/superuser/companies/{id}/billing-required`   → require Stripe payment
//!
//! Superusers only. Replaces the Remix `admin.superuser-dashboard.companies*`
//! routes.

use crate::auth::CurrentUser;
use crate::auth::session::SessionUser;
use crate::deliverability::{self, DomainTrust};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::{IntoParams, ToSchema};
use crate::openapi::{OpenApiRouter, routes};
use crate::phone::revoke::{RevokeReason, revoke_company_phones};
use crate::routes::created::CreatedId;
use crate::routes::onboarding::store::{self, OnboardingState};
use crate::routes::uploads::{self, schemas::UploadFolder};
use crate::setup_checklist::{self, Checklist, CompanyFacts};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_companies, create_company))
        .routes(routes!(get_company, update_company, delete_company))
        .routes(routes!(company_users))
        .routes(routes!(domain_trust))
        .routes(routes!(ses_inbound))
        .routes(routes!(setup_checklist))
        .routes(routes!(tick_setup_item))
        .routes(routes!(switch_feature))
        .routes(routes!(set_billing_required))
}

const CHECK_IN: i32 = 8;
const INSTALLER: i32 = 6;
const MANAGER: i32 = 5;

const fn superuser(user: &SessionUser) -> ApiResult<()> {
    if user.is_superuser {
        Ok(())
    } else {
        Err(ApiError::Forbidden)
    }
}

/// Active users that count toward `max_users` (walk-in-only accounts do not).
const COUNTS_TOWARD_LIMIT: &str = "u.is_deleted = 0 AND (
    EXISTS (SELECT 1 FROM users_positions up WHERE up.user_id = u.id AND up.company_id = u.company_id
                                              AND up.position_id <> 8)
    OR NOT EXISTS (SELECT 1 FROM users_positions up WHERE up.user_id = u.id AND up.company_id = u.company_id))";

/// Users who can run the guided tours (not installer or walk-in accounts).
const RUNS_ONBOARDING: &str = "(u.is_superuser = 1 OR NOT EXISTS (
    SELECT 1 FROM users_positions up WHERE up.user_id = u.id AND up.company_id = u.company_id
                                       AND up.position_id IN (8, 6)))";

/// A company's switches.
#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct Features {
    /// QuickBooks is on.
    pub quickbooks: bool,
    /// The old company eSignatures switch; no longer used (eSignatures are
    /// given per user).
    pub signature: bool,
    /// Guided tours are on (own switch, else the platform default).
    pub onboarding: bool,
    /// The points leaderboard is on (own switch, else the platform default).
    pub leaderboard: bool,
}

/// The old company eSignatures switch. It no longer gates anything:
/// eSignatures are given per user (Superuser › Feature access). Kept so old
/// pages that still show it read the stored value.
const LEGACY_SIGNATURE_SWITCH: &str = "(COALESCE(c.signature_enabled, 0) <> 0)";

fn features_sql() -> String {
    format!(
        "COALESCE(c.quickbooks_enabled, 1) <> 0 AS quickbooks, {LEGACY_SIGNATURE_SWITCH} AS signature,
         {} AS onboarding, {} AS leaderboard",
        crate::routes::company::queries::ONBOARDING_ON,
        crate::routes::company::queries::LEADERBOARD_ON
    )
}

/// One company in the list.
#[derive(Debug, Serialize, ToSchema)]
pub struct CompanySummary {
    /// Company id.
    pub id: i32,
    /// Company name.
    pub name: String,
    /// Its subdomain (`acme` of `acme.granite-manager.com`), or null.
    pub subdomain: Option<String>,
    /// Its email domain, or null.
    pub domain: Option<String>,
    /// Logo, or null.
    pub logo_url: Option<String>,
    /// User limit, or null.
    pub max_users: Option<i32>,
    /// Active users counting toward the limit.
    pub user_count: i64,
    /// Users who finished the first-run tour.
    pub onboarding_finished: i64,
    /// Users who run the tours.
    pub onboarding_eligible: i64,
    /// `CloudTalk` keys are saved.
    pub cloudtalk_connected: bool,
    /// A `QuickBooks` company is linked.
    pub quickbooks_connected: bool,
    /// Access depends on the Stripe subscription.
    pub billing_required: bool,
    pub features: Features,
}

#[derive(sqlx::FromRow)]
struct SummaryRow {
    id: i32,
    name: String,
    subdomain: Option<String>,
    domain: Option<String>,
    logo_url: Option<String>,
    max_users: Option<i32>,
    user_count: i64,
    onboarding_finished: i64,
    onboarding_eligible: i64,
    cloudtalk_connected: bool,
    quickbooks_connected: bool,
    billing_required: bool,
    #[sqlx(flatten)]
    features: Features,
}

/// List the companies
///
/// Every live company, most users first: users counting toward the limit,
/// tours progress, connected integrations, feature switches and whether
/// Stripe payment is required. The slower checks are per company:
/// `/domain-trust`, `/ses-inbound`, `/setup-checklist`.
#[crate::openapi::path(
    get,
    path = "/v1/superuser/companies",
    operation_id = "list_platform_companies",
    tag = "superuser",
    responses(
        (status = 200, description = "The companies", body = [CompanySummary]),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a superuser", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list_companies(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> ApiResult<Json<Vec<CompanySummary>>> {
    superuser(&user)?;
    let sql = format!(
        "SELECT c.id, c.name, c.subdomain, c.domain, c.logo_url, c.max_users,
                CAST(COUNT(u.id) AS SIGNED) AS user_count,
                CAST(COALESCE(SUM(u.id IS NOT NULL AND {RUNS_ONBOARDING} AND uo.completed_at IS NOT NULL), 0) AS SIGNED)
                  AS onboarding_finished,
                CAST(COALESCE(SUM(u.id IS NOT NULL AND {RUNS_ONBOARDING}), 0) AS SIGNED) AS onboarding_eligible,
                (COALESCE(c.cloudtalk_access_key, '') <> '' AND COALESCE(c.cloudtalk_access_secret, '') <> '')
                  AS cloudtalk_connected,
                COALESCE(TRIM(c.qbo_realm_id), '') <> '' AS quickbooks_connected,
                COALESCE(c.billing_required, 0) <> 0 AS billing_required,
                {features}
           FROM company c
           {joins}
           LEFT JOIN users u ON u.company_id = c.id AND {COUNTS_TOWARD_LIMIT}
           LEFT JOIN user_onboarding uo ON uo.user_id = u.id
          WHERE c.deleted_at IS NULL
          GROUP BY c.id
          ORDER BY user_count DESC, c.name ASC, c.id",
        features = features_sql(),
        joins = crate::routes::company::queries::ROLLOUT_JOINS,
    );
    let rows: Vec<SummaryRow> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .fetch_all(&state.pool)
        .await?;
    Ok(Json(
        rows.into_iter()
            .map(|row| CompanySummary {
                id: row.id,
                name: row.name,
                subdomain: row.subdomain,
                domain: row.domain,
                logo_url: row.logo_url,
                max_users: row.max_users,
                user_count: row.user_count,
                onboarding_finished: row.onboarding_finished,
                onboarding_eligible: row.onboarding_eligible,
                cloudtalk_connected: row.cloudtalk_connected,
                quickbooks_connected: row.quickbooks_connected,
                billing_required: row.billing_required,
                features: row.features,
            })
            .collect(),
    ))
}

async fn company_exists(state: &AppState, id: i32) -> ApiResult<()> {
    let found: Option<i32> =
        sqlx::query_scalar("SELECT id FROM company WHERE id = ? AND deleted_at IS NULL")
            .bind(id)
            .fetch_optional(&state.pool)
            .await?;
    found.map(|_| ()).ok_or(ApiError::NotFound)
}

/// A company's settings as super admins edit them.
#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct CompanyDetails {
    /// Company id.
    pub id: i32,
    /// Company name.
    pub name: String,
    /// Subdomain, or null.
    pub subdomain: Option<String>,
    /// Address, or null.
    pub address: Option<String>,
    /// Email domain, or null.
    pub domain: Option<String>,
    /// Logo, or null.
    pub logo_url: Option<String>,
    /// Logo height in pixels as saved, or null.
    pub logo_height: Option<i32>,
    /// Sales tax in percent, or null.
    pub sales_tax_percent: Option<f64>,
    /// User limit, or null.
    pub max_users: Option<i32>,
    /// `CloudTalk` API key id, or null.
    pub cloudtalk_access_key: Option<String>,
    /// `CloudTalk` API secret, or null.
    pub cloudtalk_access_secret: Option<String>,
    /// `RingCentral` JWT app client id, or null.
    pub ringcentral_client_id: Option<String>,
    /// `RingCentral` JWT app client secret, or null.
    pub ringcentral_client_secret: Option<String>,
    /// `RingCentral` JWT, or null.
    pub ringcentral_jwt: Option<String>,
    /// `RingCentral` server override, or null.
    pub ringcentral_server_url: Option<String>,
    /// Linked `QuickBooks` company, or null.
    pub qbo_realm_id: Option<String>,
    /// `SignWell` API key, or null.
    pub signwell_api_key: Option<String>,
    /// QuickBooks switch.
    pub quickbooks_enabled: bool,
    /// The old company eSignatures switch; no longer used (eSignatures are
    /// given per user).
    pub signature_enabled: bool,
    /// Stripe payment required.
    pub billing_required: bool,
    /// Guided tours switch (own setting, else the platform default).
    #[sqlx(skip)]
    pub onboarding_enabled: bool,
    /// Points leaderboard switch (own setting, else the platform default).
    #[sqlx(skip)]
    pub leaderboard_enabled: bool,
}

/// Get a company
///
/// A company's settings, keys and switches, as the edit form shows them.
#[crate::openapi::path(
    get,
    path = "/v1/superuser/companies/{id}",
    operation_id = "get_platform_company",
    tag = "superuser",
    params(("id" = i32, Path, description = "Company id")),
    responses(
        (status = 200, description = "The company", body = CompanyDetails),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a superuser", body = ErrorBody),
        (status = 404, description = "No such company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get_company(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<CompanyDetails>> {
    superuser(&user)?;
    let sql = format!(
        "SELECT c.id, c.name, c.subdomain, c.address, c.domain, c.logo_url, c.logo_height,
                CAST(ROUND(c.state_taxes * 100, 2) AS DOUBLE) AS sales_tax_percent, c.max_users,
                c.cloudtalk_access_key, c.cloudtalk_access_secret, c.ringcentral_client_id,
                c.ringcentral_client_secret, c.ringcentral_jwt, c.ringcentral_server_url,
                c.qbo_realm_id, c.signwell_api_key,
                COALESCE(c.quickbooks_enabled, 1) <> 0 AS quickbooks_enabled,
                {LEGACY_SIGNATURE_SWITCH} AS signature_enabled, COALESCE(c.billing_required, 0) <> 0 AS billing_required,
                {} AS onboarding, {} AS leaderboard
           FROM company c {}
          WHERE c.id = ? AND c.deleted_at IS NULL",
        crate::routes::company::queries::ONBOARDING_ON,
        crate::routes::company::queries::LEADERBOARD_ON,
        crate::routes::company::queries::ROLLOUT_JOINS,
    );
    let row: CompanyDetailsWithRollout = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or(ApiError::NotFound)?;
    let mut details = row.details;
    details.onboarding_enabled = row.onboarding;
    details.leaderboard_enabled = row.leaderboard;
    Ok(Json(details))
}

#[derive(sqlx::FromRow)]
struct CompanyDetailsWithRollout {
    #[sqlx(flatten)]
    details: CompanyDetails,
    onboarding: bool,
    leaderboard: bool,
}

/// Body of `POST /v1/superuser/companies` and `PUT /v1/superuser/companies/{id}`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CompanyInput {
    /// Company name (required).
    pub name: String,
    /// Subdomain: lowercase letters, digits and inner hyphens (required, unique).
    pub subdomain: String,
    #[serde(default)]
    /// Address.
    pub address: Option<String>,
    /// Email domain.
    #[serde(default)]
    pub domain: Option<String>,
    /// Sales tax in percent (0-100).
    #[serde(default)]
    pub sales_tax_percent: Option<f64>,
    /// User limit (1 or more); edit only.
    #[serde(default)]
    pub max_users: Option<i32>,
    /// A new logo uploaded to `company-logos`; edit only, omit to keep it.
    #[serde(default)]
    pub logo_url: Option<String>,
    /// Logo height, 32-192; edit only (default 48).
    #[serde(default)]
    pub logo_height: Option<i32>,
    #[serde(default)]
    /// `CloudTalk` API key id.
    pub cloudtalk_access_key: Option<String>,
    #[serde(default)]
    /// `CloudTalk` API secret.
    pub cloudtalk_access_secret: Option<String>,
    #[serde(default)]
    /// `RingCentral` JWT app client id.
    pub ringcentral_client_id: Option<String>,
    #[serde(default)]
    /// `RingCentral` JWT app client secret.
    pub ringcentral_client_secret: Option<String>,
    #[serde(default)]
    /// `RingCentral` JWT.
    pub ringcentral_jwt: Option<String>,
    #[serde(default)]
    /// `RingCentral` server override.
    pub ringcentral_server_url: Option<String>,
    #[serde(default)]
    /// `SignWell` API key; edit only.
    pub signwell_api_key: Option<String>,
    /// QuickBooks switch; edit only (default on).
    #[serde(default)]
    pub quickbooks_enabled: Option<bool>,
    /// The old company eSignatures switch; edit only (omit to leave it). No
    /// longer used: eSignatures are given per user.
    #[serde(default)]
    pub signature_enabled: Option<bool>,
}

fn text(value: Option<&String>) -> Option<String> {
    value
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

fn valid_subdomain(value: &str) -> bool {
    !value.is_empty()
        && value.split('-').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

struct Clean {
    name: String,
    subdomain: String,
    tax: Option<String>,
}

fn validate(input: &CompanyInput) -> ApiResult<Clean> {
    let name = input.name.trim().to_string();
    if name.is_empty() {
        return Err(ApiError::BadRequest("Company name is required".into()));
    }
    let subdomain = input.subdomain.trim().to_lowercase();
    if !valid_subdomain(&subdomain) {
        return Err(ApiError::BadRequest(
            "Subdomain must contain only lowercase letters, numbers, and hyphens".into(),
        ));
    }
    if input
        .sales_tax_percent
        .is_some_and(|tax| !tax.is_finite() || !(0.0..=100.0).contains(&tax))
    {
        return Err(ApiError::BadRequest(
            "Sales tax must be between 0 and 100".into(),
        ));
    }
    if input.max_users.is_some_and(|max| max < 1) {
        return Err(ApiError::BadRequest("max_users must be 1 or more".into()));
    }
    if input.logo_height.is_some_and(|h| !(32..=192).contains(&h)) {
        return Err(ApiError::BadRequest("logo_height must be 32 to 192".into()));
    }
    Ok(Clean {
        name,
        subdomain,
        tax: input
            .sales_tax_percent
            .map(|tax| format!("{:.4}", (tax * 100.0).round() / 10_000.0)),
    })
}

async fn subdomain_taken(state: &AppState, subdomain: &str, except: i32) -> ApiResult<bool> {
    let taken: Option<i32> = sqlx::query_scalar(
        "SELECT id FROM company WHERE subdomain = ? AND id <> ? AND deleted_at IS NULL LIMIT 1",
    )
    .bind(subdomain)
    .bind(except)
    .fetch_optional(&state.pool)
    .await?;
    Ok(taken.is_some())
}

static DEFAULT_TEMPLATES: std::sync::LazyLock<Vec<DefaultTemplate>> =
    std::sync::LazyLock::new(|| {
        serde_json::from_str(include_str!(
            "../../../../data/default_email_templates.json"
        ))
        .expect("valid default templates")
    });

#[derive(Deserialize)]
struct DefaultTemplate {
    name: String,
    subject: String,
    body: String,
}

/// Create a company
///
/// Creates the company with its system calendars, the default company email
/// templates and customer sources, the statistics widgets hidden, a
/// `walk-in@<subdomain>.com` employee for the check-in kiosk, and checks
/// its domain's deliverability.
#[crate::openapi::path(
    post,
    path = "/v1/superuser/companies",
    operation_id = "create_platform_company",
    tag = "superuser",
    request_body = CompanyInput,
    responses(
        (status = 201, description = "Created", body = CreatedId),
        (status = 400, description = "Missing name, bad subdomain or tax", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a superuser", body = ErrorBody),
        (status = 409, description = "Subdomain already in use", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create_company(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    JsonBody(input): JsonBody<CompanyInput>,
) -> ApiResult<(StatusCode, Json<CreatedId>)> {
    superuser(&user)?;
    let clean = validate(&input)?;
    if subdomain_taken(&state, &clean.subdomain, 0).await? {
        return Err(ApiError::Conflict("Subdomain is already in use".into()));
    }
    let password = bcrypt::hash("1111", 10).map_err(|e| ApiError::Internal(e.to_string()))?;
    let mut tx = state.pool.begin().await?;
    let id = sqlx::query(
        "INSERT INTO company (name, subdomain, address, state_taxes, domain, cloudtalk_access_key,
                              cloudtalk_access_secret, ringcentral_client_id, ringcentral_client_secret,
                              ringcentral_jwt, ringcentral_server_url, statistics_widgets_hidden)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1)",
    )
    .bind(&clean.name)
    .bind(&clean.subdomain)
    .bind(text(input.address.as_ref()))
    .bind(&clean.tax)
    .bind(text(input.domain.as_ref()))
    .bind(text(input.cloudtalk_access_key.as_ref()))
    .bind(text(input.cloudtalk_access_secret.as_ref()))
    .bind(text(input.ringcentral_client_id.as_ref()))
    .bind(text(input.ringcentral_client_secret.as_ref()))
    .bind(text(input.ringcentral_jwt.as_ref()))
    .bind(text(input.ringcentral_server_url.as_ref()))
    .execute(&mut *tx)
    .await?
    .last_insert_id();
    let id = i32::try_from(id).map_err(|_| ApiError::Internal("company id out of range".into()))?;
    for (slug, name, color) in crate::routes::calendars::queries::SYSTEM_CALENDARS {
        sqlx::query("INSERT INTO calendars (company_id, name, color, slug, created_by) VALUES (?, ?, ?, ?, NULL)")
            .bind(id)
            .bind(name)
            .bind(color)
            .bind(slug)
            .execute(&mut *tx)
            .await?;
    }
    for template in DEFAULT_TEMPLATES.iter() {
        sqlx::query(
            "INSERT INTO email_templates (template_name, template_subject, template_body, company_id, user_id)
             VALUES (?, ?, ?, ?, NULL)",
        )
        .bind(&template.name)
        .bind(&template.subject)
        .bind(&template.body)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query(
        "INSERT INTO users (name, email, password, company_id, is_employee, is_admin)
         VALUES ('Walk-in', ?, ?, ?, 1, 0)",
    )
    .bind(format!("walk-in@{}.com", clean.subdomain))
    .bind(password)
    .bind(id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO customer_sources (company_id, source_key, label, is_system) VALUES
           (?, 'check-in', 'Walk-in', 1), (?, 'leads', 'Leads', 1),
           (?, 'call-in', 'Call-in', 1), (?, 'other', 'Other', 1)",
    )
    .bind(id)
    .bind(id)
    .bind(id)
    .bind(id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    deliverability::ensure_company_trust(&state.pool, &state.dns, id, input.domain.as_deref())
        .await?;
    Ok((StatusCode::CREATED, Json(CreatedId { id: i64::from(id) })))
}

/// Save a company
///
/// Replaces the company's settings, keys and switches (and its logo when a
/// new one is given), then re-checks its domain's deliverability.
#[crate::openapi::path(
    put,
    path = "/v1/superuser/companies/{id}",
    operation_id = "update_platform_company",
    tag = "superuser",
    params(("id" = i32, Path, description = "Company id")),
    request_body = CompanyInput,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Missing name, bad subdomain, tax, limit or logo", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a superuser", body = ErrorBody),
        (status = 404, description = "No such company", body = ErrorBody),
        (status = 409, description = "Subdomain already in use", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update_company(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<CompanyInput>,
) -> ApiResult<StatusCode> {
    superuser(&user)?;
    company_exists(&state, id).await?;
    let clean = validate(&input)?;
    if subdomain_taken(&state, &clean.subdomain, id).await? {
        return Err(ApiError::Conflict("Subdomain is already in use".into()));
    }
    let new_logo = match input
        .logo_url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
    {
        Some(url) => {
            uploads::attach(
                &state.storage,
                url,
                UploadFolder::CompanyLogos,
                user.company_id,
            )
            .await?;
            Some(url.to_string())
        }
        None => None,
    };
    let mut query = sqlx::QueryBuilder::new("UPDATE company SET name = ");
    query.push_bind(&clean.name);
    query.push(", subdomain = ").push_bind(&clean.subdomain);
    query
        .push(", address = ")
        .push_bind(text(input.address.as_ref()));
    query
        .push(", domain = ")
        .push_bind(text(input.domain.as_ref()));
    if let Some(logo) = &new_logo {
        query.push(", logo_url = ").push_bind(logo);
    }
    query
        .push(", logo_height = ")
        .push_bind(input.logo_height.unwrap_or(48));
    query.push(", state_taxes = ").push_bind(&clean.tax);
    query.push(", max_users = ").push_bind(input.max_users);
    for (column, value) in [
        ("cloudtalk_access_key", &input.cloudtalk_access_key),
        ("cloudtalk_access_secret", &input.cloudtalk_access_secret),
        ("ringcentral_client_id", &input.ringcentral_client_id),
        (
            "ringcentral_client_secret",
            &input.ringcentral_client_secret,
        ),
        ("ringcentral_jwt", &input.ringcentral_jwt),
        ("ringcentral_server_url", &input.ringcentral_server_url),
        ("signwell_api_key", &input.signwell_api_key),
    ] {
        query
            .push(format!(", {column} = "))
            .push_bind(text(value.as_ref()));
    }
    query
        .push(", quickbooks_enabled = ")
        .push_bind(i32::from(input.quickbooks_enabled.unwrap_or(true)));
    if let Some(on) = input.signature_enabled {
        query
            .push(", signature_enabled = ")
            .push_bind(i32::from(on));
    }
    query
        .push(" WHERE id = ")
        .push_bind(id)
        .push(" AND deleted_at IS NULL");
    query.build().execute(&state.pool).await?;
    deliverability::ensure_company_trust(&state.pool, &state.dns, id, input.domain.as_deref())
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `?confirm_name=` of `DELETE /v1/superuser/companies/{id}`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ConfirmName {
    /// The company's exact name, typed by the super admin.
    #[serde(default)]
    pub confirm_name: String,
}

/// Delete a company
///
/// Marks the company deleted. `confirm_name` must be its exact name (400
/// otherwise). Its Granite Phone calling is switched off and its phones are
/// signed out (Telnyx credentials and push tokens dropped).
#[crate::openapi::path(
    delete,
    path = "/v1/superuser/companies/{id}",
    operation_id = "delete_platform_company",
    tag = "superuser",
    params(("id" = i32, Path, description = "Company id"), ConfirmName),
    responses(
        (status = 204, description = "Deleted"),
        (status = 400, description = "The name does not match", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a superuser", body = ErrorBody),
        (status = 404, description = "No such company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_company(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i32>,
    Query(confirm): Query<ConfirmName>,
) -> ApiResult<StatusCode> {
    superuser(&user)?;
    let name: Option<String> =
        sqlx::query_scalar("SELECT name FROM company WHERE id = ? AND deleted_at IS NULL")
            .bind(id)
            .fetch_optional(&state.pool)
            .await?;
    let name = name.ok_or(ApiError::NotFound)?;
    if confirm.confirm_name.trim() != name {
        return Err(ApiError::BadRequest("Company name does not match".into()));
    }
    // Granite Phone stops with the company: calling off, then its phones
    // signed out (Telnyx credentials and push tokens dropped).
    let mut tx = state.pool.begin().await?;
    sqlx::query("UPDATE company SET deleted_at = NOW() WHERE id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE telnyx_company_config SET enabled = 0 WHERE company_id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    if let Err(err) = revoke_company_phones(&state, id, RevokeReason::CompanyDeleted).await {
        // A deleted company's sign-ins no longer work; the phone job ends them.
        tracing::warn!(
            company_id = id,
            ?err,
            "phones of a deleted company not signed out"
        );
    }
    Ok(StatusCode::NO_CONTENT)
}

/// A user of a company.
#[derive(Debug, Serialize, ToSchema)]
pub struct CompanyUser {
    /// User id.
    pub id: i32,
    /// Name, or null.
    pub name: Option<String>,
    /// Email, or null.
    pub email: Option<String>,
    /// Phone, or null.
    pub phone_number: Option<String>,
    /// Admin access.
    pub is_admin: bool,
    /// Platform superuser.
    pub is_superuser: bool,
    /// `CloudTalk` agent id as saved, or null.
    pub cloudtalk_agent_id: Option<String>,
    /// `CloudTalk` number as saved, or null.
    pub cloudtalk_phone_number: Option<String>,
    /// Holds the Manager position (manages users).
    pub can_manage_users: bool,
    /// Runs the guided tours (not an installer or walk-in account).
    pub runs_onboarding: bool,
    /// Counts toward the user limit (not walk-in only).
    pub counts_toward_user_limit: bool,
    /// When the first-run tour was completed (ISO 8601), or null.
    pub onboarding_completed_at: Option<String>,
    /// A tours record exists (`progress` is saved, not the empty default).
    pub onboarding_started: bool,
    /// Saved tours progress.
    pub progress: OnboardingState,
}

/// `GET /v1/superuser/companies/{id}/users`.
#[derive(Debug, Serialize, ToSchema)]
pub struct CompanyUsers {
    /// Active users, by name.
    pub users: Vec<CompanyUser>,
    /// Deleted users.
    pub deleted_count: i64,
    /// The company has `CloudTalk` keys (the tours read it).
    pub has_cloudtalk_api: bool,
}

#[derive(sqlx::FromRow)]
struct UserRow {
    id: i32,
    name: Option<String>,
    email: Option<String>,
    phone_number: Option<String>,
    is_admin: bool,
    is_superuser: bool,
    cloudtalk_agent_id: Option<String>,
    cloudtalk_phone_number: Option<String>,
}

/// List a company's users
///
/// The company's active users with their roles, `CloudTalk` fields and
/// guided tours progress, and how many users were deleted.
#[crate::openapi::path(
    get,
    path = "/v1/superuser/companies/{id}/users",
    operation_id = "list_platform_company_users",
    tag = "superuser",
    params(("id" = i32, Path, description = "Company id")),
    responses(
        (status = 200, description = "The users", body = CompanyUsers),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a superuser", body = ErrorBody),
        (status = 404, description = "No such company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn company_users(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<CompanyUsers>> {
    superuser(&user)?;
    company_exists(&state, id).await?;
    let pool = &state.pool;
    let rows: Vec<UserRow> = sqlx::query_as(
        "SELECT id, name, email, phone_number, COALESCE(is_admin, 0) <> 0 AS is_admin,
                COALESCE(is_superuser, 0) <> 0 AS is_superuser,
                CAST(cloudtalk_agent_id AS CHAR) AS cloudtalk_agent_id,
                CAST(cloudtalk_phone_number AS CHAR) AS cloudtalk_phone_number
           FROM users WHERE company_id = ? AND is_deleted = 0 ORDER BY name ASC, id",
    )
    .bind(id)
    .fetch_all(pool)
    .await?;
    let positions: Vec<(i32, i32)> =
        sqlx::query_as("SELECT user_id, position_id FROM users_positions WHERE company_id = ?")
            .bind(id)
            .fetch_all(pool)
            .await?;
    let mut held: HashMap<i32, Vec<i32>> = HashMap::new();
    for (user_id, position) in positions {
        held.entry(user_id).or_default().push(position);
    }
    let ids: Vec<i32> = rows.iter().map(|row| row.id).collect();
    let mut records = store::load_records(pool, &ids).await?;
    let deleted_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE company_id = ? AND is_deleted = 1")
            .bind(id)
            .fetch_one(pool)
            .await?;
    let has_cloudtalk_api: bool = sqlx::query_scalar(
        "SELECT COALESCE(cloudtalk_access_key, '') <> '' AND COALESCE(cloudtalk_access_secret, '') <> ''
           FROM company WHERE id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await?;
    let users = rows
        .into_iter()
        .map(|row| {
            let positions = held.get(&row.id).map(Vec::as_slice).unwrap_or_default();
            let record = records.remove(&row.id).unwrap_or_default();
            CompanyUser {
                can_manage_users: positions.contains(&MANAGER),
                runs_onboarding: row.is_superuser
                    || !positions.iter().any(|p| *p == CHECK_IN || *p == INSTALLER),
                counts_toward_user_limit: !(positions.contains(&CHECK_IN)
                    && positions.iter().all(|p| *p == CHECK_IN)),
                onboarding_completed_at: record.completed_at,
                onboarding_started: record.has_row,
                progress: record.state,
                id: row.id,
                name: row.name,
                email: row.email,
                phone_number: row.phone_number,
                is_admin: row.is_admin,
                is_superuser: row.is_superuser,
                cloudtalk_agent_id: row.cloudtalk_agent_id,
                cloudtalk_phone_number: row.cloudtalk_phone_number,
            }
        })
        .collect();
    Ok(Json(CompanyUsers {
        users,
        deleted_count,
        has_cloudtalk_api,
    }))
}

async fn company_domain(state: &AppState, id: i32) -> ApiResult<Option<String>> {
    let domain: Option<Option<String>> =
        sqlx::query_scalar("SELECT domain FROM company WHERE id = ? AND deleted_at IS NULL")
            .bind(id)
            .fetch_optional(&state.pool)
            .await?;
    domain.ok_or(ApiError::NotFound)
}

/// Get a company domain's deliverability
///
/// Checks the company domain's email deliverability (DNS: MX, SPF, DMARC,
/// DKIM, blocklists; a check of the same domain from the last week is
/// reused) and answers it; null when the company has no usable domain.
#[crate::openapi::path(
    get,
    path = "/v1/superuser/companies/{id}/domain-trust",
    operation_id = "get_platform_company_domain_trust",
    tag = "superuser",
    params(("id" = i32, Path, description = "Company id")),
    responses(
        (status = 200, description = "The deliverability, or null", body = Option<DomainTrust>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a superuser", body = ErrorBody),
        (status = 404, description = "No such company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn domain_trust(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<Option<DomainTrust>>> {
    superuser(&user)?;
    let domain = company_domain(&state, id).await?;
    Ok(Json(
        deliverability::ensure_company_trust(&state.pool, &state.dns, id, domain.as_deref())
            .await?,
    ))
}

/// Whether a company receives email through SES.
#[derive(Debug, Serialize, ToSchema)]
pub struct SesInbound {
    /// The domain checked, or null without one.
    pub domain: Option<String>,
    /// SES verified the domain; null when SES could not be asked.
    pub ses_verified: Option<bool>,
    /// Verification is pending at SES.
    pub ses_pending: bool,
    /// The domain's MX points at SES email receiving (`inbound-smtp.`).
    pub mx_inbound: bool,
    /// Verified and the MX points at SES: inbound email works.
    pub inbound_ready: bool,
}

/// Get a company's SES inbound status
///
/// Whether SES verified the company's domain and its MX points at SES email
/// receiving, i.e. whether customers' replies reach the CRM.
#[crate::openapi::path(
    get,
    path = "/v1/superuser/companies/{id}/ses-inbound",
    operation_id = "get_platform_company_ses_inbound",
    tag = "superuser",
    params(("id" = i32, Path, description = "Company id")),
    responses(
        (status = 200, description = "The status", body = SesInbound),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a superuser", body = ErrorBody),
        (status = 404, description = "No such company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn ses_inbound(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<SesInbound>> {
    use crate::deliverability::Dns;
    superuser(&user)?;
    let domain = deliverability::company_domain(company_domain(&state, id).await?.as_deref());
    let Some(domain) = domain else {
        return Ok(Json(SesInbound {
            domain: None,
            ses_verified: Some(false),
            ses_pending: false,
            mx_inbound: false,
            inbound_ready: false,
        }));
    };
    let mx = tokio::time::timeout(std::time::Duration::from_secs(3), state.dns.mx(&domain));
    let (sets, mx) = tokio::join!(state.ses.sets(), mx);
    let mx_inbound = matches!(mx, Ok(Ok(records)) if records.iter().any(|(host, _)| host.to_lowercase().contains("inbound-smtp.")));
    let ses_verified = sets
        .as_ref()
        .map(|(verified, _)| verified.contains(&domain));
    let ses_pending = sets
        .as_ref()
        .is_some_and(|(_, pending)| pending.contains(&domain));
    Ok(Json(SesInbound {
        inbound_ready: ses_verified == Some(true) && mx_inbound,
        domain: Some(domain),
        ses_verified,
        ses_pending,
        mx_inbound,
    }))
}

async fn facts(state: &AppState, id: i32) -> ApiResult<CompanyFacts> {
    let sql = format!(
        "SELECT {} FROM company c WHERE c.id = ? AND c.deleted_at IS NULL",
        setup_checklist::FACTS_SQL
    );
    sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or(ApiError::NotFound)
}

/// Get a company's setup checklist
///
/// What is done of the company's setup: subdomain DNS, domain, key people,
/// inbound email (SES), stones on its website, tours, integrations; each
/// automatically or ticked by hand.
#[crate::openapi::path(
    get,
    path = "/v1/superuser/companies/{id}/setup-checklist",
    operation_id = "get_platform_company_setup_checklist",
    tag = "superuser",
    params(("id" = i32, Path, description = "Company id")),
    responses(
        (status = 200, description = "The checklist", body = Checklist),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a superuser", body = ErrorBody),
        (status = 404, description = "No such company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn setup_checklist(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<Checklist>> {
    superuser(&user)?;
    let company = facts(&state, id).await?;
    let mut lists = setup_checklist::checklists(&state, std::slice::from_ref(&company)).await?;
    lists.remove(&id).map(Json).ok_or(ApiError::NotFound)
}

/// Body of `PUT .../setup-checklist/items/{key}`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct Tick {
    /// Ticked by hand.
    pub checked: bool,
}

/// Tick a setup item by hand
///
/// Marks a checklist item done (or not) by hand; automatic checks still
/// count either way.
#[crate::openapi::path(
    put,
    path = "/v1/superuser/companies/{id}/setup-checklist/items/{key}",
    operation_id = "tick_platform_company_setup_item",
    tag = "superuser",
    params(("id" = i32, Path, description = "Company id"), ("key" = String, Path, description = "Item key, e.g. `users.manager`")),
    request_body = Tick,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Unknown item", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a superuser", body = ErrorBody),
        (status = 404, description = "No such company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn tick_setup_item(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((id, key)): Path<(i32, String)>,
    JsonBody(input): JsonBody<Tick>,
) -> ApiResult<StatusCode> {
    superuser(&user)?;
    if !setup_checklist::ITEM_KEYS.contains(&key.as_str()) {
        return Err(ApiError::BadRequest("Invalid checklist item".into()));
    }
    company_exists(&state, id).await?;
    setup_checklist::set_manual(&state.pool, id, &key, input.checked).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Body of the switch routes.
#[derive(Debug, Deserialize, ToSchema)]
pub struct Switch {
    /// On or off.
    pub enabled: bool,
}

/// Switch a company feature
///
/// Turns `quickbooks`, `signature`, `onboarding` or `leaderboard` on or off
/// for the company (an explicit setting, over the platform default).
#[crate::openapi::path(
    put,
    path = "/v1/superuser/companies/{id}/features/{feature}",
    operation_id = "switch_platform_company_feature",
    tag = "superuser",
    params(("id" = i32, Path, description = "Company id"), ("feature" = String, Path, description = "`quickbooks`, `signature`, `onboarding` or `leaderboard`")),
    request_body = Switch,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Unknown feature", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a superuser", body = ErrorBody),
        (status = 404, description = "No such company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn switch_feature(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((id, feature)): Path<(i32, String)>,
    JsonBody(input): JsonBody<Switch>,
) -> ApiResult<StatusCode> {
    superuser(&user)?;
    let column = match feature.as_str() {
        "quickbooks" => "quickbooks_enabled",
        "signature" => "signature_enabled",
        "onboarding" => "onboarding_enabled",
        "leaderboard" => "leaderboard_enabled",
        _ => return Err(ApiError::BadRequest("Invalid feature toggle".into())),
    };
    let updated = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE company SET {column} = ? WHERE id = ? AND deleted_at IS NULL"
    )))
    .bind(i32::from(input.enabled))
    .bind(id)
    .execute(&state.pool)
    .await?;
    if updated.rows_affected() == 0 {
        company_exists(&state, id).await?;
    }
    crate::posthog::capture(
        &state.telephony_http,
        "company_feature_toggled",
        &user.id.to_string(),
        serde_json::json!({ "company_id": id, "feature": feature, "value": input.enabled, "superuser_id": user.id }),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// Require Stripe payment
///
/// Whether the company's access depends on its Stripe subscription (off:
/// it pays some other way).
#[crate::openapi::path(
    put,
    path = "/v1/superuser/companies/{id}/billing-required",
    operation_id = "set_platform_company_billing_required",
    tag = "superuser",
    params(("id" = i32, Path, description = "Company id")),
    request_body = Switch,
    responses(
        (status = 204, description = "Saved"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a superuser", body = ErrorBody),
        (status = 404, description = "No such company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn set_billing_required(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<Switch>,
) -> ApiResult<StatusCode> {
    superuser(&user)?;
    company_exists(&state, id).await?;
    sqlx::query("UPDATE company SET billing_required = ? WHERE id = ? AND deleted_at IS NULL")
        .bind(i32::from(input.enabled))
        .bind(id)
        .execute(&state.pool)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
