//! The notification bell.
//!
//! - `POST /v1/notifications/done` → dismiss notifications, or all of them
//!
//! Replaces the Remix `api.notifications.done` action.

use crate::auth::EmployeeUser;
use crate::auth::session::SessionUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::state::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Deserialize;
use sqlx::{MySql, QueryBuilder};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(done))
}

/// What a notification is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum NotificationKind {
    Activity,
    Email,
    Call,
    Billing,
}

/// One notification of the bell.
#[derive(Debug, Deserialize, ToSchema)]
pub struct NotificationRef {
    /// `notif-<id>` for activities, `billing-<id>` for billing, the thread
    /// id for email. Calls are dismissed in the browser only.
    pub id: String,
    pub kind: NotificationKind,
}

/// Body of `POST /v1/notifications/done`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct DoneInput {
    /// Dismiss everything: activity and billing notifications, and every
    /// unread incoming email.
    #[serde(default)]
    pub clear_all: bool,
    /// The notifications to dismiss.
    #[serde(default)]
    pub items: Vec<NotificationRef>,
}

fn numbered(items: &[NotificationRef], kind: NotificationKind, prefix: &str) -> Vec<i64> {
    items
        .iter()
        .filter(|item| item.kind == kind)
        .filter_map(|item| {
            item.id
                .strip_prefix(prefix)
                .unwrap_or(&item.id)
                .parse()
                .ok()
        })
        .filter(|id| *id > 0)
        .collect()
}

/// Pushes the CRM's `dismissIncomingEmailSql`: customer mail to the user or
/// on their deal, and coworkers' mail addressed to them.
fn push_incoming(builder: &mut QueryBuilder<MySql>, user: &SessionUser) {
    let email = user.email.trim().to_lowercase();
    let like = format!("%<{email}>");
    let addressed = |builder: &mut QueryBuilder<MySql>| {
        builder
            .push("e.receiver_user_id = ")
            .push_bind(user.id)
            .push(" OR LOWER(TRIM(SUBSTRING_INDEX(SUBSTRING_INDEX(e.receiver_email, '<', -1), '>', 1))) = ")
            .push_bind(email.clone())
            .push(" OR (e.receiver_email NOT LIKE '%<%' AND LOWER(TRIM(e.receiver_email)) = ")
            .push_bind(email.clone())
            .push(") OR e.receiver_email LIKE ")
            .push_bind(like.clone());
    };
    builder
        .push("((e.sender_user_id IS NULL AND (d.user_id = ")
        .push_bind(user.id)
        .push(" OR ");
    addressed(builder);
    builder
        .push(")) OR (e.sender_user_id IS NOT NULL AND e.sender_user_id <> ")
        .push_bind(user.id)
        .push(" AND (");
    addressed(builder);
    builder.push(")))");
}

/// Dismiss notifications
///
/// Marks activity and billing notifications done and the listed email
/// threads read. With `clear_all`, dismisses every one of them and marks all
/// unread incoming mail read.
#[utoipa::path(
    post,
    path = "/v1/notifications/done",
    operation_id = "dismiss_notifications",
    tag = "notifications",
    request_body = DoneInput,
    responses(
        (status = 204, description = "Dismissed"),
        (status = 400, description = "Nothing to dismiss", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn done(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<DoneInput>,
) -> ApiResult<StatusCode> {
    let threads: Vec<&str> = input
        .items
        .iter()
        .filter(|item| item.kind == NotificationKind::Email && !item.id.is_empty())
        .map(|item| item.id.as_str())
        .collect();
    let activities = numbered(&input.items, NotificationKind::Activity, "notif-");
    let billing = numbered(&input.items, NotificationKind::Billing, "billing-");
    if !input.clear_all && input.items.is_empty() {
        return Err(ApiError::BadRequest("notification target required".into()));
    }
    let mut tx = state.pool.begin().await?;
    for (table, ids) in [
        ("notifications", &activities),
        ("billing_notifications", &billing),
    ] {
        if !input.clear_all && ids.is_empty() {
            continue;
        }
        let mut builder =
            QueryBuilder::<MySql>::new(format!("UPDATE {table} SET is_done = 1 WHERE user_id = "));
        builder.push_bind(user.id);
        if !input.clear_all {
            builder.push(" AND id IN (");
            let mut list = builder.separated(", ");
            for id in ids {
                list.push_bind(*id);
            }
            builder.push(")");
        }
        builder.build().execute(&mut *tx).await?;
    }
    if input.clear_all || !threads.is_empty() {
        let mut builder = QueryBuilder::<MySql>::new(
            "UPDATE emails e LEFT JOIN deals d ON d.id = e.deal_id AND d.deleted_at IS NULL
                SET e.employee_read_at = NOW()
              WHERE e.deleted_at IS NULL AND e.thread_id IS NOT NULL AND e.employee_read_at IS NULL AND ",
        );
        if !input.clear_all {
            builder.push("e.thread_id IN (");
            let mut list = builder.separated(", ");
            for thread in &threads {
                list.push_bind(*thread);
            }
            builder.push(") AND ");
        }
        push_incoming(&mut builder, &user);
        builder.build().execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}
