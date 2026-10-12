//! A lead's card and projects for the marketing team, and their picture
//! uploads.

use super::require_marketer;
use crate::auth::CurrentUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::extract::MultiQuery;
use crate::routes::customers::history::{CustomerHistory, lead_history};
use crate::routes::customers::queries::{find_customer, search_customers};
use crate::routes::customers::schemas::{CustomerDetail, CustomerSearchQuery, CustomerSearchRow};
use crate::routes::uploads::handlers::extension;
use crate::routes::uploads::schemas::{Upload, UploadFolder};
use crate::serde_helpers::js_date;
use crate::state::AppState;
use crate::thumbnails::IMAGE_CONTENT_TYPES;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use std::collections::HashMap;
use utoipa::ToSchema;

/// A lead's card.
#[derive(Debug, Serialize, ToSchema)]
pub struct LeadCard {
    /// Contact details and the web form's answers.
    pub customer: CustomerDetail,
    /// Deals, reassignments and when it reached a sales rep (no emails,
    /// activities or notes).
    pub history: CustomerHistory,
    /// The lead is a business or has projects (show the projects tab).
    pub has_projects: bool,
}

/// A lead's card
///
/// A live lead of the company with its deals and reassignments.
#[utoipa::path(
    get,
    path = "/v1/marketing/{company_id}/leads/{id}/info",
    operation_id = "get_marketing_lead_card",
    tag = "marketing",
    params(
        ("company_id" = i32, Path, description = "Company id"),
        ("id" = i32, Path, description = "Customer id"),
    ),
    responses(
        (status = 200, description = "The card", body = LeadCard),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not on the company's marketing team", body = ErrorBody),
        (status = 404, description = "No such live lead in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn info(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((company_id, id)): Path<(i32, i32)>,
) -> ApiResult<Json<LeadCard>> {
    require_marketer(&state, &user, company_id).await?;
    let pool = &state.pool;
    let customer = find_customer(pool, company_id, id, false)
        .await?
        .ok_or(ApiError::NotFound)?;
    let history = lead_history(pool, company_id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let child: Option<i32> = sqlx::query_scalar(
        "SELECT id FROM customers WHERE parent_id = ? AND company_id = ? AND deleted_at IS NULL LIMIT 1",
    )
    .bind(id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    let has_projects = child.is_some()
        || customer
            .company_name
            .as_deref()
            .is_some_and(|name| !name.trim().is_empty());
    Ok(Json(LeadCard {
        customer,
        history,
        has_projects,
    }))
}

/// A sale of a project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow, ToSchema)]
pub struct ProjectSale {
    /// Sale id.
    pub id: i32,
    /// Total price, decimal string.
    pub price: Option<String>,
    /// Job address.
    pub project_address: Option<String>,
    /// When it was sold.
    #[serde(serialize_with = "js_date")]
    pub sale_date: Option<DateTime<Utc>>,
    #[serde(skip)]
    customer_id: i32,
}

/// A project (the lead's business or one of its jobs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow, ToSchema)]
pub struct LeadProject {
    /// Customer id.
    pub id: i32,
    /// Name.
    pub name: Option<String>,
    /// Primary email.
    pub email: Option<String>,
    /// Phone.
    pub phone: Option<String>,
    /// Address.
    pub address: Option<String>,
    /// Source.
    pub source: Option<String>,
    /// City.
    pub city: Option<String>,
    /// State.
    pub state: Option<String>,
    /// ZIP code.
    pub postal_code: Option<String>,
    /// Business name.
    pub company_name: Option<String>,
    /// Notes.
    pub notes: Option<String>,
    /// Assigned sales rep's name.
    pub sales_rep_name: Option<String>,
    /// Stage of its newest live deal.
    pub deal_stage: Option<String>,
    /// Every email address, primary first.
    #[sqlx(skip)]
    pub emails: Vec<String>,
    /// Its sales.
    #[sqlx(skip)]
    pub sales: Vec<ProjectSale>,
}

/// A lead's projects
///
/// The lead's business (its parent, or itself) and every project under
/// it, by name, with their sales.
#[utoipa::path(
    get,
    path = "/v1/marketing/{company_id}/leads/{id}/projects",
    operation_id = "list_marketing_lead_projects",
    tag = "marketing",
    params(
        ("company_id" = i32, Path, description = "Company id"),
        ("id" = i32, Path, description = "Customer id"),
    ),
    responses(
        (status = 200, description = "Projects by name", body = Vec<LeadProject>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not on the company's marketing team", body = ErrorBody),
        (status = 404, description = "No such live lead in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn projects(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((company_id, id)): Path<(i32, i32)>,
) -> ApiResult<Json<Vec<LeadProject>>> {
    require_marketer(&state, &user, company_id).await?;
    let pool = &state.pool;
    let parent: Option<Option<i32>> = sqlx::query_scalar(
        "SELECT parent_id FROM customers WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    let root = parent.ok_or(ApiError::NotFound)?.unwrap_or(id);
    let mut projects: Vec<LeadProject> = sqlx::query_as(
        "SELECT c.id, c.name, primary_email.email, c.phone, c.address, c.source, c.city, c.state,
                c.postal_code, c.company_name, c.notes, u.name AS sales_rep_name,
                (SELECT dl.name FROM deals d JOIN deals_list dl ON d.list_id = dl.id
                  WHERE d.customer_id = c.id AND d.deleted_at IS NULL
                  ORDER BY d.id DESC LIMIT 1) AS deal_stage
           FROM customers c
           LEFT JOIN customers_emails primary_email
             ON primary_email.id = c.email_id AND primary_email.customer_id = c.id
           LEFT JOIN users u ON c.sales_rep = u.id
          WHERE (c.parent_id = ? OR c.id = ?) AND c.company_id = ? AND c.deleted_at IS NULL
          ORDER BY c.name ASC, c.id ASC",
    )
    .bind(root)
    .bind(root)
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    if projects.is_empty() {
        return Ok(Json(projects));
    }
    let marks = vec!["?"; projects.len()].join(", ");
    let mut emails_query = sqlx::query_as::<_, (i32, String)>(sqlx::AssertSqlSafe(format!(
        "SELECT ce.customer_id, ce.email FROM customers_emails ce
           JOIN customers c ON c.id = ce.customer_id
          WHERE ce.customer_id IN ({marks})
          ORDER BY ce.customer_id, ce.id = c.email_id DESC, ce.created_at, ce.id"
    )));
    let mut sales_query = sqlx::query_as::<_, ProjectSale>(sqlx::AssertSqlSafe(format!(
        "SELECT id, CAST(price AS CHAR) AS price, project_address, sale_date, customer_id
           FROM sales WHERE customer_id IN ({marks}) ORDER BY sale_date, id"
    )));
    for project in &projects {
        emails_query = emails_query.bind(project.id);
        sales_query = sales_query.bind(project.id);
    }
    let mut emails: HashMap<i32, Vec<String>> = HashMap::new();
    for (customer_id, email) in emails_query.fetch_all(pool).await? {
        emails.entry(customer_id).or_default().push(email);
    }
    let mut sales: HashMap<i32, Vec<ProjectSale>> = HashMap::new();
    for sale in sales_query.fetch_all(pool).await? {
        sales.entry(sale.customer_id).or_default().push(sale);
    }
    for project in &mut projects {
        project.emails = emails.remove(&project.id).unwrap_or_default();
        project.sales = sales.remove(&project.id).unwrap_or_default();
    }
    Ok(Json(projects))
}

/// Body of `POST /v1/marketing/{company_id}/uploads`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct LeadUploadRequest {
    /// Original file name; only its extension is kept.
    pub file_name: String,
    /// An image type (`image/jpeg`, `image/png`...): the `Content-Type` of the `PUT`.
    pub content_type: String,
}

/// Upload a lead picture
///
/// A presigned `PUT` URL for a picture to attach to a new lead, and the
/// public URL to send as its `file_url`.
#[utoipa::path(
    post,
    path = "/v1/marketing/{company_id}/uploads",
    operation_id = "create_marketing_upload",
    tag = "marketing",
    params(("company_id" = i32, Path, description = "Company id")),
    request_body = LeadUploadRequest,
    responses(
        (status = 201, description = "Upload URL and public URL", body = Upload),
        (status = 400, description = "Not an image", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not on the company's marketing team", body = ErrorBody),
        (status = 500, description = "Storage error", body = ErrorBody),
        (status = 503, description = "Uploads are not configured on this server", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn upload(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(company_id): Path<i32>,
    JsonBody(request): JsonBody<LeadUploadRequest>,
) -> ApiResult<(StatusCode, Json<Upload>)> {
    require_marketer(&state, &user, company_id).await?;
    let content_type = request.content_type.trim().to_lowercase();
    if !IMAGE_CONTENT_TYPES.contains(&content_type.as_str()) {
        return Err(ApiError::BadRequest(format!(
            "unsupported content type: {content_type}"
        )));
    }
    let key = format!(
        "{}/{}{}",
        UploadFolder::Leads.as_str(),
        uuid::Uuid::new_v4(),
        extension(&request.file_name, &content_type)
    );
    let upload_url = state.storage.presign_put(&key, &content_type).await?;
    let url = state.storage.public_url(&key)?;
    Ok((
        StatusCode::CREATED,
        Json(Upload {
            upload_url,
            url,
            token: None,
        }),
    ))
}

/// Search the company's customers
///
/// The employee customer search (`GET /v1/customers/search`) for the
/// marketing team's "find a customer" box.
#[utoipa::path(
    get,
    path = "/v1/marketing/{company_id}/customers/search",
    operation_id = "search_marketing_customers",
    tag = "marketing",
    params(("company_id" = i32, Path, description = "Company id"), CustomerSearchQuery),
    responses(
        (status = 200, description = "Matching customers, best first", body = Vec<CustomerSearchRow>),
        (status = 400, description = "Missing `term` or unknown `by`", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not on the company's marketing team", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn search(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(company_id): Path<i32>,
    MultiQuery(query): MultiQuery<CustomerSearchQuery>,
) -> ApiResult<Json<Vec<CustomerSearchRow>>> {
    require_marketer(&state, &user, company_id).await?;
    Ok(Json(
        search_customers(&state.pool, company_id, user.id, &query).await?,
    ))
}
