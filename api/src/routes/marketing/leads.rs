//! The marketing team's leads: the list with how they did, and adding,
//! editing and deleting one.

use super::require_marketer;
use super::sources::source_named;
use crate::auth::CurrentUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::routes::customers::service::sync_emails;
use crate::serde_helpers::js_date;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use utoipa::{IntoParams, ToSchema};

/// SQL: the lead (`c`) came from Facebook.
const FACEBOOK: &str = "LOWER(c.referral_source) IN ('facebook', 'facebook-form', 'facebook form')";
/// SQL: the lead (`c`) came from the website.
const WEBSITE: &str = "(LOWER(c.referral_source) IN ('website', 'wordpress-form', 'webform') \
     OR LOWER(c.referral_source) LIKE 'website %')";
/// SQL: the lead (`c`) is marked invalid.
const INVALID: &str = "c.invalid_lead IS NOT NULL AND c.invalid_lead <> ''";

/// Query of `GET /v1/marketing/{company_id}/leads`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct LeadsQuery {
    /// `leads` (default; web and ad leads), `walkins` (check-ins) or `all`.
    pub view: Option<String>,
    /// `all` (default), `facebook` or `website`.
    pub channel: Option<String>,
    /// Created on or after this day, `YYYY-MM-DD`.
    pub from: Option<NaiveDate>,
    /// Created on or before this day, `YYYY-MM-DD`. Swapped with `from`
    /// when before it.
    pub to: Option<NaiveDate>,
}

/// A lead in the list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow, ToSchema)]
pub struct MarketingLead {
    /// Customer id.
    pub id: i32,
    /// Name.
    pub name: Option<String>,
    /// Primary email.
    pub email: Option<String>,
    /// Phone.
    pub phone: Option<String>,
    /// When the lead came in.
    #[serde(serialize_with = "js_date")]
    pub created_date: Option<DateTime<Utc>>,
    /// `leads` or `check-in`.
    pub source: Option<String>,
    /// Where it came from, e.g. `facebook`.
    pub referral_source: Option<String>,
    /// The web form it came through.
    pub form_name: Option<String>,
    /// Assigned sales rep.
    pub sales_rep: Option<i32>,
    /// Their name.
    pub sales_rep_name: Option<String>,
    /// Why it is invalid; null when valid.
    pub invalid_lead: Option<String>,
}

/// A live deal of a listed lead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow, ToSchema)]
pub struct MarketingDeal {
    /// Deal id.
    pub id: i64,
    /// The lead.
    pub customer_id: i32,
    /// Its stage.
    pub list_id: Option<i32>,
    /// Amount, decimal string.
    pub amount: Option<String>,
    /// Status text.
    pub status: Option<String>,
    /// Why it was lost.
    pub lost_reason: Option<String>,
    /// 1 won, 0 lost, null open.
    pub is_won: Option<i8>,
}

/// A deal stage of the company's displayed pipelines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow, ToSchema)]
pub struct MarketingStage {
    /// Stage id.
    pub id: i32,
    /// Name.
    pub name: Option<String>,
    /// Order in the board.
    pub position: Option<i32>,
}

/// Invalid leads by channel.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, FromRow, ToSchema)]
pub struct InvalidStats {
    /// From Facebook.
    pub facebook: i64,
    /// From the website.
    pub website: i64,
    /// All.
    pub total: i64,
}

/// Leads lost or marked invalid, by reason.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, FromRow, ToSchema)]
pub struct InvalidReasons {
    /// "Too expensive".
    pub too_expensive: i64,
    /// "Out of area".
    pub out_of_area: i64,
    /// "Never responded".
    pub never_responded: i64,
    /// "Wrong number, email, etc.".
    pub wrong_contact: i64,
    /// "Accident submission".
    pub accident_submission: i64,
    /// "Looking for unrelated service".
    pub unrelated_service: i64,
    /// "Bought somewhere else".
    pub bought_elsewhere: i64,
    /// "Stopped responding".
    pub stopped_responding: i64,
    /// "Gave up on the project".
    pub gave_up_on_the_project: i64,
}

/// The leads page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct LeadsPage {
    /// The range used, `YYYY-MM-DD`; null when open.
    pub from: Option<String>,
    /// The range used.
    pub to: Option<String>,
    /// Leads, newest first.
    pub leads: Vec<MarketingLead>,
    /// Their live deals created in the range, newest first.
    pub deals: Vec<MarketingDeal>,
    /// The company's stages.
    pub stages: Vec<MarketingStage>,
    /// Invalid leads by channel.
    pub invalid_stats: InvalidStats,
    /// Lost or invalid leads by reason.
    pub invalid_reasons: InvalidReasons,
}

/// `WHERE` of the leads of the query, `c` being `customers`, with its
/// binds (the dates, then the company).
fn lead_filter(
    query: &LeadsQuery,
    company_id: i32,
) -> (String, Vec<String>, Option<NaiveDate>, Option<NaiveDate>) {
    let (from, to) = match (query.from, query.to) {
        (Some(from), Some(to)) if from > to => (Some(to), Some(from)),
        other => other,
    };
    let source = match query.view.as_deref() {
        Some("walkins") => "c.source = 'check-in'",
        Some("all") => "(c.source = 'leads' OR c.source = 'check-in')",
        _ => "c.source = 'leads'",
    };
    let channel = match query.channel.as_deref().map(str::to_lowercase).as_deref() {
        Some("facebook") => format!(" AND {FACEBOOK}"),
        Some("website") => format!(" AND {WEBSITE}"),
        _ => String::new(),
    };
    let mut binds = Vec::new();
    let mut sql = format!("{source} AND c.deleted_at IS NULL");
    if let Some(from) = from {
        sql.push_str(" AND DATE({date}) >= ?");
        binds.push(from.to_string());
    }
    if let Some(to) = to {
        sql.push_str(" AND DATE({date}) <= ?");
        binds.push(to.to_string());
    }
    sql.push_str(" AND c.company_id = ?");
    binds.push(company_id.to_string());
    sql.push_str(&channel);
    (sql, binds, from, to)
}

fn bound<'q, O>(
    query: sqlx::query::QueryAs<'q, sqlx::MySql, O, sqlx::mysql::MySqlArguments>,
    binds: &'q [String],
) -> sqlx::query::QueryAs<'q, sqlx::MySql, O, sqlx::mysql::MySqlArguments> {
    binds.iter().fold(query, sqlx::query::QueryAs::bind)
}

/// List the leads
///
/// The company's leads (or walk-ins) created in the range, newest first,
/// with their live deals, the company's stages and invalid / lost counts.
#[utoipa::path(
    get,
    path = "/v1/marketing/{company_id}/leads",
    operation_id = "list_marketing_leads",
    tag = "marketing",
    params(("company_id" = i32, Path, description = "Company id"), LeadsQuery),
    responses(
        (status = 200, description = "The leads page", body = LeadsPage),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not on the company's marketing team", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(company_id): Path<i32>,
    Query(query): Query<LeadsQuery>,
) -> ApiResult<Json<LeadsPage>> {
    require_marketer(&state, &user, company_id).await?;
    let (filter, binds, from, to) = lead_filter(&query, company_id);
    let by_created = filter.replace("{date}", "c.created_date");
    let by_deal = filter.replace("{date}", "d.created_at");
    let pool = &state.pool;
    let leads: Vec<MarketingLead> = bound(
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT c.id, c.name, primary_email.email, c.phone, c.created_date, c.source,
                    c.referral_source, c.form_name, c.sales_rep, u.name AS sales_rep_name,
                    c.invalid_lead
               FROM customers c
               LEFT JOIN customers_emails primary_email
                 ON primary_email.id = c.email_id AND primary_email.customer_id = c.id
               LEFT JOIN users u ON c.sales_rep = u.id AND u.is_deleted = 0
                 AND u.company_id = c.company_id
              WHERE {by_created}
              ORDER BY c.created_date DESC, c.id DESC"
        ))),
        &binds,
    )
    .fetch_all(pool)
    .await?;
    let deals: Vec<MarketingDeal> = bound(
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT CAST(d.id AS SIGNED) AS id, d.customer_id, d.list_id,
                    CAST(d.amount AS CHAR) AS amount, d.status, d.lost_reason,
                    CAST(d.is_won AS SIGNED) AS is_won
               FROM deals d JOIN customers c ON d.customer_id = c.id
              WHERE {by_deal} AND d.deleted_at IS NULL
              ORDER BY d.created_at DESC, d.id DESC"
        ))),
        &binds,
    )
    .fetch_all(pool)
    .await?;
    let stages: Vec<MarketingStage> = sqlx::query_as(
        "SELECT dl.id, dl.name, dl.position FROM deals_list dl
           JOIN groups_list gl ON dl.group_id = gl.id
          WHERE dl.deleted_at IS NULL AND gl.deleted_at IS NULL AND gl.is_displayed = 1
            AND (gl.company_id = ? OR gl.id = 1)
          ORDER BY dl.position, dl.id",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    let invalid_stats: InvalidStats = bound(
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT CAST(COALESCE(SUM({FACEBOOK} AND {INVALID}), 0) AS SIGNED) AS facebook,
                    CAST(COALESCE(SUM({WEBSITE} AND {INVALID}), 0) AS SIGNED) AS website,
                    CAST(COALESCE(SUM({INVALID}), 0) AS SIGNED) AS total
               FROM customers c WHERE {by_created}"
        ))),
        &binds,
    )
    .fetch_one(pool)
    .await?;
    let reason = |names: &[&str]| {
        let list = names
            .iter()
            .map(|name| format!("'{}'", name.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "COUNT(DISTINCT CASE WHEN c.invalid_lead IN ({list}) OR d.lost_reason IN ({list}) THEN c.id END)"
        )
    };
    let invalid_reasons: InvalidReasons = bound(
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {} AS too_expensive, {} AS out_of_area, {} AS never_responded,
                    {} AS wrong_contact, {} AS accident_submission, {} AS unrelated_service,
                    {} AS bought_elsewhere, {} AS stopped_responding, {} AS gave_up_on_the_project
               FROM customers c
               LEFT JOIN deals d ON d.customer_id = c.id AND d.deleted_at IS NULL AND d.is_won = 0
              WHERE {by_created}",
            reason(&["Too expensive"]),
            reason(&["Out of area"]),
            reason(&["Never responded"]),
            reason(&["Wrong number, email, etc.", "Wrong number, email, etc"]),
            reason(&["Accident submission"]),
            reason(&["Looking for unrelated service"]),
            reason(&["Bought somewhere else"]),
            reason(&["Stopped responding", "Stoped responding"]),
            reason(&["Gave up on the project"]),
        ))),
        &binds,
    )
    .fetch_one(pool)
    .await?;
    Ok(Json(LeadsPage {
        from: from.map(|d| d.to_string()),
        to: to.map(|d| d.to_string()),
        leads,
        deals,
        stages,
        invalid_stats,
        invalid_reasons,
    }))
}

/// Body of `POST` and `PUT` on a lead.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct LeadInput {
    /// Name; required.
    pub name: String,
    /// Email addresses, the first one primary.
    #[serde(default)]
    pub emails: Vec<String>,
    /// Phone.
    #[serde(default)]
    pub phone: Option<String>,
    /// Second phone.
    #[serde(default)]
    pub phone_2: Option<String>,
    /// Address.
    #[serde(default)]
    pub address: Option<String>,
    /// What the customer wrote.
    #[serde(default)]
    pub your_message: Option<String>,
    /// Creating only: one of the company's referral sources; required.
    #[serde(default)]
    pub referral_source: Option<String>,
    /// Creating only: a picture uploaded with `POST /v1/marketing/{company_id}/uploads`.
    #[serde(default)]
    pub file_url: Option<String>,
}

fn text(value: Option<&String>) -> Option<&str> {
    value.map(|v| v.trim()).filter(|v| !v.is_empty())
}

/// Response of creating a lead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
pub struct CreatedLead {
    /// Customer id.
    pub id: i32,
}

/// Add a lead
///
/// Saves a lead (source `leads`, created by "Marketing team") with its
/// emails, then hands it to the lead pipeline, which opens its deal and
/// tells the sales team.
#[utoipa::path(
    post,
    path = "/v1/marketing/{company_id}/leads",
    operation_id = "create_marketing_lead",
    tag = "marketing",
    params(("company_id" = i32, Path, description = "Company id")),
    request_body = LeadInput,
    responses(
        (status = 201, description = "Added", body = CreatedLead),
        (status = 400, description = "No name, or an unknown referral source", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not on the company's marketing team", body = ErrorBody),
        (status = 409, description = "An email belongs to another customer", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(company_id): Path<i32>,
    JsonBody(input): JsonBody<LeadInput>,
) -> ApiResult<(StatusCode, Json<CreatedLead>)> {
    require_marketer(&state, &user, company_id).await?;
    let name = input.name.trim();
    if name.is_empty() {
        return Err(ApiError::BadRequest("Name is required".into()));
    }
    let referral = text(input.referral_source.as_ref()).unwrap_or_default();
    if referral.is_empty()
        || source_named(&state, company_id, referral, None)
            .await?
            .is_none()
    {
        return Err(ApiError::BadRequest(
            "Select a valid referral source.".into(),
        ));
    }
    let mut tx = state.pool.begin().await?;
    let id = sqlx::query(
        "INSERT INTO customers (name, phone, phone_2, address, your_message, referral_source,
            source, company_id, created_by)
         VALUES (?, ?, ?, ?, ?, ?, 'leads', ?, 'Marketing team')",
    )
    .bind(name)
    .bind(text(input.phone.as_ref()))
    .bind(text(input.phone_2.as_ref()))
    .bind(text(input.address.as_ref()))
    .bind(text(input.your_message.as_ref()))
    .bind(referral)
    .bind(company_id)
    .execute(&mut *tx)
    .await?
    .last_insert_id();
    let id = i32::try_from(id).unwrap_or(i32::MAX);
    sync_emails(&mut tx, id, &input.emails, user.id).await?;
    tx.commit().await?;
    let emails = crate::routes::customers::service::normalize_emails(&input.emails);
    let lead = serde_json::json!({
        "name": name,
        "email": emails.first(),
        "phone": text(input.phone.as_ref()),
        "your_message": text(input.your_message.as_ref()),
        "address": text(input.address.as_ref()),
        "source": "leads",
        "company_id": company_id,
        "referral_source": referral,
        "file": text(input.file_url.as_ref()).unwrap_or_default(),
    });
    state.webhooks.notify_new_lead(company_id, &lead).await;
    state.webhooks.sync_contacts(company_id, id).await;
    Ok((StatusCode::CREATED, Json(CreatedLead { id })))
}

/// A lead as its edit form shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow, ToSchema)]
pub struct EditableLead {
    /// Customer id.
    pub id: i32,
    /// Name.
    pub name: Option<String>,
    /// Phone.
    pub phone: Option<String>,
    /// Second phone.
    pub phone_2: Option<String>,
    /// Address.
    pub address: Option<String>,
    /// `leads` or `check-in`.
    pub source: Option<String>,
    /// What the customer wrote.
    pub your_message: Option<String>,
    /// Email addresses, primary first.
    #[sqlx(skip)]
    pub emails: Vec<String>,
}

/// Get a lead
///
/// A live lead of the company, for its edit form.
#[utoipa::path(
    get,
    path = "/v1/marketing/{company_id}/leads/{id}",
    operation_id = "get_marketing_lead",
    tag = "marketing",
    params(
        ("company_id" = i32, Path, description = "Company id"),
        ("id" = i32, Path, description = "Customer id"),
    ),
    responses(
        (status = 200, description = "The lead", body = EditableLead),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not on the company's marketing team", body = ErrorBody),
        (status = 404, description = "No such live lead in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((company_id, id)): Path<(i32, i32)>,
) -> ApiResult<Json<EditableLead>> {
    require_marketer(&state, &user, company_id).await?;
    let mut lead: EditableLead = sqlx::query_as(
        "SELECT id, name, phone, phone_2, address, source, your_message FROM customers
          WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(company_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or(ApiError::NotFound)?;
    lead.emails = sqlx::query_scalar(
        "SELECT ce.email FROM customers_emails ce JOIN customers c ON c.id = ce.customer_id
          WHERE ce.customer_id = ? ORDER BY ce.id = c.email_id DESC, ce.created_at, ce.id",
    )
    .bind(id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(lead))
}

/// Edit a lead
///
/// Saves the name, phones, address, message and emails of a live lead.
#[utoipa::path(
    put,
    path = "/v1/marketing/{company_id}/leads/{id}",
    operation_id = "update_marketing_lead",
    tag = "marketing",
    params(
        ("company_id" = i32, Path, description = "Company id"),
        ("id" = i32, Path, description = "Customer id"),
    ),
    request_body = LeadInput,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "No name", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not on the company's marketing team", body = ErrorBody),
        (status = 404, description = "No such live lead in the company", body = ErrorBody),
        (status = 409, description = "An email belongs to another customer", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((company_id, id)): Path<(i32, i32)>,
    JsonBody(input): JsonBody<LeadInput>,
) -> ApiResult<StatusCode> {
    require_marketer(&state, &user, company_id).await?;
    let name = input.name.trim();
    if name.is_empty() {
        return Err(ApiError::BadRequest("Name is required".into()));
    }
    let mut tx = state.pool.begin().await?;
    let updated = sqlx::query(
        "UPDATE customers SET name = ?, phone = ?, phone_2 = ?, address = ?, your_message = ?
          WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(name)
    .bind(text(input.phone.as_ref()))
    .bind(text(input.phone_2.as_ref()))
    .bind(text(input.address.as_ref()))
    .bind(text(input.your_message.as_ref()))
    .bind(id)
    .bind(company_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if updated == 0 {
        let live: Option<i32> = sqlx::query_scalar(
            "SELECT 1 FROM customers WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
        )
        .bind(id)
        .bind(company_id)
        .fetch_optional(&mut *tx)
        .await?;
        live.ok_or(ApiError::NotFound)?;
    }
    sync_emails(&mut tx, id, &input.emails, user.id).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Delete a lead
///
/// Deletes a live lead and its deals, and removes the Telegram messages
/// about it.
#[utoipa::path(
    delete,
    path = "/v1/marketing/{company_id}/leads/{id}",
    operation_id = "delete_marketing_lead",
    tag = "marketing",
    params(
        ("company_id" = i32, Path, description = "Company id"),
        ("id" = i32, Path, description = "Customer id"),
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not on the company's marketing team", body = ErrorBody),
        (status = 404, description = "No such live lead in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((company_id, id)): Path<(i32, i32)>,
) -> ApiResult<StatusCode> {
    require_marketer(&state, &user, company_id).await?;
    let mut tx = state.pool.begin().await?;
    sqlx::query(
        "UPDATE deals d JOIN customers c ON c.id = d.customer_id SET d.deleted_at = NOW()
          WHERE d.customer_id = ? AND c.company_id = ? AND c.deleted_at IS NULL
            AND d.deleted_at IS NULL",
    )
    .bind(id)
    .bind(company_id)
    .execute(&mut *tx)
    .await?;
    let deleted = sqlx::query(
        "UPDATE customers SET deleted_at = NOW() WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(company_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if deleted == 0 {
        return Err(ApiError::NotFound);
    }
    tx.commit().await?;
    state.webhooks.delete_lead_messages(company_id, id).await;
    Ok(StatusCode::NO_CONTENT)
}
