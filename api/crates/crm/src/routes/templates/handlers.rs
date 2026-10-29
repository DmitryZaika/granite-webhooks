use super::order;
use super::schemas::{
    EmailTemplate, EmailTemplateInput, FillRequest, Filled, OrderPath, SaveOrder,
    TemplateAttachment, TemplateAttachmentInput, TemplateOrder, TemplateScope,
};
use crate::auth::EmployeeUser;
use crate::auth::session::SessionUser;
use crate::automation_notice::{self, Snapshot};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::routes::emails::sql::{normalize_address, normalized};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use common::crud::template::fetch_template_variable_data;
use common::utils::template::replace_template_variables;
use sqlx::{AssertSqlSafe, FromRow, MySqlPool};
use std::collections::HashMap;

#[derive(Debug, FromRow)]
struct TemplateRow {
    id: i32,
    user_id: Option<i32>,
    template_name: Option<String>,
    template_subject: Option<String>,
    template_body: Option<String>,
    created_at: Option<DateTime<Utc>>,
    lead_list_id: Option<i32>,
    hour_delay: Option<i32>,
    sales_rep_ids: Option<String>,
    list_name: Option<String>,
    group_name: Option<String>,
}

const TEMPLATE_COLUMNS: &str = "id, user_id, template_name, template_subject, template_body, \
     created_at, lead_list_id, hour_delay, CAST(sales_rep_ids AS CHAR) AS sales_rep_ids, \
     (SELECT dl.name FROM deals_list dl \
       WHERE dl.id = email_templates.lead_list_id AND dl.deleted_at IS NULL) AS list_name, \
     (SELECT gl.name FROM deals_list dl JOIN groups_list gl ON gl.id = dl.group_id AND gl.deleted_at IS NULL \
       WHERE dl.id = email_templates.lead_list_id AND dl.deleted_at IS NULL) AS group_name";

fn to_template(
    row: TemplateRow,
    user: &SessionUser,
    attachments: Vec<TemplateAttachment>,
) -> EmailTemplate {
    EmailTemplate {
        id: row.id,
        template_name: row.template_name.unwrap_or_default(),
        template_subject: row.template_subject.unwrap_or_default(),
        template_body: row.template_body.unwrap_or_default(),
        attachments,
        scope: if row.user_id.is_none() {
            TemplateScope::Company
        } else {
            TemplateScope::Personal
        },
        can_edit: row.user_id == Some(user.id),
        lead_list_id: row.lead_list_id,
        hour_delay: row.hour_delay,
        sales_rep_ids: super::reps::ids_of(row.sales_rep_ids.as_deref()),
        list_name: row.list_name,
        group_name: row.group_name,
        created_at: row.created_at,
    }
}

async fn attachments(
    pool: &MySqlPool,
    template_ids: &[i32],
) -> Result<HashMap<i32, Vec<TemplateAttachment>>, sqlx::Error> {
    if template_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let placeholders = vec!["?"; template_ids.len()].join(", ");
    let sql = format!(
        "SELECT template_id, id, filename, url, content_type, content_subtype
         FROM email_template_attachments
         WHERE template_id IN ({placeholders}) AND deleted_at IS NULL
         ORDER BY template_id, position, id"
    );
    let mut query =
        sqlx::query_as::<_, (i32, i32, String, String, String, Option<String>)>(AssertSqlSafe(sql));
    for id in template_ids {
        query = query.bind(id);
    }
    let mut grouped: HashMap<i32, Vec<TemplateAttachment>> = HashMap::new();
    for (template_id, id, filename, url, content_type, content_subtype) in
        query.fetch_all(pool).await?
    {
        grouped
            .entry(template_id)
            .or_default()
            .push(TemplateAttachment {
                id,
                filename,
                url,
                content_type,
                content_subtype: content_subtype.unwrap_or_default(),
            });
    }
    Ok(grouped)
}

/// A company template, or one of the caller's own.
async fn accessible(
    pool: &MySqlPool,
    user: &SessionUser,
    template_id: i32,
) -> Result<Option<TemplateRow>, sqlx::Error> {
    sqlx::query_as(AssertSqlSafe(format!(
        "SELECT {TEMPLATE_COLUMNS} FROM email_templates
         WHERE id = ? AND company_id = ? AND deleted_at IS NULL
           AND (user_id IS NULL OR user_id = ?)"
    )))
    .bind(template_id)
    .bind(user.company_id)
    .bind(user.id)
    .fetch_optional(pool)
    .await
}

/// One the caller may change: their own, or (admins) a company template.
async fn editable(
    pool: &MySqlPool,
    user: &SessionUser,
    template_id: i32,
) -> ApiResult<TemplateRow> {
    let admin = user.is_admin || user.is_superuser;
    match accessible(pool, user, template_id).await? {
        Some(row) if row.user_id == Some(user.id) || (row.user_id.is_none() && admin) => Ok(row),
        _ => Err(ApiError::NotFound),
    }
}

/// The checked automation of a company template: list, its group, the delay
/// and the sales reps whose deals get it (empty: every rep's).
struct Automation {
    list: i32,
    group: i32,
    hours: i32,
    reps: Vec<i32>,
}

/// `lead_list_id` and `hour_delay` of a company template: both or neither, a
/// live deal list the company sees, a (list, delay) pair no other template
/// uses for the same reps. Left-out reps are `kept_reps`.
async fn automation(
    pool: &MySqlPool,
    company_id: i32,
    input: &EmailTemplateInput,
    company: bool,
    exclude: Option<i32>,
    kept_reps: &[i32],
) -> ApiResult<Option<Automation>> {
    let bad = |message: &str| Err(ApiError::BadRequest(message.to_string()));
    let Some(list) = input.lead_list_id else {
        return match input.hour_delay {
            Some(hours) if !(0..=8760).contains(&hours) => {
                bad("Delay must be between 0 and 8760 hours.")
            }
            _ => Ok(None),
        };
    };
    if !company {
        return bad("Only company templates send automatically.");
    }
    let Some(hours) = input.hour_delay else {
        return bad("Set how many hours to wait before sending.");
    };
    if !(0..=8760).contains(&hours) {
        return bad("Delay must be between 0 and 8760 hours.");
    }
    let group: Option<i32> = sqlx::query_scalar(
        "SELECT dl.group_id FROM deals_list dl
           JOIN groups_list gl ON gl.id = dl.group_id AND gl.deleted_at IS NULL
          WHERE dl.id = ? AND dl.deleted_at IS NULL AND (gl.company_id = ? OR gl.id = 1)",
    )
    .bind(list)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    let Some(group) = group else {
        return bad("Select a valid deal list.");
    };
    let reps = match &input.sales_rep_ids {
        Some(ids) => super::reps::checked(pool, company_id, ids).await?,
        None => kept_reps.to_vec(),
    };
    let others: Vec<Option<String>> = sqlx::query_scalar(
        "SELECT CAST(sales_rep_ids AS CHAR) FROM email_templates
          WHERE company_id = ? AND user_id IS NULL AND lead_list_id = ? AND hour_delay = ?
            AND deleted_at IS NULL AND id <> ?",
    )
    .bind(company_id)
    .bind(list)
    .bind(hours)
    .bind(exclude.unwrap_or(0))
    .fetch_all(pool)
    .await?;
    if others
        .iter()
        .any(|other| super::reps::overlap(&reps, &super::reps::ids_of(other.as_deref())))
    {
        return Err(ApiError::Conflict(
            "Another email template already uses this list and delay for the same sales reps."
                .into(),
        ));
    }
    Ok(Some(Automation {
        list,
        group,
        hours,
        reps,
    }))
}

/// Pending automatic sends of a template are called off.
async fn cancel_pending(
    pool: &MySqlPool,
    template_id: i32,
    reason: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE scheduled_emails SET status = 'cancelled', error_message = ?
          WHERE template_id = ? AND status = 'pending'",
    )
    .bind(reason)
    .bind(template_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// `{{` and `}}` pair up, none is empty and none nests (`validateTemplateBody`).
pub fn template_body_error(body: &str) -> Option<&'static str> {
    let mut text = String::with_capacity(body.len());
    let mut in_tag = false;
    for c in body.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => text.push(c),
            _ => {}
        }
    }
    if text.matches("{{").count() != text.matches("}}").count() {
        return Some("Invalid template body format. Check for unclosed {{ or }}.");
    }
    let mut rest = text.as_str();
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            break;
        };
        let inner = &after[..end];
        if inner.trim().is_empty() || inner.contains('{') {
            return Some("Invalid template body format. Check for unclosed {{ or }}.");
        }
        rest = &after[end + 2..];
    }
    None
}

struct CleanInput {
    name: String,
    subject: String,
    body: String,
}

fn clean(input: &EmailTemplateInput) -> ApiResult<CleanInput> {
    let name = input.template_name.trim();
    let subject = input.template_subject.trim();
    let body = input.template_body.trim();
    let bad = |message: &str| Err(ApiError::BadRequest(message.to_string()));
    if name.is_empty() {
        return bad("Template name is required");
    }
    if subject.is_empty() {
        return bad("Template subject is required");
    }
    if body.is_empty() {
        return bad("Template body is required");
    }
    if name.chars().count() > 255 || subject.chars().count() > 255 {
        return bad("Template name and subject are at most 255 characters");
    }
    if let Some(error) = template_body_error(body) {
        return bad(error);
    }
    Ok(CleanInput {
        name: name.to_string(),
        subject: subject.to_string(),
        body: body.to_string(),
    })
}

/// List email templates
///
/// The company's templates and the caller's own, sorted by the caller's
/// picker order (most used first unless they saved a custom order; then
/// company templates first, newest first), with their attachments.
#[crate::openapi::path(
    get,
    path = "/v1/email-templates",
    operation_id = "list_email_templates",
    tag = "templates",
    responses(
        (status = 200, description = "Templates", body = Vec<EmailTemplate>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<Vec<EmailTemplate>>> {
    let pool = &state.pool;
    let mut rows: Vec<TemplateRow> = sqlx::query_as(AssertSqlSafe(format!(
        "SELECT {TEMPLATE_COLUMNS} FROM email_templates
         WHERE company_id = ? AND deleted_at IS NULL AND (user_id IS NULL OR user_id = ?)"
    )))
    .bind(user.company_id)
    .bind(user.id)
    .fetch_all(pool)
    .await?;
    let context = order::context(pool, user.id, super::schemas::OrderingType::Email).await?;
    order::sort(
        &mut rows,
        |row| row.id,
        &context,
        |a, b| {
            a.user_id
                .is_some()
                .cmp(&b.user_id.is_some())
                .then_with(|| b.created_at.cmp(&a.created_at))
                .then_with(|| b.id.cmp(&a.id))
        },
    );
    let ids: Vec<i32> = rows.iter().map(|row| row.id).collect();
    let mut files = attachments(pool, &ids).await?;
    Ok(Json(
        rows.into_iter()
            .map(|row| {
                let attachments = files.remove(&row.id).unwrap_or_default();
                to_template(row, &user, attachments)
            })
            .collect(),
    ))
}

/// Create an email template
///
/// `scope: company` (admins only) makes it the whole company's; otherwise
/// it is the caller's own. A company template may send automatically:
/// `lead_list_id` + `hour_delay` schedule it for deals entering that list.
#[crate::openapi::path(
    post,
    path = "/v1/email-templates",
    operation_id = "create_email_template",
    tag = "templates",
    request_body = EmailTemplateInput,
    responses(
        (status = 201, description = "Created", body = EmailTemplate),
        (status = 400, description = "Missing name, subject or body, unpaired braces, or a bad automation", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee, or not an admin for a company template", body = ErrorBody),
        (status = 409, description = "Another company template sends on this list after this delay", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<EmailTemplateInput>,
) -> ApiResult<(StatusCode, Json<EmailTemplate>)> {
    let clean = clean(&input)?;
    let company = input.scope == Some(TemplateScope::Company);
    if company && !(user.is_admin || user.is_superuser) {
        return Err(ApiError::Forbidden);
    }
    let automation = automation(&state.pool, user.company_id, &input, company, None, &[]).await?;
    let id = sqlx::query(
        "INSERT INTO email_templates
           (template_name, template_subject, template_body, company_id, user_id, created_by,
            lead_group_id, lead_list_id, hour_delay, sales_rep_ids)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&clean.name)
    .bind(&clean.subject)
    .bind(&clean.body)
    .bind(user.company_id)
    .bind((!company).then_some(user.id))
    .bind(user.id)
    .bind(automation.as_ref().map(|a| a.group))
    .bind(automation.as_ref().map(|a| a.list))
    .bind(automation.as_ref().map(|a| a.hours))
    .bind(
        automation
            .as_ref()
            .and_then(|a| super::reps::to_json(&a.reps)),
    )
    .execute(&state.pool)
    .await?
    .last_insert_id();
    let id = i32::try_from(id).unwrap_or(i32::MAX);
    let row = accessible(&state.pool, &user, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let after = automation_notice::email_template(&state.pool, user.company_id, id).await;
    automation_notice::notify(&state, &user, &Snapshot::Template(None), &after).await;
    Ok((
        StatusCode::CREATED,
        Json(to_template(row, &user, Vec::new())),
    ))
}

/// Edit an email template
///
/// The caller's own templates, and (admins) the company's, whose automation
/// (`lead_list_id`, `hour_delay`) it sets too; removing the automation calls
/// off the template's pending sends.
#[crate::openapi::path(
    put,
    path = "/v1/email-templates/{id}",
    operation_id = "update_email_template",
    tag = "templates",
    params(("id" = i32, Path, description = "Template id")),
    request_body = EmailTemplateInput,
    responses(
        (status = 200, description = "Updated", body = EmailTemplate),
        (status = 400, description = "Missing name, subject or body, unpaired braces, or a bad automation", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 404, description = "No such template the caller may edit", body = ErrorBody),
        (status = 409, description = "Another company template sends on this list after this delay", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<EmailTemplateInput>,
) -> ApiResult<Json<EmailTemplate>> {
    let clean = clean(&input)?;
    let existing = editable(&state.pool, &user, id).await?;
    let company = existing.user_id.is_none();
    let kept_reps = super::reps::ids_of(existing.sales_rep_ids.as_deref());
    let automation = automation(
        &state.pool,
        user.company_id,
        &input,
        company,
        Some(id),
        &kept_reps,
    )
    .await?;
    let before = automation_notice::email_template(&state.pool, user.company_id, id).await;
    sqlx::query(
        "UPDATE email_templates SET template_name = ?, template_subject = ?, template_body = ?,
                lead_group_id = ?, lead_list_id = ?, hour_delay = ?, sales_rep_ids = ?
         WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(&clean.name)
    .bind(&clean.subject)
    .bind(&clean.body)
    .bind(automation.as_ref().map(|a| a.group))
    .bind(automation.as_ref().map(|a| a.list))
    .bind(automation.as_ref().map(|a| a.hours))
    .bind(
        automation
            .as_ref()
            .and_then(|a| super::reps::to_json(&a.reps)),
    )
    .bind(id)
    .bind(user.company_id)
    .execute(&state.pool)
    .await?;
    match &automation {
        None => cancel_pending(&state.pool, id, "Template deleted").await?,
        Some(automation) => {
            super::reps::cancel_for_other_reps(
                &state.pool,
                super::reps::Queue::Emails,
                id,
                &automation.reps,
            )
            .await?;
        }
    }
    let after = automation_notice::email_template(&state.pool, user.company_id, id).await;
    automation_notice::notify(&state, &user, &before, &after).await;
    let row = editable(&state.pool, &user, id).await?;
    let attachments = attachments(&state.pool, &[id])
        .await?
        .remove(&id)
        .unwrap_or_default();
    Ok(Json(to_template(row, &user, attachments)))
}

/// Delete an email template
///
/// Soft-deletes one of the caller's own templates or (admins) a company
/// template, with its attachments; its pending automatic sends are called off.
#[crate::openapi::path(
    delete,
    path = "/v1/email-templates/{id}",
    operation_id = "delete_email_template",
    tag = "templates",
    params(("id" = i32, Path, description = "Template id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 404, description = "No such template the caller may delete", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    editable(&state.pool, &user, id).await?;
    let before = automation_notice::email_template(&state.pool, user.company_id, id).await;
    sqlx::query(
        // The automation goes too.
        "UPDATE email_templates
            SET deleted_at = NOW(), lead_group_id = NULL, lead_list_id = NULL, hour_delay = NULL,
                sales_rep_ids = NULL
          WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(user.company_id)
    .execute(&state.pool)
    .await?;
    cancel_pending(&state.pool, id, "Template deleted").await?;
    sqlx::query(
        "UPDATE email_template_attachments SET deleted_at = NOW()
          WHERE template_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .execute(&state.pool)
    .await?;
    automation_notice::notify(&state, &user, &before, &Snapshot::Template(None)).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Attach a file to an email template
///
/// Adds a file uploaded to the `email-templates` folder (`POST /v1/uploads`)
/// after the template's other attachments. Emails made from the template
/// carry it.
#[crate::openapi::path(
    post,
    path = "/v1/email-templates/{id}/attachments",
    operation_id = "add_email_template_attachment",
    tag = "templates",
    params(("id" = i32, Path, description = "Template id")),
    request_body = TemplateAttachmentInput,
    responses(
        (status = 201, description = "Attached", body = TemplateAttachment),
        (status = 400, description = "Not an uploaded template file, or no file name or type", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 404, description = "No such template the caller may edit", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn add_attachment(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<TemplateAttachmentInput>,
) -> ApiResult<(StatusCode, Json<TemplateAttachment>)> {
    editable(&state.pool, &user, id).await?;
    let in_folder = crate::routes::uploads::accepted_key(
        &state.storage,
        &input.url,
        crate::routes::uploads::schemas::UploadFolder::EmailTemplates,
        user.company_id,
    )
    .is_some();
    let filename = input.filename.trim();
    let (kind, subtype) = input
        .content_type
        .trim()
        .to_lowercase()
        .split_once('/')
        .map(|(kind, subtype)| (kind.to_string(), subtype.to_string()))
        .unwrap_or_default();
    if !in_folder || filename.is_empty() || kind.is_empty() {
        return Err(ApiError::BadRequest(
            "Attach a file uploaded to the email-templates folder, with its name and type".into(),
        ));
    }
    let position: i64 = sqlx::query_scalar(
        "SELECT CAST(COALESCE(MAX(position) + 1, 0) AS SIGNED) FROM email_template_attachments
          WHERE template_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .fetch_one(&state.pool)
    .await?;
    let attachment_id = sqlx::query(
        "INSERT INTO email_template_attachments
           (template_id, content_type, content_subtype, filename, url, position)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(&kind)
    .bind(&subtype)
    .bind(filename)
    .bind(&input.url)
    .bind(position)
    .execute(&state.pool)
    .await?
    .last_insert_id();
    Ok((
        StatusCode::CREATED,
        Json(TemplateAttachment {
            id: i32::try_from(attachment_id).unwrap_or(i32::MAX),
            filename: filename.to_string(),
            url: input.url,
            content_type: kind,
            content_subtype: subtype,
        }),
    ))
}

/// Remove a file from an email template
///
/// Emails made from the template no longer carry it.
#[crate::openapi::path(
    delete,
    path = "/v1/email-templates/{id}/attachments/{attachment_id}",
    operation_id = "delete_email_template_attachment",
    tag = "templates",
    params(
        ("id" = i32, Path, description = "Template id"),
        ("attachment_id" = i32, Path, description = "Attachment id"),
    ),
    responses(
        (status = 204, description = "Removed"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 404, description = "No such template the caller may edit, or no such attachment on it", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_attachment(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((id, attachment_id)): Path<(i32, i32)>,
) -> ApiResult<StatusCode> {
    editable(&state.pool, &user, id).await?;
    let removed = sqlx::query(
        "UPDATE email_template_attachments SET deleted_at = NOW()
          WHERE id = ? AND template_id = ? AND deleted_at IS NULL",
    )
    .bind(attachment_id)
    .bind(id)
    .execute(&state.pool)
    .await?
    .rows_affected();
    if removed == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Get a template picker's order
///
/// Whether the caller's picker of this type sorts automatically (most used
/// first) or in their saved order.
#[crate::openapi::path(
    get,
    path = "/v1/me/template-order/{type}",
    operation_id = "get_template_order",
    tag = "templates",
    params(OrderPath),
    responses(
        (status = 200, description = "Mode", body = TemplateOrder),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get_order(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(path): Path<OrderPath>,
) -> ApiResult<Json<TemplateOrder>> {
    let mode = order::mode(&state.pool, user.id, path.r#type).await?;
    Ok(Json(TemplateOrder { mode }))
}

/// Save a template picker's order
///
/// Saves the dragged order (every template the picker shows, first to
/// last) and switches the picker to it.
#[crate::openapi::path(
    put,
    path = "/v1/me/template-order/{type}",
    operation_id = "save_template_order",
    tag = "templates",
    params(OrderPath),
    request_body = SaveOrder,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "No ids, or an id that is not positive", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn save_order(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(path): Path<OrderPath>,
    JsonBody(body): JsonBody<SaveOrder>,
) -> ApiResult<StatusCode> {
    if body.template_ids.is_empty() || body.template_ids.iter().any(|id| *id <= 0) {
        return Err(ApiError::BadRequest("invalid template ids".to_string()));
    }
    order::save_custom(&state.pool, user.id, path.r#type, &body.template_ids).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Reset a template picker's order
///
/// Forgets the saved order: the picker sorts by use again.
#[crate::openapi::path(
    delete,
    path = "/v1/me/template-order/{type}",
    operation_id = "reset_template_order",
    tag = "templates",
    params(OrderPath),
    responses(
        (status = 204, description = "Reset"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn reset_order(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(path): Path<OrderPath>,
) -> ApiResult<StatusCode> {
    order::reset(&state.pool, user.id, path.r#type).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Fill template variables
///
/// Fills `{{user.*}}`, `{{customer.*}}`, `{{company.*}}`, `{{deal.*}}` and
/// `{{current_date}}` for the caller, the deal and the customer (given, or
/// found by `recipient_email`). Keys match case-insensitively:
/// `user.name`, `user.first_name`, `user.last_name`, `user.email`,
/// `user.phone_number`; `customer.name`, `customer.first_name`,
/// `customer.last_name`, `customer.email`, `customer.phone`,
/// `customer.address`, `customer.inventory_link`; `company.name`,
/// `company.address`, `company.hours_of_operation`, `company.domain`,
/// `company.subdomain`, `company.id`; `deal.name` and `deal.amount`
/// (e.g. `$4,500.00`, only with `deal_id`). `{{quote.*}}` are filled only
/// when a quote is emailed (`POST /v1/quotes/{id}/emails`). Variables
/// without a value, and unknown ones, are left as they are.
#[crate::openapi::path(
    post,
    path = "/v1/templates/fill",
    operation_id = "fill_template",
    tag = "templates",
    request_body = FillRequest,
    responses(
        (status = 200, description = "Filled text", body = Filled),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn fill(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(request): JsonBody<FillRequest>,
) -> ApiResult<Json<Filled>> {
    if !request.template.contains("{{") {
        return Ok(Json(Filled {
            text: request.template,
        }));
    }
    let pool = &state.pool;
    let mut customer_id = request.customer_id.filter(|id| *id > 0);
    if customer_id.is_none()
        && let Some(address) = request.recipient_email.as_deref()
    {
        let address = normalize_address(Some(address));
        if !address.is_empty() {
            customer_id = sqlx::query_scalar(AssertSqlSafe(format!(
                "SELECT c.id FROM customers c
                 JOIN customers_emails ce ON ce.customer_id = c.id
                 WHERE c.company_id = ? AND c.deleted_at IS NULL AND {} = ?
                 LIMIT 1",
                normalized("ce.email")
            )))
            .bind(user.company_id)
            .bind(address)
            .fetch_optional(pool)
            .await?;
        }
    }
    let data = fetch_template_variable_data(
        pool,
        user.id,
        request.deal_id.filter(|id| *id > 0),
        customer_id,
        user.company_id,
    )
    .await?;
    Ok(Json(Filled {
        text: replace_template_variables(&request.template, &data),
    }))
}

#[cfg(test)]
mod tests {
    use super::template_body_error;

    #[test]
    fn template_bodies() {
        assert!(template_body_error("<p>Hi {{customer.first_name}}</p>").is_none());
        assert!(template_body_error("Hi {{customer.first_name}").is_some());
        assert!(template_body_error("Hi {{ }}").is_some());
        assert!(template_body_error("Hi {{a{b}}").is_some());
        assert!(template_body_error("No variables").is_none());
    }
}
