//! SMS flows: sequences of texts (and emails) sent to a customer over time.
//!
//! A company flow (admins) is everyone's; a personal one its author's.
//! Steps use a template the flow may reference (a company flow only company
//! templates) or their own text. Replaces the Remix
//! `api.{cloudtalk,ringcentral}.sms.flows*` routes (`smsFlows.server.ts`).

use crate::auth::EmployeeUser;
use crate::automation_notice::{self, Snapshot};
use crate::auth::session::SessionUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::routes::templates::handlers::template_body_error;
use crate::routes::templates::schemas::TemplateScope;
use crate::sql::push_in;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, NaiveDateTime, Utc};
use common::utils::email_send_window::{FollowupChannel, followup_next_send_at};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, MySql, MySqlPool, QueryBuilder, Transaction};
use std::collections::{HashMap, HashSet};
use utoipa::ToSchema;

/// The flow the AI writes for one customer; not listed with the others.
const AI_FLOW_NAME: &str = "Generated with AI";
const MAX_STEPS: usize = 20;
/// 400 days: AI holds may wait longer than a normal gap.
const MAX_DELAY_MINUTES: i32 = 400 * 24 * 60;
const MAX_TEXT: usize = 100_000;
const MAX_SMS_TEXT: usize = 1600;
const MAX_SUBJECT: usize = 300;

/// Variables a step may use; anything else would go out unfilled.
const VARIABLE_KEYS: [&str; 20] = [
    "user.first_name",
    "customer.first_name",
    "customer.inventory_page",
    "company.name",
    "user.email",
    "user.phone_number",
    "customer.name",
    "current_date",
    "company.hours_of_operation",
    "customer.address",
    "company.domain",
    "user.name",
    "company.address",
    "customer.inventory_link",
    "company.subdomain",
    "company.id",
    "user.last_name",
    "customer.last_name",
    "customer.email",
    "customer.phone",
];

/// How a step is delivered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum StepChannel {
    #[default]
    Sms,
    Email,
}

impl StepChannel {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Sms => "sms",
            Self::Email => "email",
        }
    }
}

/// One step of a flow.
#[derive(Debug, Serialize, ToSchema)]
pub struct FlowStep {
    /// The step.
    pub id: i32,
    /// 1 = first.
    pub position: i32,
    pub channel: StepChannel,
    /// The template it sends, if any.
    pub template_id: Option<i32>,
    /// The template's name (null when it is gone or not the company's).
    pub template_name: Option<String>,
    /// Its own text, when it has no template.
    pub custom_text: Option<String>,
    /// The template's text, for display.
    pub template_body: Option<String>,
    /// Email subject of the step.
    pub subject: Option<String>,
    /// The email template's subject, for display.
    pub template_subject: Option<String>,
    /// Minutes after the previous step (or the start).
    pub delay_minutes: i32,
}

/// A flow.
#[derive(Debug, Serialize, ToSchema)]
pub struct Flow {
    /// The flow.
    pub id: i32,
    /// Its name.
    pub name: String,
    pub scope: TemplateScope,
    /// The caller may change it.
    pub can_edit: bool,
    /// In order.
    pub steps: Vec<FlowStep>,
    /// When it was made.
    pub created_at: Option<DateTime<Utc>>,
    /// When it last changed.
    pub updated_at: Option<DateTime<Utc>>,
}

/// `GET /v1/sms-flows`.
#[derive(Debug, Serialize, ToSchema)]
pub struct Flows {
    /// Company flows first, newest first.
    pub flows: Vec<Flow>,
    /// The caller may create and change company flows.
    pub can_manage_company: bool,
}

/// A step as written in the flow editor.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct StepInput {
    #[serde(default)]
    pub channel: StepChannel,
    /// A template, or else `custom_text`.
    pub template_id: Option<i32>,
    /// The step's own text (variables allowed, none of the author's own).
    pub custom_text: Option<String>,
    /// Email steps: the subject.
    pub subject: Option<String>,
    /// Minutes after the previous step, up to 400 days.
    pub delay_minutes: i32,
}

/// Body of `POST /v1/sms-flows`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct NewFlow {
    /// Up to 255 characters.
    pub name: String,
    /// `company` (admins only) or `personal`.
    pub scope: TemplateScope,
    /// 1 to 20 steps.
    pub steps: Vec<StepInput>,
}

/// Body of `PUT /v1/sms-flows/{id}`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct FlowUpdate {
    /// Up to 255 characters.
    pub name: String,
    /// 1 to 20 steps; they replace the old ones.
    pub steps: Vec<StepInput>,
}

/// `POST /v1/sms-flows`.
#[derive(Debug, Serialize, ToSchema)]
pub struct CreatedFlow {
    /// The new flow.
    pub id: i32,
}

#[derive(Debug, FromRow)]
struct FlowRow {
    id: i32,
    user_id: Option<i32>,
    name: String,
    created_at: Option<NaiveDateTime>,
    updated_at: Option<NaiveDateTime>,
}

#[derive(Debug, FromRow)]
struct StepRow {
    id: i32,
    flow_id: i32,
    position: i32,
    channel: Option<String>,
    template_id: Option<i32>,
    template_name: Option<String>,
    custom_text: Option<String>,
    template_body: Option<String>,
    subject: Option<String>,
    template_subject: Option<String>,
    delay_minutes: i32,
}

const fn can_manage_company(user: &SessionUser) -> bool {
    user.is_admin || user.is_superuser
}

fn bad(token: &str) -> ApiError {
    ApiError::BadRequest(token.to_string())
}

/// Text between `{{` and `}}` without tags and entities (`cleanVariableName`).
pub fn variable_names(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else { break };
        let inner = &after[..end];
        if !inner.contains(['{', '}']) && !inner.is_empty() && inner.chars().count() <= 200 {
            let mut plain = String::new();
            let mut in_tag = false;
            for c in inner.chars() {
                match c {
                    '<' => in_tag = true,
                    '>' => in_tag = false,
                    c if !in_tag => plain.push(c),
                    _ => {}
                }
            }
            let plain = plain
                .replace("&nbsp;", " ")
                .replace("&amp;", "&")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            names.push(plain);
        }
        rest = &after[end + 2..];
    }
    names
}

/// Checks a step the way the editor's schema does; the error token.
fn check_step(step: &StepInput) -> Result<(), &'static str> {
    let text = step.custom_text.as_deref().map(str::trim);
    if text == Some("") {
        return Err("validation_failed");
    }
    if step.template_id.is_some() == text.is_some() {
        return Err("step_needs_template_or_text");
    }
    if step.template_id.is_some_and(|id| id <= 0)
        || !(0..=MAX_DELAY_MINUTES).contains(&step.delay_minutes)
        || step
            .subject
            .as_deref()
            .is_some_and(|s| s.trim().chars().count() > MAX_SUBJECT)
    {
        return Err("validation_failed");
    }
    if let Some(text) = text {
        if text.chars().count() > MAX_TEXT {
            return Err("validation_failed");
        }
        if template_body_error(text).is_some() {
            return Err("invalid_template_body");
        }
        if variable_names(text)
            .iter()
            .any(|name| !name.is_empty() && !VARIABLE_KEYS.contains(&name.as_str()))
        {
            return Err("template_has_custom_variables");
        }
        if step.channel == StepChannel::Sms && text.chars().count() > MAX_SMS_TEXT {
            return Err("sms_text_too_long");
        }
    }
    Ok(())
}

fn checked(name: &str, steps: &[StepInput]) -> ApiResult<String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 255 || steps.is_empty() || steps.len() > MAX_STEPS
    {
        return Err(bad("validation_failed"));
    }
    for step in steps {
        check_step(step).map_err(bad)?;
    }
    Ok(name.to_string())
}

pub async fn steps_by_flow(
    pool: &MySqlPool,
    company_id: i32,
    flow_ids: &[i32],
) -> Result<HashMap<i32, Vec<FlowStep>>, sqlx::Error> {
    let mut grouped: HashMap<i32, Vec<FlowStep>> = HashMap::new();
    if flow_ids.is_empty() {
        return Ok(grouped);
    }
    // The company guard turns a foreign template into a missing one.
    let mut sql = QueryBuilder::<MySql>::new(
        "SELECT s.id, s.flow_id, s.position, CAST(s.channel AS CHAR) AS channel, s.template_id,
                COALESCE(t.name, rt.name, et.template_name) AS template_name, s.custom_text,
                COALESCE(t.body, rt.body, et.template_body) AS template_body, s.subject,
                et.template_subject, s.delay_minutes
           FROM sms_flow_steps s
           LEFT JOIN cloudtalk_sms_templates t
             ON t.id = s.template_id AND t.company_id = ",
    );
    sql.push_bind(company_id)
        .push(
            " AND COALESCE(s.channel, 'sms') = 'sms'
           LEFT JOIN ringcentral_sms_templates rt ON rt.id = s.template_id AND rt.company_id = ",
        )
        .push_bind(company_id)
        .push(
            " AND COALESCE(s.channel, 'sms') = 'sms'
           LEFT JOIN email_templates et ON et.id = s.template_id AND et.company_id = ",
        )
        .push_bind(company_id)
        .push(
            " AND COALESCE(s.channel, 'sms') = 'email' AND et.deleted_at IS NULL
          WHERE 1 = 1",
        );
    push_in(&mut sql, "s.flow_id", flow_ids);
    sql.push(" ORDER BY s.flow_id, s.position");
    for row in sql.build_query_as::<StepRow>().fetch_all(pool).await? {
        grouped.entry(row.flow_id).or_default().push(FlowStep {
            id: row.id,
            position: row.position,
            channel: if row.channel.as_deref() == Some("email") {
                StepChannel::Email
            } else {
                StepChannel::Sms
            },
            template_id: row.template_id,
            template_name: row.template_name,
            custom_text: row.custom_text,
            template_body: row.template_body,
            subject: row.subject,
            template_subject: row.template_subject,
            delay_minutes: row.delay_minutes,
        });
    }
    Ok(grouped)
}

fn to_flow(row: FlowRow, steps: Vec<FlowStep>, user: &SessionUser) -> Flow {
    Flow {
        id: row.id,
        name: row.name,
        scope: if row.user_id.is_none() {
            TemplateScope::Company
        } else {
            TemplateScope::Personal
        },
        can_edit: row
            .user_id
            .map_or_else(|| can_manage_company(user), |owner| owner == user.id),
        steps,
        created_at: row.created_at.map(|at| at.and_utc()),
        updated_at: row.updated_at.map(|at| at.and_utc()),
    }
}

const FLOW_COLUMNS: &str = "id, user_id, name, created_at, updated_at";

/// List SMS flows
///
/// The company's flows and the caller's own (not the per-customer AI flows),
/// company flows first, newest first.
#[utoipa::path(
    get,
    path = "/v1/sms-flows",
    operation_id = "list_sms_flows",
    tag = "sms",
    responses(
        (status = 200, description = "Flows", body = Flows),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<Flows>> {
    let pool = &state.pool;
    let rows: Vec<FlowRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {FLOW_COLUMNS} FROM sms_flows
          WHERE company_id = ? AND deleted_at IS NULL AND (user_id IS NULL OR user_id = ?)
            AND name <> ?
          ORDER BY CASE WHEN user_id IS NULL THEN 0 ELSE 1 END, created_at DESC"
    )))
    .bind(user.company_id)
    .bind(user.id)
    .bind(AI_FLOW_NAME)
    .fetch_all(pool)
    .await?;
    let ids: Vec<i32> = rows.iter().map(|row| row.id).collect();
    let mut steps = steps_by_flow(pool, user.company_id, &ids).await?;
    Ok(Json(Flows {
        flows: rows
            .into_iter()
            .map(|row| {
                let steps = steps.remove(&row.id).unwrap_or_default();
                to_flow(row, steps, &user)
            })
            .collect(),
        can_manage_company: can_manage_company(&user),
    }))
}

/// Get an SMS flow
///
/// One of the company's flows or the caller's own, including an AI flow
/// the caller confirmed for a customer.
#[utoipa::path(
    get,
    path = "/v1/sms-flows/{id}",
    operation_id = "get_sms_flow",
    tag = "sms",
    params(("id" = i32, Path, description = "Flow id")),
    responses(
        (status = 200, description = "The flow", body = Flow),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such flow the caller may see", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<Flow>> {
    let pool = &state.pool;
    let row: FlowRow = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {FLOW_COLUMNS} FROM sms_flows
          WHERE id = ? AND company_id = ? AND deleted_at IS NULL AND (user_id IS NULL OR user_id = ?)"
    )))
    .bind(id)
    .bind(user.company_id)
    .bind(user.id)
    .fetch_optional(pool)
    .await?
    .ok_or(ApiError::NotFound)?;
    let steps = steps_by_flow(pool, user.company_id, &[id])
        .await?
        .remove(&id)
        .unwrap_or_default();
    Ok(Json(to_flow(row, steps, &user)))
}

/// Steps may use only templates the flow can: a company flow the company's,
/// a personal one also the author's own.
async fn check_templates(
    tx: &mut Transaction<'_, MySql>,
    user: &SessionUser,
    steps: &[StepInput],
    company_flow: bool,
) -> ApiResult<()> {
    for (channel, tables) in [
        (
            StepChannel::Sms,
            &["cloudtalk_sms_templates", "ringcentral_sms_templates"][..],
        ),
        (StepChannel::Email, &["email_templates"][..]),
    ] {
        let wanted: HashSet<i32> = steps
            .iter()
            .filter(|step| step.channel == channel)
            .filter_map(|step| step.template_id)
            .collect();
        if wanted.is_empty() {
            continue;
        }
        let mut found = HashSet::new();
        for table in tables {
            let mut sql =
                QueryBuilder::<MySql>::new(format!("SELECT id FROM {table} WHERE company_id = "));
            sql.push_bind(user.company_id)
                .push(" AND deleted_at IS NULL");
            if company_flow {
                sql.push(" AND user_id IS NULL");
            } else {
                sql.push(" AND (user_id IS NULL OR user_id = ")
                    .push_bind(user.id)
                    .push(")");
            }
            let ids: Vec<i32> = wanted.iter().copied().collect();
            push_in(&mut sql, "id", &ids);
            let rows: Vec<(i32,)> = sql.build_query_as().fetch_all(&mut **tx).await?;
            found.extend(rows.into_iter().map(|row| row.0));
        }
        if !wanted.is_subset(&found) {
            return Err(bad("template_not_found"));
        }
    }
    Ok(())
}

async fn insert_steps(
    tx: &mut Transaction<'_, MySql>,
    flow_id: i64,
    steps: &[StepInput],
) -> Result<(), sqlx::Error> {
    for (index, step) in steps.iter().enumerate() {
        sqlx::query(
            "INSERT INTO sms_flow_steps (flow_id, position, channel, template_id, custom_text, subject, delay_minutes)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(flow_id)
        .bind(i32::try_from(index + 1).unwrap_or(i32::MAX))
        .bind(step.channel.as_str())
        .bind(step.template_id)
        .bind(step.custom_text.as_deref().map(str::trim))
        .bind(if step.channel == StepChannel::Email {
            step.subject.as_deref().map(str::trim)
        } else {
            None
        })
        .bind(step.delay_minutes)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// Create an SMS flow
///
/// Errors come back as a token: `validation_failed`,
/// `step_needs_template_or_text`, `invalid_template_body`,
/// `template_has_custom_variables`, `sms_text_too_long`,
/// `template_not_found`.
#[utoipa::path(
    post,
    path = "/v1/sms-flows",
    operation_id = "create_sms_flow",
    tag = "sms",
    request_body = NewFlow,
    responses(
        (status = 201, description = "Created", body = CreatedFlow),
        (status = 400, description = "Invalid flow (see the token)", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Not an employee, or not an admin for a company flow", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<NewFlow>,
) -> ApiResult<(StatusCode, Json<CreatedFlow>)> {
    let name = checked(&input.name, &input.steps)?;
    let company = input.scope == TemplateScope::Company;
    if company && !can_manage_company(&user) {
        return Err(ApiError::Forbidden);
    }
    let mut tx = state.pool.begin().await?;
    check_templates(&mut tx, &user, &input.steps, company).await?;
    let id = sqlx::query(
        "INSERT INTO sms_flows (company_id, user_id, name, created_by) VALUES (?, ?, ?, ?)",
    )
    .bind(user.company_id)
    .bind((!company).then_some(user.id))
    .bind(&name)
    .bind(user.id)
    .execute(&mut *tx)
    .await?
    .last_insert_id();
    let id = i64::try_from(id).map_err(|err| sqlx::Error::Decode(Box::new(err)))?;
    insert_steps(&mut tx, id, &input.steps).await?;
    tx.commit().await?;
    let id = i32::try_from(id).unwrap_or_default();
    let after = automation_notice::flow(&state.pool, user.company_id, id).await;
    automation_notice::notify(&state, &user, &Snapshot::Flow(None), &after).await;
    Ok((StatusCode::CREATED, Json(CreatedFlow { id })))
}

/// Saves a personal flow of the caller's own text (no templates), e.g. an
/// AI follow-up the rep approved; its id.
pub async fn insert_personal_flow(
    pool: &MySqlPool,
    user: &SessionUser,
    name: &str,
    steps: &[StepInput],
) -> Result<i32, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let id = sqlx::query(
        "INSERT INTO sms_flows (company_id, user_id, name, created_by) VALUES (?, ?, ?, ?)",
    )
    .bind(user.company_id)
    .bind(user.id)
    .bind(name)
    .bind(user.id)
    .execute(&mut *tx)
    .await?
    .last_insert_id();
    let id = i64::try_from(id).map_err(|err| sqlx::Error::Decode(Box::new(err)))?;
    insert_steps(&mut tx, id, steps).await?;
    tx.commit().await?;
    Ok(i32::try_from(id).unwrap_or_default())
}

/// The flow, when the caller may change it; 403 when it is the company's
/// and they may not, 404 when there is none.
async fn editable(pool: &MySqlPool, user: &SessionUser, id: i32) -> ApiResult<FlowRow> {
    let row: FlowRow = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {FLOW_COLUMNS} FROM sms_flows WHERE id = ? AND company_id = ? AND deleted_at IS NULL"
    )))
    .bind(id)
    .bind(user.company_id)
    .fetch_optional(pool)
    .await?
    .ok_or(ApiError::NotFound)?;
    let allowed = row
        .user_id
        .map_or_else(|| can_manage_company(user), |owner| owner == user.id);
    if allowed {
        Ok(row)
    } else {
        Err(ApiError::Forbidden)
    }
}

#[derive(FromRow)]
struct ActiveEnrollment {
    id: i32,
    current_step_position: i32,
    anchor_at: NaiveDateTime,
}

/// Moves `next_send_at` of the flow's active enrollments to its new steps.
async fn reschedule(pool: &MySqlPool, company_id: i32, flow_id: i32) -> Result<(), sqlx::Error> {
    let enrollments: Vec<ActiveEnrollment> = sqlx::query_as(
        "SELECT id, current_step_position, anchor_at FROM sms_flow_enrollments
          WHERE company_id = ? AND flow_id = ? AND status = 'active'",
    )
    .bind(company_id)
    .bind(flow_id)
    .fetch_all(pool)
    .await?;
    if enrollments.is_empty() {
        return Ok(());
    }
    let steps = steps_by_flow(pool, company_id, &[flow_id])
        .await?
        .remove(&flow_id)
        .unwrap_or_default();
    for enrollment in enrollments {
        let Some(step) = steps
            .iter()
            .find(|step| step.position == enrollment.current_step_position)
        else {
            continue;
        };
        let channel = match step.channel {
            StepChannel::Sms => FollowupChannel::Sms,
            StepChannel::Email => FollowupChannel::Email,
        };
        let next = followup_next_send_at(
            enrollment.anchor_at.and_utc(),
            i64::from(step.delay_minutes),
            channel,
        );
        sqlx::query(
            "UPDATE sms_flow_enrollments SET next_send_at = ?, updated_at = UTC_TIMESTAMP()
              WHERE id = ? AND status = 'active'",
        )
        .bind(next.naive_utc())
        .bind(enrollment.id)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// Edit an SMS flow
///
/// Replaces the name and steps of the caller's flow (admins: a company
/// flow); customers already in it keep their place, on the new timing.
#[utoipa::path(
    put,
    path = "/v1/sms-flows/{id}",
    operation_id = "update_sms_flow",
    tag = "sms",
    params(("id" = i32, Path, description = "Flow id")),
    request_body = FlowUpdate,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Invalid flow (see the token)", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "The flow is not the caller's to change", body = ErrorBody),
        (status = 404, description = "No such flow", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<FlowUpdate>,
) -> ApiResult<StatusCode> {
    let pool = &state.pool;
    let existing = editable(pool, &user, id).await?;
    let name = checked(&input.name, &input.steps)?;
    let before = automation_notice::flow(pool, user.company_id, id).await;
    let mut tx = pool.begin().await?;
    // The flow's own scope decides, never the request.
    check_templates(&mut tx, &user, &input.steps, existing.user_id.is_none()).await?;
    sqlx::query(
        "UPDATE sms_flows SET name = ?, updated_at = UTC_TIMESTAMP()
          WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(&name)
    .bind(id)
    .bind(user.company_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM sms_flow_steps WHERE flow_id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    insert_steps(&mut tx, i64::from(id), &input.steps).await?;
    tx.commit().await?;
    reschedule(pool, user.company_id, id).await?;
    let after = automation_notice::flow(pool, user.company_id, id).await;
    automation_notice::notify(&state, &user, &before, &after).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Delete an SMS flow
///
/// Customers in it stop receiving it.
#[utoipa::path(
    delete,
    path = "/v1/sms-flows/{id}",
    operation_id = "delete_sms_flow",
    tag = "sms",
    params(("id" = i32, Path, description = "Flow id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "The flow is not the caller's to delete", body = ErrorBody),
        (status = 404, description = "No such flow", body = ErrorBody),
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
    editable(pool, &user, id).await?;
    let before = automation_notice::flow(pool, user.company_id, id).await;
    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE sms_flows SET deleted_at = UTC_TIMESTAMP(), updated_at = UTC_TIMESTAMP()
          WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(user.company_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE sms_flow_enrollments SET status = 'cancelled', next_send_at = NULL, updated_at = UTC_TIMESTAMP()
          WHERE flow_id = ? AND company_id = ? AND status IN ('active', 'paused')",
    )
    .bind(id)
    .bind(user.company_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    automation_notice::notify(&state, &user, &before, &Snapshot::Flow(None)).await;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(text: &str) -> StepInput {
        StepInput {
            channel: StepChannel::Sms,
            template_id: None,
            custom_text: Some(text.to_string()),
            subject: None,
            delay_minutes: 60,
        }
    }

    #[test]
    fn steps_are_checked_like_the_editor() {
        assert_eq!(check_step(&step("Hi {{customer.first_name}}")), Ok(()));
        assert_eq!(check_step(&step("Hi {{ <b>customer.name</b> }}")), Ok(()));
        assert_eq!(
            check_step(&step("{{customer.last_name}} {{customer.email}} {{customer.phone}} {{user.last_name}}")),
            Ok(())
        );
        assert_eq!(
            check_step(&step("Hi {{nickname}}")),
            Err("template_has_custom_variables")
        );
        // Flows run without a deal or quote, so these would go out unfilled.
        assert_eq!(
            check_step(&step("{{deal.name}}")),
            Err("template_has_custom_variables")
        );
        assert_eq!(
            check_step(&step("{{quote.total}}")),
            Err("template_has_custom_variables")
        );
        assert_eq!(check_step(&step("Hi {{name")), Err("invalid_template_body"));
        assert_eq!(
            check_step(&step(&"a".repeat(1601))),
            Err("sms_text_too_long")
        );
        let mut both = step("x");
        both.template_id = Some(3);
        assert_eq!(check_step(&both), Err("step_needs_template_or_text"));
        let mut late = step("x");
        late.delay_minutes = MAX_DELAY_MINUTES + 1;
        assert_eq!(check_step(&late), Err("validation_failed"));
    }
}
