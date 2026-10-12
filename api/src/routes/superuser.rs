//! Pages of the super admin dashboard.
//!
//! - `GET /v1/superuser/onboarding-feedbacks`           → every guided-tour rating
//! - `GET /v1/superuser/signature-access`               → who may send for signature
//! - `PUT /v1/superuser/users/{id}/signature-access`    → let a user send, or not
//!
//! Replaces the Remix `admin.superuser-dashboard.onboarding-feedbacks` and
//! `admin.superuser-dashboard.feature-access` loaders and actions.

use super::company::queries::signature_on;
use crate::auth::CurrentUser;
use crate::auth::session::SessionUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::{JsonBody, MultiQuery};
use crate::serde_helpers::js_date;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(onboarding_feedbacks))
        .routes(routes!(signature_access))
        .routes(routes!(set_signature_access))
        .routes(routes!(widget_analytics))
}

const fn super_only(user: &SessionUser) -> ApiResult<()> {
    if user.is_superuser {
        Ok(())
    } else {
        Err(ApiError::Forbidden)
    }
}

/// A user's rating of the guided tours.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct OnboardingFeedback {
    /// Feedback id.
    pub id: i32,
    /// The user.
    pub user_id: i32,
    /// Their company.
    pub company_id: i32,
    /// 1–5.
    pub rating: i32,
    /// What they wrote.
    pub feedback_text: Option<String>,
    /// When.
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub created_at: Option<DateTime<Utc>>,
    /// The user's name.
    pub user_name: Option<String>,
    /// The user's email.
    pub user_email: Option<String>,
    /// The company's name.
    pub company_name: Option<String>,
}

/// List onboarding feedback
///
/// Every company's guided-tour ratings, newest first. Super admins only.
#[utoipa::path(
    get,
    path = "/v1/superuser/onboarding-feedbacks",
    operation_id = "list_onboarding_feedbacks",
    tag = "superuser",
    responses(
        (status = 200, description = "Ratings", body = Vec<OnboardingFeedback>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a super admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn onboarding_feedbacks(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> ApiResult<Json<Vec<OnboardingFeedback>>> {
    super_only(&user)?;
    Ok(Json(
        sqlx::query_as(
            "SELECT ofb.id, ofb.user_id, ofb.company_id, CAST(ofb.rating AS SIGNED) AS rating,
                    ofb.feedback_text, ofb.created_at, u.name AS user_name, u.email AS user_email,
                    c.name AS company_name
               FROM onboarding_feedback ofb
               LEFT JOIN users u ON u.id = ofb.user_id
               LEFT JOIN company c ON c.id = ofb.company_id
              ORDER BY ofb.created_at DESC, ofb.id DESC",
        )
        .fetch_all(&state.pool)
        .await?,
    ))
}

/// Query of `GET /v1/superuser/signature-access`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct AccessQuery {
    /// Only this company's users (ignored unless it has signatures).
    pub company_id: Option<i32>,
}

/// A company with signatures on.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct CompanyOption {
    /// Company id.
    pub id: i32,
    /// Its name.
    pub name: Option<String>,
}

/// A user and whether they may send documents for signature.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct SignatureUser {
    /// User id.
    pub id: i32,
    /// Name.
    pub name: Option<String>,
    /// Email.
    pub email: Option<String>,
    /// Company.
    pub company_id: i32,
    /// Company name.
    pub company_name: Option<String>,
    /// May send for signature.
    pub is_signature_available: bool,
}

/// Who may send for signature.
#[derive(Debug, Serialize, ToSchema)]
pub struct SignatureAccess {
    /// Companies with signatures on, by name.
    pub companies: Vec<CompanyOption>,
    /// Their live users, by company then name.
    pub users: Vec<SignatureUser>,
    /// The company the list is narrowed to.
    pub company_id: Option<i32>,
}

/// List signature access
///
/// The companies that have signatures switched on and their users with
/// whether each may send documents for signature; `company_id` narrows the
/// users to one of those companies. Super admins only.
#[utoipa::path(
    get,
    path = "/v1/superuser/signature-access",
    operation_id = "list_signature_access",
    tag = "superuser",
    params(AccessQuery),
    responses(
        (status = 200, description = "Companies and users", body = SignatureAccess),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a super admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn signature_access(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    MultiQuery(query): MultiQuery<AccessQuery>,
) -> ApiResult<Json<SignatureAccess>> {
    super_only(&user)?;
    let companies: Vec<CompanyOption> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT c.id, c.name FROM company c
          WHERE c.deleted_at IS NULL AND {}
          ORDER BY c.name, c.id",
        signature_on()
    )))
    .fetch_all(&state.pool)
    .await?;
    let company_id = query
        .company_id
        .filter(|id| companies.iter().any(|company| company.id == *id));
    let ids: Vec<i32> = company_id.map_or_else(
        || companies.iter().map(|company| company.id).collect(),
        |id| vec![id],
    );
    let users: Vec<SignatureUser> = if ids.is_empty() {
        Vec::new()
    } else {
        let marks = vec!["?"; ids.len()].join(",");
        let mut sql = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT u.id, u.name, u.email, u.company_id, c.name AS company_name,
                    COALESCE(u.is_signature_available, 0) <> 0 AS is_signature_available
               FROM users u JOIN company c ON c.id = u.company_id
              WHERE u.is_deleted = 0 AND c.deleted_at IS NULL AND u.company_id IN ({marks})
              ORDER BY c.name, u.name, u.id"
        )));
        for id in &ids {
            sql = sql.bind(id);
        }
        sql.fetch_all(&state.pool).await?
    };
    Ok(Json(SignatureAccess {
        companies,
        users,
        company_id,
    }))
}

/// Body of `PUT /v1/superuser/users/{id}/signature-access`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SignatureAccessInput {
    /// May send documents for signature.
    pub enabled: bool,
}

/// Set a user's signature access
///
/// Lets a live user send documents for signature, or stops them. Super
/// admins only.
#[utoipa::path(
    put,
    path = "/v1/superuser/users/{id}/signature-access",
    operation_id = "set_signature_access",
    tag = "superuser",
    params(("id" = i32, Path, description = "User id")),
    request_body = SignatureAccessInput,
    responses(
        (status = 204, description = "Saved"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a super admin", body = ErrorBody),
        (status = 404, description = "No such live user", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn set_signature_access(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<SignatureAccessInput>,
) -> ApiResult<StatusCode> {
    super_only(&user)?;
    let found: Option<i32> =
        sqlx::query_scalar("SELECT id FROM users WHERE id = ? AND is_deleted = 0")
            .bind(id)
            .fetch_optional(&state.pool)
            .await?;
    found.ok_or(ApiError::NotFound)?;
    sqlx::query("UPDATE users SET is_signature_available = ? WHERE id = ?")
        .bind(input.enabled)
        .bind(id)
        .execute(&state.pool)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// How often users clicked or changed something on a widget.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct WidgetUse {
    /// The company.
    pub company_id: i32,
    /// Its name.
    pub company_name: Option<String>,
    /// The user.
    pub user_id: i32,
    /// Their name.
    pub user_name: Option<String>,
    /// The widget.
    pub widget_id: String,
    /// What was clicked or changed.
    pub target: String,
    /// `click` or `change`.
    pub action: String,
    /// How many times.
    pub uses: i64,
}

/// A widget event.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct WidgetEvent {
    /// Event id.
    pub id: i32,
    /// The company.
    pub company_id: i32,
    /// Its name.
    pub company_name: Option<String>,
    /// The user.
    pub user_id: i32,
    /// Their name.
    pub user_name: Option<String>,
    /// Their email.
    pub user_email: Option<String>,
    /// The widget.
    pub widget_id: String,
    /// What happened.
    pub action: String,
    /// On what.
    pub target: String,
    /// When.
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub created_at: Option<DateTime<Utc>>,
}

/// A widget on a user's dashboard.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct DashboardWidget {
    /// The company.
    pub company_id: i32,
    /// Its name.
    pub company_name: Option<String>,
    /// The user.
    pub user_id: i32,
    /// Their name.
    pub user_name: Option<String>,
    /// Their email.
    pub user_email: Option<String>,
    /// The widget.
    pub widget_id: String,
    /// Its place on the dashboard.
    pub position: i32,
}

/// Dashboard widget analytics.
#[derive(Debug, Serialize, ToSchema)]
pub struct WidgetAnalytics {
    /// Uses per user, widget and target, most first.
    pub top_used: Vec<WidgetUse>,
    /// Every event, newest first.
    pub events: Vec<WidgetEvent>,
    /// Every user's dashboard widgets, by company, user and position.
    pub current_widgets: Vec<DashboardWidget>,
}

/// Dashboard widget analytics
///
/// How every company's users use their dashboard widgets: uses per widget,
/// all events and the widgets each dashboard has. Super admins only.
#[utoipa::path(
    get,
    path = "/v1/superuser/widget-analytics",
    operation_id = "get_widget_analytics",
    tag = "superuser",
    responses(
        (status = 200, description = "The analytics", body = WidgetAnalytics),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a super admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn widget_analytics(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> ApiResult<Json<WidgetAnalytics>> {
    super_only(&user)?;
    let pool = &state.pool;
    let top_used = sqlx::query_as(
        "SELECT e.company_id, c.name AS company_name, e.user_id, u.name AS user_name, e.widget_id,
                e.target, e.action, COUNT(*) AS uses
           FROM widget_analytics_events e
           LEFT JOIN users u ON u.id = e.user_id
           LEFT JOIN company c ON c.id = e.company_id
          WHERE e.action IN ('click', 'change')
          GROUP BY e.company_id, c.name, e.user_id, u.name, e.widget_id, e.target, e.action
          ORDER BY uses DESC, e.user_id ASC, e.widget_id ASC, e.target ASC",
    )
    .fetch_all(pool)
    .await?;
    let events = sqlx::query_as(
        "SELECT e.id, e.company_id, c.name AS company_name, e.user_id, u.name AS user_name,
                u.email AS user_email, e.widget_id, e.action, e.target, e.created_at
           FROM widget_analytics_events e
           LEFT JOIN users u ON u.id = e.user_id
           LEFT JOIN company c ON c.id = e.company_id
          ORDER BY e.created_at DESC, e.id DESC",
    )
    .fetch_all(pool)
    .await?;
    let current_widgets = sqlx::query_as(
        "SELECT w.company_id, c.name AS company_name, w.user_id, u.name AS user_name,
                u.email AS user_email, w.widget_id, w.position
           FROM user_dashboard_widgets w
           LEFT JOIN users u ON u.id = w.user_id
           LEFT JOIN company c ON c.id = w.company_id
          ORDER BY c.name ASC, u.name ASC, w.position ASC, w.widget_id ASC",
    )
    .fetch_all(pool)
    .await?;
    Ok(Json(WidgetAnalytics {
        top_used,
        events,
        current_widgets,
    }))
}
