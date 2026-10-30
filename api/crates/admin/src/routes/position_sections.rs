//! Position access on the Company page: which sections of the app Inventory
//! Manager and Scheduling Manager open in this company.
//!
//! - `GET    /v1/company/position-sections`                → every section, and each position's
//! - `PUT    /v1/company/position-sections/{position_id}`  → this company's own list
//! - `DELETE /v1/company/position-sections/{position_id}`  → back to the defaults
//!
//! The API enforces them (`crate::auth::sections`); the app hides the tabs.

use crate::auth::UserManager;
use crate::auth::sections::{self, SECTIONED_POSITIONS, SECTIONS};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::openapi::{OpenApiRouter, routes};
use crate::routes::integrations::{TargetCompany, target};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list))
        .routes(routes!(save, reset))
}

/// A part of the app a position may open.
#[derive(Debug, Serialize, ToSchema)]
pub struct SectionInfo {
    /// Stable key (`inventory`, `customers`, `schedule`, ...).
    pub key: &'static str,
    /// Its name on the Company page.
    pub label: &'static str,
    /// Opens admin pages (e.g. the admin inventory) without making the user
    /// a company admin.
    pub admin: bool,
}

/// One sectioned position in the company.
#[derive(Debug, Serialize, ToSchema)]
pub struct PositionSections {
    /// `positions.id`: 12 Inventory Manager, 13 Scheduling Manager.
    pub position_id: i32,
    /// The sections it opens in this company, in [`SectionInfo`] order.
    pub sections: Vec<&'static str>,
    /// The sections it opens in a company that has not changed it.
    pub default_sections: Vec<&'static str>,
    /// The company changed it (false: it uses the defaults).
    pub customized: bool,
}

/// `GET /v1/company/position-sections`.
#[derive(Debug, Serialize, ToSchema)]
pub struct PositionAccess {
    /// Every section, in the order to show them.
    pub sections: Vec<SectionInfo>,
    /// Each sectioned position. My Account and Report a Bug always stay open.
    pub positions: Vec<PositionSections>,
}

/// Body of `PUT /v1/company/position-sections/{position_id}`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PositionSectionsInput {
    /// Section keys the position opens (may be empty: then only My Account).
    pub sections: Vec<String>,
}

/// List position access
///
/// Every section of the app, and for Inventory Manager and Scheduling
/// Manager the sections they open in the company, their defaults and
/// whether the company changed them. Someone who holds only these positions
/// reaches only their sections (with several, all of them together); any
/// other position except the portal ones keeps the whole app. Superusers and
/// company managers (admins with the Manager position); superusers may pass
/// `company_id`. Change one with `PUT
/// /v1/company/position-sections/{position_id}`.
#[crate::openapi::path(
    get,
    path = "/v1/company/position-sections",
    operation_id = "list_position_sections",
    tag = "company",
    params(TargetCompany),
    responses(
        (status = 200, description = "The sections and each position's", body = PositionAccess),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller does not manage the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list(
    State(state): State<AppState>,
    UserManager(user): UserManager,
    Query(query): Query<TargetCompany>,
) -> ApiResult<Json<PositionAccess>> {
    let company_id = target(&user, &query);
    let positions = sections::company_sections(&state.pool, company_id)
        .await?
        .into_iter()
        .map(|(position_id, open, customized)| PositionSections {
            position_id,
            sections: open,
            default_sections: sections::default_sections(position_id).to_vec(),
            customized,
        })
        .collect();
    Ok(Json(PositionAccess {
        sections: SECTIONS
            .iter()
            .map(|section| SectionInfo {
                key: section.key,
                label: section.label,
                admin: section.admin,
            })
            .collect(),
        positions,
    }))
}

fn sectioned(position_id: i32) -> ApiResult<()> {
    if SECTIONED_POSITIONS.contains(&position_id) {
        Ok(())
    } else {
        Err(ApiError::BadRequest(
            "Only Inventory Manager and Scheduling Manager have sections".into(),
        ))
    }
}

/// Save a position's sections
///
/// Replaces the sections the position opens in the company, from then on
/// for every user holding it (their next request). Unknown keys are refused.
/// `DELETE` the same path to go back to the defaults.
#[crate::openapi::path(
    put,
    path = "/v1/company/position-sections/{position_id}",
    operation_id = "save_position_sections",
    tag = "company",
    params(
        ("position_id" = i32, Path, description = "12 Inventory Manager, 13 Scheduling Manager"),
        TargetCompany,
    ),
    request_body = PositionSectionsInput,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Not a sectioned position, or an unknown section", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller does not manage the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn save(
    State(state): State<AppState>,
    UserManager(user): UserManager,
    Path(position_id): Path<i32>,
    Query(query): Query<TargetCompany>,
    JsonBody(input): JsonBody<PositionSectionsInput>,
) -> ApiResult<StatusCode> {
    sectioned(position_id)?;
    if let Some(unknown) = input
        .sections
        .iter()
        .find(|key| sections::section(key).is_none())
    {
        return Err(ApiError::BadRequest(format!("Unknown section: {unknown}")));
    }
    let keys = sections::parse_sections(&input.sections.join(",")).join(",");
    sqlx::query(
        "INSERT INTO company_position_sections (company_id, position_id, sections)
         VALUES (?, ?, ?)
         ON DUPLICATE KEY UPDATE sections = VALUES(sections)",
    )
    .bind(target(&user, &query))
    .bind(position_id)
    .bind(keys)
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Reset a position's sections
///
/// The position opens its default sections again in the company.
#[crate::openapi::path(
    delete,
    path = "/v1/company/position-sections/{position_id}",
    operation_id = "reset_position_sections",
    tag = "company",
    params(
        ("position_id" = i32, Path, description = "12 Inventory Manager, 13 Scheduling Manager"),
        TargetCompany,
    ),
    responses(
        (status = 204, description = "Back to the defaults"),
        (status = 400, description = "Not a sectioned position", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller does not manage the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn reset(
    State(state): State<AppState>,
    UserManager(user): UserManager,
    Path(position_id): Path<i32>,
    Query(query): Query<TargetCompany>,
) -> ApiResult<StatusCode> {
    sectioned(position_id)?;
    sqlx::query("DELETE FROM company_position_sections WHERE company_id = ? AND position_id = ?")
        .bind(target(&user, &query))
        .bind(position_id)
        .execute(&state.pool)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
