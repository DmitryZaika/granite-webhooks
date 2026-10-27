//! `POST /v1/phone/pushes/test` (temporary): the app's Settings sends the
//! caller's phones one push of each kind, to see how each looks and that it
//! arrives. Delete with the app's Test notifications screen.

use super::pushes::PushSent;
use crate::auth::device::DeviceUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::phone::crm_push::{LeadKind, push_lead, push_maintenance, push_notification};
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use serde::Deserialize;
use crate::openapi::ToSchema;

/// What `POST /v1/phone/pushes/test` sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TestPushKind {
    /// "Added an Activity" on my newest deal.
    ActivityAdded,
    /// "Added a Note" on my newest deal.
    NoteAdded,
    /// "Added a Comment" on my newest deal.
    CommentAdded,
    /// The reminder of my latest open activity that has a time.
    ActivityReminder,
    /// An installation reminder on my newest deal.
    AppointmentReminder,
    /// A new lead (the company's newest customer without a deal).
    LeadNew,
    /// A repeated lead (my newest deal).
    LeadRepeat,
    /// A lead assigned to me (my newest deal).
    LeadAssigned,
    /// A finished online quote (my newest deal).
    LeadQuote,
    /// Three maintenance checks due today.
    Maintenance,
}

/// Body of `POST /v1/phone/pushes/test`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct TestPush {
    pub kind: TestPushKind,
}

/// The caller's newest live deal and its customer.
async fn newest_deal(state: &AppState, user_id: i32, company_id: i32) -> ApiResult<(i64, i32)> {
    sqlx::query_as(
        "SELECT CAST(d.id AS SIGNED), d.customer_id FROM deals d
           JOIN customers c ON c.id = d.customer_id AND c.company_id = ? AND c.deleted_at IS NULL
          WHERE d.user_id = ? AND d.deleted_at IS NULL
          ORDER BY d.id DESC LIMIT 1",
    )
    .bind(company_id)
    .bind(user_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::Conflict("You have no deal to test with".into()))
}

/// Saves a `notifications` row for the caller, as the CRM would, and pushes
/// it.
async fn push_test_notification(
    state: &AppState,
    user_id: i32,
    deal_id: i64,
    notification_type: &str,
    message: &str,
    actor_name: Option<&str>,
) -> ApiResult<usize> {
    let id = sqlx::query(
        "INSERT INTO notifications (user_id, deal_id, message, notification_type, actor_name, due_at)
         VALUES (?, ?, ?, ?, ?, NOW())",
    )
    .bind(user_id)
    .bind(deal_id)
    .bind(message)
    .bind(notification_type)
    .bind(actor_name)
    .execute(&state.pool)
    .await?
    .last_insert_id();
    Ok(push_notification(state, i64::try_from(id).unwrap_or_default()).await?)
}

/// Send me a test push (temporary)
///
/// Sends one push of `kind` to the caller's signed-in phones, as the real
/// event would, so the app's Settings can show each notification. The CRM
/// kinds save a real notification first (on the caller's newest deal; it
/// shows in the bell too); the activity reminder uses the caller's latest
/// open activity that has a time; lead kinds name a real customer. 409 when
/// there is nothing to test with.
#[crate::openapi::path(
    post,
    path = "/v1/phone/pushes/test",
    operation_id = "send_test_push",
    tag = "phone",
    request_body = TestPush,
    responses(
        (status = 200, description = "Phones told", body = PushSent),
        (status = 400, description = "Invalid body", body = ErrorBody),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 409, description = "Nothing to test with", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn send_test_push(
    State(state): State<AppState>,
    device: DeviceUser,
    JsonBody(body): JsonBody<TestPush>,
) -> ApiResult<Json<PushSent>> {
    let user = &device.user;
    let (me, company) = (user.id, user.company_id);
    let actor = user.name.as_deref().unwrap_or("Test");
    let crm = match body.kind {
        TestPushKind::ActivityAdded => Some((
            "activity_added",
            "Test: call the customer back",
            Some(actor),
        )),
        TestPushKind::NoteAdded => Some((
            "note_added",
            "Test: the customer wants the quote by Friday",
            Some(actor),
        )),
        TestPushKind::CommentAdded => Some(("comment_added", "Test: sounds good", Some(actor))),
        TestPushKind::AppointmentReminder => Some((
            "installation_appointment_reminder",
            "Test: installation tomorrow at 9:00 AM.",
            None,
        )),
        _ => None,
    };
    if let Some((kind, message, actor)) = crm {
        let (deal_id, _) = newest_deal(&state, me, company).await?;
        let pushed = push_test_notification(&state, me, deal_id, kind, message, actor).await?;
        return Ok(Json(PushSent { pushed }));
    }
    let pushed = match body.kind {
        TestPushKind::ActivityReminder => {
            // The bell lists a reminder only while its timed activity is open.
            let activity: Option<(i64, String)> = sqlx::query_as(
                "SELECT CAST(da.deal_id AS SIGNED), LEFT(da.name, 255) FROM deal_activities da
                   JOIN deals d ON d.id = da.deal_id AND d.deleted_at IS NULL AND d.user_id = ?
                  WHERE da.deleted_at IS NULL AND da.is_completed = 0 AND da.deadline IS NOT NULL
                    AND TIME(da.deadline) <> '00:00:00'
                  ORDER BY da.deadline DESC LIMIT 1",
            )
            .bind(me)
            .fetch_optional(&state.pool)
            .await?;
            let (deal_id, name) = activity.ok_or_else(|| {
                ApiError::Conflict("Add an activity with a time to test its reminder".into())
            })?;
            push_test_notification(
                &state,
                me,
                deal_id,
                "activity_deadline_reminder",
                &name,
                None,
            )
            .await?
        }
        TestPushKind::LeadNew => {
            let customer: Option<i32> = sqlx::query_scalar(
                "SELECT c.id FROM customers c
                  WHERE c.company_id = ? AND c.deleted_at IS NULL
                    AND NOT EXISTS (SELECT 1 FROM deals d
                                     WHERE d.customer_id = c.id AND d.deleted_at IS NULL)
                  ORDER BY c.id DESC LIMIT 1",
            )
            .bind(company)
            .fetch_optional(&state.pool)
            .await?;
            let customer = customer.ok_or_else(|| {
                ApiError::Conflict("No customer without a deal to test with".into())
            })?;
            push_lead(&state, company, &[me], LeadKind::New, customer, None).await?
        }
        TestPushKind::Maintenance => push_maintenance(&state, company, me, 3).await?,
        kind => {
            let lead = match kind {
                TestPushKind::LeadRepeat => LeadKind::Repeat,
                TestPushKind::LeadAssigned => LeadKind::Assigned,
                _ => LeadKind::Quote,
            };
            let (deal_id, customer) = newest_deal(&state, me, company).await?;
            push_lead(&state, company, &[me], lead, customer, Some(deal_id)).await?
        }
    };
    Ok(Json(PushSent { pushed }))
}
