//! `POST /v1/phone/pushes/notification` and `/v1/phone/pushes/lead`: phone
//! pushes of what the other Lambdas send to Telegram.
//!
//! The `time-triggered` Lambda (activity deadline reminders) and the
//! `webhooks` Lambda (lead alerts) call these by their key; the phones and
//! FCM are here (`crate::phone::crm_push`).

use crate::auth::job::JobCaller;
use crate::error::{ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::phone::crm_push::{
    LeadKind, push_lead, push_leads_changed_logged, push_manager_lead, push_notification,
    sales_managers,
};
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};

/// Body of `POST /v1/phone/pushes/notification`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct NotificationPush {
    /// The `notifications` row (`notif-<id>` in `GET /v1/notifications`).
    pub notification_id: i64,
}

/// Body of `POST /v1/phone/pushes/lead`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct LeadPush {
    /// The lead's company.
    pub company_id: i32,
    /// The lead (a customer of `company_id`).
    pub customer_id: i32,
    /// Its deal; else the customer's newest live deal, if any.
    #[serde(default)]
    pub deal_id: Option<i64>,
    pub kind: LeadKind,
    /// Who is told: this user (the sales rep); the company's sales managers
    /// when left out.
    #[serde(default)]
    pub user_id: Option<i32>,
    /// Sales managers not told (no `user_id` only): for `taken`, the rep
    /// and the manager who assigned the lead.
    #[serde(default)]
    pub except_user_ids: Vec<i32>,
}

/// How many phones were told.
#[derive(Debug, Serialize, ToSchema)]
pub struct PushSent {
    /// Phones FCM accepted the push for; 0 when there was nothing to push or
    /// no signed-in phone.
    pub pushed: usize,
}

/// Push a CRM notification to its user's phones
///
/// Called by the `time-triggered` Lambda (job key) when an activity
/// deadline reminder is due, as it sends the Telegram message. The
/// notification is pushed to its user's signed-in phones as data
/// `{granite: "1", type: "notification", notificationId, heading}`: its id
/// and heading ("Activity Reminder"). The app reads the rest with
/// `GET /v1/notifications/{id}`. Nothing is pushed
/// (`pushed: 0`) for an unknown notification or a deleted user. Calling it
/// again pushes again.
#[crate::openapi::path(
    post,
    path = "/v1/phone/pushes/notification",
    operation_id = "push_notification",
    tag = "phone",
    request_body = NotificationPush,
    responses(
        (status = 200, description = "Phones told", body = PushSent),
        (status = 400, description = "Invalid body", body = ErrorBody),
        (status = 401, description = "Missing or wrong key", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("job_key" = []))
)]
pub async fn push_notification_route(
    State(state): State<AppState>,
    _caller: JobCaller,
    JsonBody(body): JsonBody<NotificationPush>,
) -> ApiResult<Json<PushSent>> {
    let pushed = push_notification(&state, body.notification_id).await?;
    Ok(Json(PushSent { pushed }))
}

/// Push a lead alert to phones
///
/// Called by the `webhooks` Lambda (job key) as it sends a lead alert to
/// Telegram: to the sales rep `user_id`, else to every sales manager of the
/// company (linked to Telegram or not). Pushed as data `{granite: "1",
/// type: "lead", kind, customerId, dealId}` (`dealId` left out when the lead
/// has no deal yet): ids only. The managers' new and repeated leads also
/// carry `assign: "1"` (tapping opens the Assign a sales rep sheet). A
/// `taken` push tells the managers (but `except_user_ids`) that the lead
/// was assigned, as its Telegram message is edited. Nothing is pushed (`pushed: 0`) for another
/// company's or a deleted customer. Calling it again pushes again.
///
/// A managers' push (no `user_id`) but `quote` also sends every other phone
/// of the company, the rep and the manager who assigned it included, a quiet
/// `{granite: "1", type: "leads"}` push (not counted in `pushed`): it shows
/// nothing and makes the app refetch its lead inbox, so a lead assigned in
/// Telegram leaves "Waiting for a sales rep" on every phone at once.
#[crate::openapi::path(
    post,
    path = "/v1/phone/pushes/lead",
    operation_id = "push_lead",
    tag = "phone",
    request_body = LeadPush,
    responses(
        (status = 200, description = "Phones told", body = PushSent),
        (status = 400, description = "Invalid body", body = ErrorBody),
        (status = 401, description = "Missing or wrong key", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("job_key" = []))
)]
pub async fn push_lead_route(
    State(state): State<AppState>,
    _caller: JobCaller,
    JsonBody(body): JsonBody<LeadPush>,
) -> ApiResult<Json<PushSent>> {
    let pushed = match body.user_id {
        Some(user_id) => {
            push_lead(
                &state,
                body.company_id,
                &[user_id],
                body.kind,
                body.customer_id,
                body.deal_id,
            )
            .await?
        }
        None => {
            let mut managers = sales_managers(&state.pool, body.company_id).await?;
            managers.retain(|id| !body.except_user_ids.contains(id));
            let pushed = push_manager_lead(
                &state,
                body.company_id,
                &managers,
                body.kind,
                body.customer_id,
                body.deal_id,
            )
            .await?;
            if body.kind != LeadKind::Quote && lead_of(&state, &body).await? {
                push_leads_changed_logged(&state, body.company_id, &managers).await;
            }
            pushed
        }
    };
    Ok(Json(PushSent { pushed }))
}

/// The push names a live customer of its company (else nobody is told).
async fn lead_of(state: &AppState, body: &LeadPush) -> sqlx::Result<bool> {
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM customers WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(body.customer_id)
    .bind(body.company_id)
    .fetch_optional(&state.pool)
    .await?;
    Ok(found.is_some())
}
