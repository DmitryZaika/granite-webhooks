//! What the chat's AI design tool picks from.
//!
//! - `GET /v1/ai/design/pictures` → pictures customers emailed the caller
//! - `GET /v1/ai/design/deals` → the caller's deals, to save a design to
//!
//! A picked picture loads from `GET /v1/email-attachments/{id}/preview`; a
//! design is saved with `POST /v1/uploads` and `POST /v1/deals/{id}/files`.
//! Replaces the Remix `api.ai-design-assets` route.

use crate::auth::EmployeeUser;
use crate::error::{ApiResult, ErrorBody};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Query, State};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

const IMAGE_SUBTYPES: [&str; 6] = ["jpeg", "jpg", "png", "webp", "heic", "heif"];

/// The address in `Name <address>`, lowercased (as SQL).
fn normalized_email_sql(column: &str) -> String {
    format!("LOWER(TRIM(SUBSTRING_INDEX(SUBSTRING_INDEX({column}, '<', -1), '>', 1)))")
}

/// Query of `GET /v1/ai/design/pictures`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PicturesQuery {
    /// Page size, 1 to 60 (default 9).
    pub limit: Option<i64>,
    /// Pictures to skip, at most 500.
    pub offset: Option<i64>,
    /// Matches the customer's name, the sender or the file name.
    pub search: Option<String>,
}

/// A picture a customer emailed.
#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct EmailedPicture {
    /// The attachment's id.
    pub id: i64,
    /// Its file name (`customer-image-<id>` when it has none).
    pub filename: String,
    /// When the email arrived.
    pub sent_at: Option<DateTime<Utc>>,
    /// Who sent it.
    pub sender_email: String,
    /// The customer with that address, when known.
    pub customer_name: Option<String>,
}

/// A page of pictures.
#[derive(Debug, Serialize, ToSchema)]
pub struct EmailedPictures {
    /// Newest first.
    pub pictures: Vec<EmailedPicture>,
    /// More pictures follow.
    pub has_more: bool,
}

/// List pictures customers emailed
///
/// Image attachments of emails the caller received, newest first, with the
/// customer who sent each when known.
#[utoipa::path(
    get,
    path = "/v1/ai/design/pictures",
    operation_id = "list_ai_design_pictures",
    tag = "ai",
    params(PicturesQuery),
    responses(
        (status = 200, description = "A page of pictures", body = EmailedPictures),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn pictures(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Query(query): Query<PicturesQuery>,
) -> ApiResult<Json<EmailedPictures>> {
    let limit = query.limit.filter(|l| *l > 0).map_or(9, |l| l.min(60));
    let offset = query.offset.filter(|o| *o > 0).map_or(0, |o| o.min(500));
    let search: String = query
        .search
        .unwrap_or_default()
        .trim()
        .chars()
        .take(100)
        .collect();
    let email = user.email.trim().to_lowercase();
    let subtypes = IMAGE_SUBTYPES.map(|s| format!("'{s}'")).join(", ");
    let sql = format!(
        "SELECT CAST(ea.id AS SIGNED) AS id,
                COALESCE(NULLIF(ea.filename, ''), CONCAT('customer-image-', ea.id)) AS filename,
                e.sent_at, COALESCE(e.sender_email, '') AS sender_email, c.name AS customer_name
           FROM email_attachments ea
           JOIN emails e ON e.id = ea.email_id
           LEFT JOIN customers_emails ce ON {ce} = {sender}
           LEFT JOIN customers c ON c.id = ce.customer_id AND c.company_id = ? AND c.deleted_at IS NULL
          WHERE e.deleted_at IS NULL
            AND e.sender_user_id IS NULL
            AND (e.receiver_user_id = ? OR {receiver} = ?
                 OR (e.receiver_email NOT LIKE '%<%' AND LOWER(TRIM(e.receiver_email)) = ?)
                 OR e.receiver_email LIKE ?)
            AND (LOWER(TRIM(ea.content_type)) = 'image' OR LOWER(TRIM(ea.content_subtype)) IN ({subtypes}))
            {search_clause}
          ORDER BY e.sent_at DESC, ea.id DESC
          LIMIT ? OFFSET ?",
        ce = normalized_email_sql("ce.email"),
        sender = normalized_email_sql("e.sender_email"),
        receiver = normalized_email_sql("e.receiver_email"),
        search_clause = if search.is_empty() {
            ""
        } else {
            "AND (c.name LIKE ? OR e.sender_email LIKE ? OR ea.filename LIKE ?)"
        },
    );
    let mut select = sqlx::query_as::<_, EmailedPicture>(sqlx::AssertSqlSafe(sql))
        .bind(user.company_id)
        .bind(user.id)
        .bind(&email)
        .bind(&email)
        .bind(format!("%<{email}>"));
    if !search.is_empty() {
        let pattern = format!("%{search}%");
        select = select
            .bind(pattern.clone())
            .bind(pattern.clone())
            .bind(pattern);
    }
    let mut pictures = select
        .bind(limit + 1)
        .bind(offset)
        .fetch_all(&state.pool)
        .await?;
    let has_more = pictures.len() > usize::try_from(limit).unwrap_or(9);
    pictures.truncate(usize::try_from(limit).unwrap_or(9));
    Ok(Json(EmailedPictures { pictures, has_more }))
}

/// A deal a design can be saved to.
#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct DesignDeal {
    /// The deal.
    pub id: i64,
    /// Its title, when it has one.
    pub title: Option<String>,
    /// Its customer.
    pub customer_name: String,
}

/// List the caller's deals for a design
///
/// The caller's own live deals, newest first (at most 200).
#[utoipa::path(
    get,
    path = "/v1/ai/design/deals",
    operation_id = "list_ai_design_deals",
    tag = "ai",
    responses(
        (status = 200, description = "The caller's deals", body = [DesignDeal]),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn deals(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<Vec<DesignDeal>>> {
    let deals = sqlx::query_as(
        "SELECT CAST(d.id AS SIGNED) AS id, d.title, c.name AS customer_name
           FROM deals d JOIN customers c ON c.id = d.customer_id
          WHERE d.deleted_at IS NULL AND c.deleted_at IS NULL AND c.company_id = ? AND d.user_id = ?
          ORDER BY d.id DESC LIMIT 200",
    )
    .bind(user.company_id)
    .bind(user.id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(deals))
}
