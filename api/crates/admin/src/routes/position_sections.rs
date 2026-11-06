//! Position access on the Company page: which tabs of the app each position
//! opens in this company, and which skills it has there.
//!
//! - `GET    /v1/company/position-sections`                → every tab and skill, and each position's
//! - `PUT    /v1/company/position-sections/{position_id}`  → this company's own lists
//! - `DELETE /v1/company/position-sections/{position_id}`  → back to the defaults
//!
//! The API applies them (`crate::auth::sections`, `crate::auth::skills`);
//! the app hides the tabs and the skills' buttons.

use crate::auth::UserManager;
use crate::auth::sections::{self, CONFIGURABLE_POSITIONS, TABS};
use crate::auth::skills::{self, SKILLS};
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

/// A tab a position may open.
#[derive(Debug, Serialize, ToSchema)]
pub struct SectionInfo {
    /// Stable key (`stones`, `customers`, `schedule`, `admin_stones`, ...).
    pub key: &'static str,
    /// Its name on the Company page (admin tabs are listed apart).
    pub label: &'static str,
    /// An admin page: it opens for someone without the Admin switch only
    /// when one of their positions has it (the Admin switch opens them all).
    pub admin: bool,
}

/// A skill a position may have: something its people may do on the tabs
/// they open.
#[derive(Debug, Serialize, ToSchema)]
pub struct SkillInfo {
    /// Stable key (`sell_slabs`, `edit_transactions`, `edit_quotes`,
    /// `plan_schedule`).
    pub key: &'static str,
    /// Its name on the Company page.
    pub label: &'static str,
    /// What it lets people do.
    pub description: &'static str,
}

/// One position whose tabs the company may change.
#[derive(Debug, Serialize, ToSchema)]
pub struct PositionSections {
    /// `positions.id`: 1 Sales Rep, 2 Sales Manager, 5 Manager, 10 Office
    /// Manager, 3 Shop Manager, 12 Inventory Manager, 13 Scheduling Manager.
    pub position_id: i32,
    /// The tabs it opens in this company, in [`SectionInfo`] order.
    pub sections: Vec<&'static str>,
    /// The tabs it opens in a company that has not changed it.
    pub default_sections: Vec<&'static str>,
    /// Its skills in this company, in [`SkillInfo`] order.
    pub skills: Vec<&'static str>,
    /// Its skills in a company that has not changed them.
    pub default_skills: Vec<&'static str>,
    /// The company changed it (false: it uses the defaults).
    pub customized: bool,
}

/// `GET /v1/company/position-sections`.
#[derive(Debug, Serialize, ToSchema)]
pub struct PositionAccess {
    /// Every tab, in the order to show them: the main app, then admin pages.
    pub sections: Vec<SectionInfo>,
    /// Every skill, in the order to show them.
    pub skills: Vec<SkillInfo>,
    /// Each position whose tabs the company may change. My Account and
    /// Report a Bug always stay open.
    pub positions: Vec<PositionSections>,
}

/// Body of `PUT /v1/company/position-sections/{position_id}`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PositionSectionsInput {
    /// Tab keys the position opens (may be empty: then only My Account).
    pub sections: Vec<String>,
    /// Skill keys it has (may be empty); null or left out: the default
    /// skills, which follow later changes to the defaults.
    #[serde(default)]
    pub skills: Option<Vec<String>>,
}

/// List position access
///
/// Every tab of the app, and for each position the company may change (all
/// but Super Admin and the portal ones: Shop Worker, Installer, Marketing,
/// Walk-in, Template Guy) the tabs it opens, its defaults and whether the
/// company changed them. Someone with several positions gets all their tabs
/// together. By default every position opens the whole main app, except
/// Inventory Manager and Scheduling Manager. The Admin switch still opens
/// every admin page; an admin tab opens that page to someone without it.
/// Every skill too, and each position's: by default every position sells
/// slabs and works with transactions and quotes, and only Scheduling Manager
/// plans the schedule; the Admin switch keeps every skill. Superusers and
/// company managers (admins with the Manager position);
/// superusers may pass `company_id`. Change one with `PUT
/// /v1/company/position-sections/{position_id}`.
#[crate::openapi::path(
    get,
    path = "/v1/company/position-sections",
    operation_id = "list_position_sections",
    tag = "company",
    params(TargetCompany),
    responses(
        (status = 200, description = "The tabs and each position's", body = PositionAccess),
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
    let company_skills = skills::company_skills(&state.pool, company_id).await;
    let positions = sections::company_tabs(&state.pool, company_id)
        .await?
        .into_iter()
        .map(|(position_id, open, customized)| {
            let own = company_skills.iter().find(|(id, _, _)| *id == position_id);
            PositionSections {
                position_id,
                sections: open,
                default_sections: sections::default_tabs(position_id),
                skills: own.map_or_else(
                    || skills::default_skills(position_id),
                    |(_, have, _)| have.clone(),
                ),
                default_skills: skills::default_skills(position_id),
                customized: customized || own.is_some_and(|(_, _, changed)| *changed),
            }
        })
        .collect();
    Ok(Json(PositionAccess {
        sections: TABS
            .iter()
            .map(|tab| SectionInfo {
                key: tab.key,
                label: tab.label,
                admin: tab.admin,
            })
            .collect(),
        skills: SKILLS
            .iter()
            .map(|skill| SkillInfo {
                key: skill.key,
                label: skill.label,
                description: skill.description,
            })
            .collect(),
        positions,
    }))
}

fn configurable(position_id: i32) -> ApiResult<()> {
    if CONFIGURABLE_POSITIONS.contains(&position_id) {
        Ok(())
    } else {
        Err(ApiError::BadRequest(
            "This position's tabs can't be changed: Super Admin keeps the whole app, and Shop Worker, Installer, Marketing, Walk-in and Template Guy keep their own apps".into(),
        ))
    }
}

/// Save a position's tabs
///
/// Replaces the tabs the position opens in the company, and its skills
/// (null: the defaults), from then on for every user holding it (their next
/// request). Unknown keys are refused.
/// `DELETE` the same path to go back to the defaults.
#[crate::openapi::path(
    put,
    path = "/v1/company/position-sections/{position_id}",
    operation_id = "save_position_sections",
    tag = "company",
    params(
        ("position_id" = i32, Path, description = "1 Sales Rep, 2 Sales Manager, 5 Manager, 10 Office Manager, 3 Shop Manager, 12 Inventory Manager, 13 Scheduling Manager"),
        TargetCompany,
    ),
    request_body = PositionSectionsInput,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "A position whose tabs can't be changed, or an unknown tab or skill", body = ErrorBody),
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
    configurable(position_id)?;
    if let Some(unknown) = input
        .sections
        .iter()
        .find(|key| sections::tab(key).is_none())
    {
        return Err(ApiError::BadRequest(format!("Unknown tab: {unknown}")));
    }
    if let Some(unknown) = input
        .skills
        .iter()
        .flatten()
        .find(|key| skills::skill(key).is_none())
    {
        return Err(ApiError::BadRequest(format!("Unknown skill: {unknown}")));
    }
    let keys = sections::parse_tabs(&input.sections.join(",")).join(",");
    let own_skills = input
        .skills
        .map(|keys| skills::parse_skills(&keys.join(",")).join(","));
    sqlx::query(
        "INSERT INTO company_position_sections (company_id, position_id, sections, skills)
         VALUES (?, ?, ?, ?)
         ON DUPLICATE KEY UPDATE sections = VALUES(sections), skills = VALUES(skills)",
    )
    .bind(target(&user, &query))
    .bind(position_id)
    .bind(keys)
    .bind(own_skills)
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Reset a position's tabs
///
/// The position opens its default tabs, with its default skills, again in
/// the company.
#[crate::openapi::path(
    delete,
    path = "/v1/company/position-sections/{position_id}",
    operation_id = "reset_position_sections",
    tag = "company",
    params(
        ("position_id" = i32, Path, description = "1 Sales Rep, 2 Sales Manager, 5 Manager, 10 Office Manager, 3 Shop Manager, 12 Inventory Manager, 13 Scheduling Manager"),
        TargetCompany,
    ),
    responses(
        (status = 204, description = "Back to the defaults"),
        (status = 400, description = "A position whose tabs can't be changed", body = ErrorBody),
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
    configurable(position_id)?;
    sqlx::query("DELETE FROM company_position_sections WHERE company_id = ? AND position_id = ?")
        .bind(target(&user, &query))
        .bind(position_id)
        .execute(&state.pool)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
