//! SMS templates (`cloudtalk_sms_templates`, used by both providers).
//!
//! A company template can be an automated drip: sent `hour_delay` hours
//! and `minute_delay` minutes after a deal enters `lead_list_id`, to
//! customers whose source is in `customer_sources` (default: leads only; one
//! template per list and delay). Replaces the Remix `api.sms.templates*` routes and their unused
//! `api.{cloudtalk,ringcentral}.sms.templates*` twins.

use super::handlers::template_body_error;
use super::order;
use super::schemas::{OrderingType, TemplateScope};
use crate::auth::EmployeeUser;
use crate::auth::session::SessionUser;
use crate::automation_notice::{self, Snapshot};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::{JsonBody, MultiQuery};
use crate::routes::company::queries::company_features;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, MySqlPool};
use utoipa::{IntoParams, ToSchema};

/// Longest text a template may hold (one SMS body).
const MAX_BODY: usize = 1600;
const MAX_HOUR_DELAY: i32 = 8760;
/// Source a drip goes to when the template names none.
const DEFAULT_SOURCE: &str = "leads";

#[derive(Debug, FromRow)]
struct Row {
    id: i32,
    user_id: Option<i32>,
    name: String,
    body: String,
    lead_list_id: Option<i32>,
    hour_delay: Option<i32>,
    minute_delay: i32,
    customer_sources: Option<String>,
    position: i32,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
}

const COLUMNS: &str = "id, user_id, name, body, lead_list_id, hour_delay, minute_delay, \
     CAST(customer_sources AS CHAR) AS customer_sources, position, created_at, updated_at";

/// An SMS template.
#[derive(Debug, Serialize, ToSchema)]
pub struct SmsTemplate {
    /// The template.
    pub id: i32,
    /// Shown in the picker.
    pub name: String,
    /// The text, with `{{variables}}`.
    pub body: String,
    /// Order among the company's (or the user's) templates.
    pub position: i32,
    pub scope: TemplateScope,
    /// The caller may change it.
    pub can_edit: bool,
    /// When it was made.
    pub created_at: Option<DateTime<Utc>>,
    /// When it last changed.
    pub updated_at: Option<DateTime<Utc>>,
    /// Company drip: the deal list that starts it.
    pub lead_list_id: Option<i32>,
    /// Company drip: hours after a deal enters the list.
    pub hour_delay: Option<i32>,
    /// Company drip: minutes on top of `hour_delay`, 0 to 59.
    pub minute_delay: i32,
    /// Company drip: customer sources (`customers.source`) that get it.
    pub customer_sources: Vec<String>,
}

/// A deal list a drip can start from.
#[derive(Debug, Serialize, FromRow, ToSchema)]
pub struct DripList {
    /// The list.
    pub id: i32,
    /// Its name.
    pub name: String,
    /// Its pipeline.
    pub group_name: String,
}

/// Query of `GET /v1/sms-templates`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListQuery {
    /// `company`: only the company's templates, for admins to manage, with
    /// the deal lists drips can use.
    #[serde(default)]
    #[param(inline)]
    pub scope: TemplateScope,
}

/// `GET /v1/sms-templates`.
#[derive(Debug, Serialize, ToSchema)]
pub struct SmsTemplates {
    /// In picker order.
    pub templates: Vec<SmsTemplate>,
    /// With `scope=company`: deal lists for drips.
    pub deal_lists: Vec<DripList>,
}

/// When a company template sends itself.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct Drip {
    /// The deal list that starts it; null: no drip.
    pub lead_list_id: Option<i32>,
    /// Hours after a deal enters the list, 0 to 8760.
    pub hour_delay: Option<i32>,
    /// Minutes on top of `hour_delay`, 0 to 59.
    #[serde(default)]
    pub minute_delay: i32,
    /// Customer sources that get it; empty or left out: only leads.
    #[serde(default)]
    pub customer_sources: Vec<String>,
}

/// A checked drip as stored: list, hours, minutes, sources JSON.
#[derive(Debug, Clone)]
struct StoredDrip {
    list: Option<i32>,
    hours: Option<i32>,
    minutes: i32,
    sources: Option<String>,
}

impl StoredDrip {
    const NONE: Self = Self {
        list: None,
        hours: None,
        minutes: 0,
        sources: None,
    };

    fn of(row: &Row) -> Self {
        Self {
            list: row.lead_list_id,
            hours: row.hour_delay,
            minutes: row.minute_delay,
            sources: row.customer_sources.clone(),
        }
    }
}

/// The stored sources JSON as a list; leads when unset or unreadable.
fn sources_of(json: Option<&str>) -> Vec<String> {
    json.and_then(|raw| serde_json::from_str::<Vec<String>>(raw).ok())
        .filter(|list| !list.is_empty())
        .unwrap_or_else(|| vec![DEFAULT_SOURCE.to_string()])
}

/// Body of `POST /v1/sms-templates`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct NewSmsTemplate {
    /// Shown in the picker.
    pub name: String,
    /// The text, with `{{variables}}`, up to 1600 characters.
    pub body: String,
    /// `company` (admins only) or `personal` (default).
    #[serde(default)]
    pub scope: TemplateScope,
    /// Company templates only.
    pub drip: Option<Drip>,
}

/// Body of `PUT /v1/sms-templates/{id}`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct SmsTemplateUpdate {
    /// Shown in the picker.
    pub name: String,
    /// The text, with `{{variables}}`, up to 1600 characters.
    pub body: String,
    /// Company templates only; left out keeps the drip.
    pub drip: Option<Drip>,
}

const fn is_admin(user: &SessionUser) -> bool {
    user.is_admin || user.is_superuser
}

fn to_template(row: Row, user: &SessionUser, company_editable: bool) -> SmsTemplate {
    let company = row.user_id.is_none();
    SmsTemplate {
        id: row.id,
        name: row.name,
        body: row.body,
        position: row.position,
        scope: if company {
            TemplateScope::Company
        } else {
            TemplateScope::Personal
        },
        can_edit: if company {
            company_editable && is_admin(user)
        } else {
            row.user_id == Some(user.id)
        },
        created_at: row.created_at,
        updated_at: row.updated_at,
        lead_list_id: row.lead_list_id,
        hour_delay: row.hour_delay,
        minute_delay: row.minute_delay,
        customer_sources: sources_of(row.customer_sources.as_deref()),
    }
}

/// Texting needs `CloudTalk` or `RingCentral`.
async fn require_sms(pool: &MySqlPool, user: &SessionUser) -> ApiResult<()> {
    let features = company_features(pool, user.company_id).await?;
    if features.cloudtalk || features.ringcentral {
        Ok(())
    } else {
        Err(ApiError::Forbidden)
    }
}

fn checked_text(name: &str, body: &str) -> ApiResult<(String, String)> {
    let (name, body) = (name.trim(), body.trim());
    let bad = |message: &str| Err(ApiError::BadRequest(message.to_string()));
    if name.is_empty() {
        return bad("Template name is required");
    }
    if name.chars().count() > 255 {
        return bad("Template name is at most 255 characters");
    }
    if body.is_empty() {
        return bad("Template body is required");
    }
    if body.chars().count() > MAX_BODY {
        return bad("Template body must be at most 1600 characters");
    }
    if let Some(error) = template_body_error(body) {
        return bad(error);
    }
    Ok((name.to_string(), body.to_string()))
}

/// A drip needs both a list and a delay in range; without a list there is
/// none. Sources are trimmed, lowercased and deduped; none means leads.
fn checked_drip(drip: Drip) -> ApiResult<StoredDrip> {
    let Some(list) = drip.lead_list_id.filter(|id| *id > 0) else {
        return Ok(StoredDrip::NONE);
    };
    let hours = match drip.hour_delay {
        None => {
            return Err(ApiError::BadRequest(
                "Set how long to wait before sending.".into(),
            ));
        }
        Some(hours) if !(0..=MAX_HOUR_DELAY).contains(&hours) => {
            return Err(ApiError::BadRequest(
                "Delay must be between 0 and 8760 hours.".into(),
            ));
        }
        Some(hours) => hours,
    };
    if !(0..=59).contains(&drip.minute_delay) {
        return Err(ApiError::BadRequest(
            "Minutes must be between 0 and 59.".into(),
        ));
    }
    let mut sources: Vec<String> = Vec::new();
    for source in &drip.customer_sources {
        let source = source.trim().to_lowercase();
        if !source.is_empty() && !sources.contains(&source) {
            sources.push(source);
        }
    }
    let sources = (!sources.is_empty() && sources != [DEFAULT_SOURCE])
        .then(|| serde_json::to_string(&sources).unwrap_or_default());
    Ok(StoredDrip {
        list: Some(list),
        hours: Some(hours),
        minutes: drip.minute_delay,
        sources,
    })
}

/// Another company template already drips on this list and delay.
async fn drip_taken(
    pool: &MySqlPool,
    company_id: i32,
    drip: &StoredDrip,
    except: Option<i32>,
) -> ApiResult<()> {
    let (Some(list), Some(delay)) = (drip.list, drip.hours) else {
        return Ok(());
    };
    let taken: Option<i32> = sqlx::query_scalar(
        "SELECT id FROM cloudtalk_sms_templates
          WHERE company_id = ? AND user_id IS NULL AND lead_list_id = ? AND hour_delay = ?
            AND minute_delay = ? AND deleted_at IS NULL AND id <> ?
          LIMIT 1",
    )
    .bind(company_id)
    .bind(list)
    .bind(delay)
    .bind(drip.minutes)
    .bind(except.unwrap_or(0))
    .fetch_optional(pool)
    .await?;
    if taken.is_some() {
        return Err(ApiError::BadRequest(
            "Another SMS template already uses this list and delay.".into(),
        ));
    }
    Ok(())
}

async fn accessible(pool: &MySqlPool, user: &SessionUser, id: i32) -> ApiResult<Row> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM cloudtalk_sms_templates
          WHERE id = ? AND company_id = ? AND deleted_at IS NULL
            AND (user_id IS NULL OR user_id = ?)"
    )))
    .bind(id)
    .bind(user.company_id)
    .bind(user.id)
    .fetch_optional(pool)
    .await?
    .ok_or(ApiError::NotFound)
}

/// A template the caller may change: their own, or a company one for admins.
async fn editable(pool: &MySqlPool, user: &SessionUser, id: i32) -> ApiResult<Row> {
    let row = accessible(pool, user, id).await?;
    let allowed = row
        .user_id
        .map_or_else(|| is_admin(user), |owner| owner == user.id);
    if allowed {
        Ok(row)
    } else {
        Err(ApiError::NotFound)
    }
}

/// Pending drip texts of a template stop once it no longer drips.
async fn cancel_scheduled(pool: &MySqlPool, template_id: i32, reason: &str) -> ApiResult<()> {
    sqlx::query(
        "UPDATE scheduled_sms SET status = 'cancelled', error_message = ?
          WHERE template_id = ? AND status = 'pending'",
    )
    .bind(reason)
    .bind(template_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// List SMS templates
///
/// The company's templates and the caller's own, in the caller's picker
/// order (most used first unless they saved a custom order; then company
/// templates first, by position, newest first). With `scope=company`
/// (admins): only the company's, with the deal lists drips can start from.
#[utoipa::path(
    get,
    path = "/v1/sms-templates",
    operation_id = "list_sms_templates",
    tag = "templates",
    params(ListQuery),
    responses(
        (status = 200, description = "Templates", body = SmsTemplates),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "No SMS provider, or not an admin for the company scope", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    MultiQuery(query): MultiQuery<ListQuery>,
) -> ApiResult<Json<SmsTemplates>> {
    let pool = &state.pool;
    require_sms(pool, &user).await?;
    if query.scope == TemplateScope::Company {
        if !is_admin(&user) {
            return Err(ApiError::Forbidden);
        }
        let rows: Vec<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {COLUMNS} FROM cloudtalk_sms_templates
              WHERE company_id = ? AND user_id IS NULL AND deleted_at IS NULL
              ORDER BY position, created_at DESC"
        )))
        .bind(user.company_id)
        .fetch_all(pool)
        .await?;
        let deal_lists = sqlx::query_as(
            "SELECT dl.id, dl.name, gl.name AS group_name
               FROM deals_list dl
               JOIN groups_list gl ON gl.id = dl.group_id AND gl.deleted_at IS NULL
              WHERE dl.deleted_at IS NULL AND (gl.company_id = ? OR gl.id = 1)
              ORDER BY gl.created_at, dl.position, dl.id",
        )
        .bind(user.company_id)
        .fetch_all(pool)
        .await?;
        return Ok(Json(SmsTemplates {
            templates: rows
                .into_iter()
                .map(|row| to_template(row, &user, true))
                .collect(),
            deal_lists,
        }));
    }
    let mut rows: Vec<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM cloudtalk_sms_templates
          WHERE company_id = ? AND deleted_at IS NULL AND (user_id IS NULL OR user_id = ?)"
    )))
    .bind(user.company_id)
    .bind(user.id)
    .fetch_all(pool)
    .await?;
    let context = order::context(pool, user.id, OrderingType::Sms).await?;
    order::sort(
        &mut rows,
        |row| row.id,
        &context,
        |a, b| {
            a.user_id
                .is_some()
                .cmp(&b.user_id.is_some())
                .then_with(|| a.position.cmp(&b.position))
                .then_with(|| b.created_at.cmp(&a.created_at))
        },
    );
    Ok(Json(SmsTemplates {
        templates: rows
            .into_iter()
            .map(|row| to_template(row, &user, false))
            .collect(),
        deal_lists: Vec::new(),
    }))
}

/// Create an SMS template
///
/// `scope: company` (admins only) makes it the company's, and only a
/// company template can drip; otherwise it is the caller's own. It goes
/// last.
#[utoipa::path(
    post,
    path = "/v1/sms-templates",
    operation_id = "create_sms_template",
    tag = "templates",
    request_body = NewSmsTemplate,
    responses(
        (status = 201, description = "Created", body = SmsTemplate),
        (status = 400, description = "Missing name or body, unpaired braces, or a drip already taken", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "No SMS provider, or not an admin for a company template", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<NewSmsTemplate>,
) -> ApiResult<(StatusCode, Json<SmsTemplate>)> {
    let pool = &state.pool;
    require_sms(pool, &user).await?;
    let (name, body) = checked_text(&input.name, &input.body)?;
    let company = input.scope == TemplateScope::Company;
    if company && !is_admin(&user) {
        return Err(ApiError::Forbidden);
    }
    let drip = match input.drip {
        Some(drip) if company => checked_drip(drip)?,
        _ => StoredDrip::NONE,
    };
    drip_taken(pool, user.company_id, &drip, None).await?;
    let owner = (!company).then_some(user.id);
    let position: Option<i32> = sqlx::query_scalar(
        "SELECT MAX(position) FROM cloudtalk_sms_templates
          WHERE company_id = ? AND user_id <=> ? AND deleted_at IS NULL",
    )
    .bind(user.company_id)
    .bind(owner)
    .fetch_one(pool)
    .await?;
    let id = sqlx::query(
        "INSERT INTO cloudtalk_sms_templates
           (company_id, user_id, name, body, lead_list_id, hour_delay, minute_delay,
            customer_sources, position, created_by)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(user.company_id)
    .bind(owner)
    .bind(&name)
    .bind(&body)
    .bind(drip.list)
    .bind(drip.hours)
    .bind(drip.minutes)
    .bind(&drip.sources)
    .bind(position.map_or(0, |max| max + 1))
    .bind(user.id)
    .execute(pool)
    .await?
    .last_insert_id();
    let id = i32::try_from(id).unwrap_or_default();
    let row = accessible(pool, &user, id).await?;
    let after = automation_notice::sms_template(pool, user.company_id, id).await;
    automation_notice::notify(&state, &user, &Snapshot::Template(None), &after).await;
    Ok((StatusCode::CREATED, Json(to_template(row, &user, company))))
}

/// Edit an SMS template
///
/// The caller's own template, or (admins) a company one. Leaving out
/// `lead_list_id` / `hour_delay` keeps the drip; a template that stops
/// dripping cancels its pending drip texts.
#[utoipa::path(
    put,
    path = "/v1/sms-templates/{id}",
    operation_id = "update_sms_template",
    tag = "templates",
    params(("id" = i32, Path, description = "Template id")),
    request_body = SmsTemplateUpdate,
    responses(
        (status = 200, description = "Saved", body = SmsTemplate),
        (status = 400, description = "Missing name or body, unpaired braces, or a drip already taken", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "No SMS provider", body = ErrorBody),
        (status = 404, description = "No such template the caller may change", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<SmsTemplateUpdate>,
) -> ApiResult<Json<SmsTemplate>> {
    let pool = &state.pool;
    require_sms(pool, &user).await?;
    let existing = editable(pool, &user, id).await?;
    let (name, body) = checked_text(&input.name, &input.body)?;
    let company = existing.user_id.is_none();
    let drip = match input.drip {
        Some(drip) if company => checked_drip(drip)?,
        _ => StoredDrip::of(&existing),
    };
    if company {
        drip_taken(pool, user.company_id, &drip, Some(id)).await?;
    }
    let before = automation_notice::sms_template(pool, user.company_id, id).await;
    sqlx::query(
        "UPDATE cloudtalk_sms_templates
            SET name = ?, body = ?, lead_list_id = ?, hour_delay = ?, minute_delay = ?,
                customer_sources = ?, updated_at = NOW()
          WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(&name)
    .bind(&body)
    .bind(drip.list)
    .bind(drip.hours)
    .bind(drip.minutes)
    .bind(&drip.sources)
    .bind(id)
    .bind(user.company_id)
    .execute(pool)
    .await?;
    if company && (drip.list.is_none() || drip.hours.is_none()) {
        cancel_scheduled(pool, id, "Template deleted").await?;
    }
    let after = automation_notice::sms_template(pool, user.company_id, id).await;
    automation_notice::notify(&state, &user, &before, &after).await;
    let row = accessible(pool, &user, id).await?;
    Ok(Json(to_template(row, &user, company)))
}

/// Delete an SMS template
///
/// The caller's own template, or (admins) a company one; its pending drip
/// texts are cancelled.
#[utoipa::path(
    delete,
    path = "/v1/sms-templates/{id}",
    operation_id = "delete_sms_template",
    tag = "templates",
    params(("id" = i32, Path, description = "Template id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "No SMS provider", body = ErrorBody),
        (status = 404, description = "No such template the caller may change", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    let pool = &state.pool;
    require_sms(pool, &user).await?;
    editable(pool, &user, id).await?;
    let before = automation_notice::sms_template(pool, user.company_id, id).await;
    sqlx::query(
        "UPDATE cloudtalk_sms_templates SET deleted_at = NOW(), updated_at = NOW()
          WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(user.company_id)
    .execute(pool)
    .await?;
    cancel_scheduled(pool, id, "Template deleted").await?;
    automation_notice::notify(&state, &user, &before, &Snapshot::Template(None)).await;
    Ok(StatusCode::NO_CONTENT)
}
