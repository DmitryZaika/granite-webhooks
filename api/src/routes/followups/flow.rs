//! The SMS / email flow running for a customer or an SMS thread.
//!
//! What the banner shows, and the rep's start, switch, pause, resume, stop,
//! "send now" and the countdown's dispatch. A step due right away is sent before
//! the response (a Lambda cannot keep working after it). Replaces the Remix
//! `api.followups.customer.$customerId.flow` and
//! `api.{cloudtalk,ringcentral}.sms.thread.$phoneDigits.flow` routes.

use crate::auth::EmployeeUser;
use crate::auth::session::SessionUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::followups::enrollments::{self, Control, Refusal, Target};
use crate::followups::processor::process_due_enrollment;
use crate::openapi::ToSchema;
use crate::routes::sms::provider::{SmsProvider, valid_thread_phone};
use crate::routes::sms::schemas::ActiveFlow;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::time::Instant;

/// The flow on a customer or thread.
#[derive(Debug, Serialize, ToSchema)]
pub struct FlowState {
    /// Running, paused or last failed; `None` when there is none (or the
    /// caller may not see it).
    pub active_flow: Option<ActiveFlow>,
}

/// What to do with the flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum FlowIntent {
    /// Start `flow_id` (409 `flow_already_active` when one runs).
    Start,
    /// Cancel the running flow and start `flow_id`.
    Switch,
    Pause,
    Resume,
    Stop,
    /// The banner's countdown ran out: send the due step.
    Dispatch,
    /// Send the next step now.
    SendNow,
}

/// `POST .../flow`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct FlowAction {
    pub intent: FlowIntent,
    /// The flow to start or switch to.
    pub flow_id: Option<i32>,
}

/// Over `SMS_FLOW_START_RATE_LIMIT_PER_MIN` (default 10) starts a minute.
pub(crate) fn take_start_slot(state: &AppState, user_id: i32) -> bool {
    let limit = std::env::var("SMS_FLOW_START_RATE_LIMIT_PER_MIN")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|limit| *limit > 0)
        .unwrap_or(10);
    let mut slots = state
        .flow_starts
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = Instant::now();
    let window = std::time::Duration::from_secs(60);
    slots.retain(|_, starts: &mut VecDeque<Instant>| {
        starts.retain(|at| now.duration_since(*at) < window);
        !starts.is_empty()
    });
    let starts = slots.entry(user_id).or_default();
    let allowed = starts.len() < limit;
    if allowed {
        starts.push_back(now);
    }
    drop(slots);
    allowed
}

fn refused(refusal: Refusal) -> ApiError {
    match refusal {
        "flow_already_active" => ApiError::Conflict(refusal.to_string()),
        "forbidden" => ApiError::Forbidden,
        _ => ApiError::BadRequest(refusal.to_string()),
    }
}

async fn summary(
    state: &AppState,
    user: &SessionUser,
    target: &Target,
    provider: SmsProvider,
) -> ApiResult<Option<ActiveFlow>> {
    Ok(match target {
        Target::Customer(id) => enrollments::summary_for_customer(&state.pool, user, *id).await?,
        Target::Phone(phone) => {
            enrollments::summary_for_phone(&state.pool, provider, user, phone).await?
        }
    })
}

async fn act(
    state: &AppState,
    user: &SessionUser,
    target: Target,
    provider: SmsProvider,
    action: FlowAction,
) -> ApiResult<FlowState> {
    let pool = &state.pool;
    let starting = matches!(action.intent, FlowIntent::Start | FlowIntent::Switch);
    if starting && !take_start_slot(state, user.id) {
        return Err(ApiError::TooManyRequests("rate_limited".into()));
    }
    let outcome = match action.intent {
        FlowIntent::Start | FlowIntent::Switch => {
            let Some(flow_id) = action.flow_id.filter(|id| *id > 0) else {
                return Err(ApiError::BadRequest("validation_failed".into()));
            };
            // The rep's own line, as their sends use it (not the company's first).
            let sms_provider =
                crate::followups::processor::rep_sms_provider(state, user.company_id, user).await?;
            let (phone, customer_id) = match &target {
                Target::Phone(phone) => (Some(phone.as_str()), None),
                Target::Customer(id) => (None, Some(*id)),
            };
            enrollments::start(
                pool,
                user,
                sms_provider,
                flow_id,
                phone,
                customer_id,
                action.intent == FlowIntent::Switch,
            )
            .await?
        }
        FlowIntent::Pause => enrollments::control(pool, user, &target, Control::Pause).await?,
        FlowIntent::Resume => enrollments::control(pool, user, &target, Control::Resume).await?,
        FlowIntent::Stop => enrollments::control(pool, user, &target, Control::Stop).await?,
        FlowIntent::SendNow => enrollments::control(pool, user, &target, Control::SendNow).await?,
        FlowIntent::Dispatch => {
            let visible = summary(state, user, &target, provider).await?;
            enrollments::dispatchable(visible.as_ref()).map(|()| true)
        }
    };
    let send_now = outcome.map_err(refused)?;
    if send_now && let Some(pending) = summary(state, user, &target, provider).await? {
        process_due_enrollment(state, pending.enrollment_id).await;
    }
    Ok(FlowState {
        active_flow: summary(state, user, &target, provider).await?,
    })
}

fn customer_target(customer_id: i32) -> ApiResult<Target> {
    if customer_id <= 0 {
        return Err(ApiError::BadRequest("customer_not_found".into()));
    }
    Ok(Target::Customer(customer_id))
}

fn phone_target(phone: &str) -> ApiResult<Target> {
    if !valid_thread_phone(phone) {
        return Err(ApiError::BadRequest("invalid_phone".into()));
    }
    Ok(Target::Phone(enrollments::canonical_phone10(phone)))
}

/// A customer's flow
///
/// The flow running for the customer (or the last one that failed).
#[crate::openapi::path(
    get,
    path = "/v1/customers/{customer_id}/flow",
    operation_id = "get_customer_flow",
    tag = "followups",
    params(("customer_id" = i32, Path, description = "Customer id")),
    responses(
        (status = 200, description = "The flow", body = FlowState),
        (status = 400, description = "Not a customer id", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get_customer_flow(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(customer_id): Path<i32>,
) -> ApiResult<Json<FlowState>> {
    let target = customer_target(customer_id)?;
    Ok(Json(FlowState {
        active_flow: summary(&state, &user, &target, SmsProvider::Cloudtalk).await?,
    }))
}

/// Act on a customer's flow
///
/// Starts, switches, pauses, resumes or stops the customer's flow, or sends
/// its next step now. Refusals come back as a token in `error`
/// (`flow_not_found`, `agent_not_linked`, `missing_email`, `invalid_status`,
/// `not_due`, ...).
#[crate::openapi::path(
    post,
    path = "/v1/customers/{customer_id}/flow",
    operation_id = "act_on_customer_flow",
    tag = "followups",
    params(("customer_id" = i32, Path, description = "Customer id")),
    request_body = FlowAction,
    responses(
        (status = 200, description = "The flow afterwards", body = FlowState),
        (status = 400, description = "Refused", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Not the caller's flow to change", body = ErrorBody),
        (status = 409, description = "A flow is already running", body = ErrorBody),
        (status = 429, description = "Over ten starts a minute", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn act_on_customer_flow(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(customer_id): Path<i32>,
    JsonBody(action): JsonBody<FlowAction>,
) -> ApiResult<Json<FlowState>> {
    let target = customer_target(customer_id)?;
    Ok(Json(
        act(&state, &user, target, SmsProvider::Cloudtalk, action).await?,
    ))
}

/// A thread's flow
///
/// The flow running on the phone, for someone who may see it.
#[crate::openapi::path(
    get,
    path = "/v1/sms/{provider}/threads/{phone}/flow",
    operation_id = "get_sms_thread_flow",
    tag = "sms",
    params(
        ("provider" = SmsProvider, Path, description = "SMS provider"),
        ("phone" = String, Path, description = "The other party's phone digits"),
    ),
    responses(
        (status = 200, description = "The flow", body = FlowState),
        (status = 400, description = "Not a phone number", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get_thread_flow(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((provider, phone)): Path<(SmsProvider, String)>,
) -> ApiResult<Json<FlowState>> {
    let target = phone_target(&phone)?;
    Ok(Json(FlowState {
        active_flow: summary(&state, &user, &target, provider).await?,
    }))
}

/// Act on a thread's flow
///
/// Like the customer's, for the phone of an SMS conversation.
#[crate::openapi::path(
    post,
    path = "/v1/sms/{provider}/threads/{phone}/flow",
    operation_id = "act_on_sms_thread_flow",
    tag = "sms",
    params(
        ("provider" = SmsProvider, Path, description = "SMS provider"),
        ("phone" = String, Path, description = "The other party's phone digits"),
    ),
    request_body = FlowAction,
    responses(
        (status = 200, description = "The flow afterwards", body = FlowState),
        (status = 400, description = "Refused", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Not the caller's flow to change", body = ErrorBody),
        (status = 409, description = "A flow is already running", body = ErrorBody),
        (status = 429, description = "Over ten starts a minute", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn act_on_thread_flow(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((provider, phone)): Path<(SmsProvider, String)>,
    JsonBody(action): JsonBody<FlowAction>,
) -> ApiResult<Json<FlowState>> {
    let target = phone_target(&phone)?;
    Ok(Json(act(&state, &user, target, provider, action).await?))
}
