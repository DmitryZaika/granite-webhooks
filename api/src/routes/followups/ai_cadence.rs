//! The AI follow-up of the flow picker: preview it, rewrite one message,
//! then start it (or switch the running flow to it). Replaces the Remix
//! `api.followups.ai-cadence` route.

use super::flow::{FlowState, take_start_slot};
use crate::auth::EmployeeUser;
use crate::auth::session::SessionUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::followups::ai_cadence::{
    self, CadencePreview, CadenceRefusal, CadenceTarget, RegenerateMessage,
};
use crate::followups::cadence::types::AiCadencePreviewMessage;
use crate::followups::crm_hold::AI_HOLD_MAX_DELAY_MINUTES;
use crate::followups::enrollments;
use crate::followups::generation::GenerationError;
use crate::followups::processor::{company_sms_provider, process_due_enrollment};
use crate::followups::send_window::FollowupChannel;
use crate::routes::sms::provider::{SmsProvider, valid_thread_phone};
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// SMS or email.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum CadenceChannel {
    Sms,
    Email,
}

impl From<FollowupChannel> for CadenceChannel {
    fn from(channel: FollowupChannel) -> Self {
        match channel {
            FollowupChannel::Sms => Self::Sms,
            FollowupChannel::Email => Self::Email,
        }
    }
}

impl From<CadenceChannel> for FollowupChannel {
    fn from(channel: CadenceChannel) -> Self {
        match channel {
            CadenceChannel::Sms => Self::Sms,
            CadenceChannel::Email => Self::Email,
        }
    }
}

/// One message of the follow-up, as written and maybe edited by the rep.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PreviewMessage {
    /// Minutes after the previous message (the first: after now).
    pub delay_minutes: i64,
    /// "Immediately", "+2 days", ...
    pub delay_label: String,
    /// What the message is for.
    pub goal: String,
    pub channel: CadenceChannel,
    /// Left out of the flow.
    pub skip: bool,
    /// Why it is left out.
    pub skip_reason: Option<String>,
    /// The text that will be sent.
    pub body: String,
    /// Email messages: the subject.
    pub subject: Option<String>,
    /// What the model wrote (the rep's edits are learned from).
    #[serde(default)]
    pub generated_body: Option<String>,
    /// What the model wrote as the subject.
    #[serde(default)]
    pub generated_subject: Option<String>,
    /// The message's angle (e.g. `quote_status`).
    #[serde(default)]
    pub angle: Option<String>,
    /// Where in the follow-up it falls (e.g. `EARLY_FOLLOWUP`).
    #[serde(default)]
    pub phase: Option<String>,
}

impl From<&AiCadencePreviewMessage> for PreviewMessage {
    fn from(message: &AiCadencePreviewMessage) -> Self {
        Self {
            #[allow(clippy::cast_possible_truncation)]
            delay_minutes: message.delay_minutes.round() as i64,
            delay_label: message.delay_label.clone(),
            goal: message.goal.clone(),
            channel: message.channel.into(),
            skip: message.skip,
            skip_reason: message.skip_reason.clone(),
            body: message.body.clone(),
            subject: message.subject.clone(),
            generated_body: message.generated_body.clone(),
            generated_subject: message.generated_subject.clone().flatten(),
            angle: message.angle.clone(),
            phase: message.phase.clone(),
        }
    }
}

impl PreviewMessage {
    fn engine(&self) -> AiCadencePreviewMessage {
        #[allow(clippy::cast_precision_loss)]
        AiCadencePreviewMessage {
            delay_minutes: self.delay_minutes as f64,
            delay_label: self.delay_label.clone(),
            goal: self.goal.clone(),
            channel: self.channel.into(),
            skip: self.skip,
            skip_reason: self.skip_reason.clone(),
            body: self.body.clone(),
            subject: self.subject.clone(),
            generated_body: self.generated_body.clone(),
            generated_subject: self
                .generated_body
                .as_ref()
                .map(|_| self.generated_subject.clone()),
            angle: self.angle.clone(),
            phase: self.phase.clone(),
        }
    }
}

/// Whose follow-up.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
pub struct Target {
    /// The customer (else the phone's customer, if any).
    pub customer_id: Option<i32>,
    /// The SMS thread's phone digits.
    pub phone_digits: Option<String>,
}

/// `POST /v1/followups/ai-cadence/preview`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct PreviewRequest {
    #[serde(flatten)]
    pub target: Target,
    /// Write it even though the customer should not be followed up now.
    #[serde(default)]
    pub force_generate: bool,
}

/// The written follow-up.
#[derive(Debug, Serialize, ToSchema)]
pub struct PreviewResult {
    pub channel: CadenceChannel,
    /// The last message sent to the customer.
    pub last_contact_at: Option<String>,
    /// Messages in the preview.
    pub remaining_step_count: usize,
    /// `new`, `unanswered` or `monthly`.
    pub cadence_mode: String,
    /// The messages, in sending order.
    pub preview: Vec<PreviewMessage>,
}

/// Why nothing was written.
#[derive(Debug, Serialize, ToSchema)]
pub struct CadenceError {
    /// `nothing_to_send` (blocked), `customer_not_found`, `missing_target`,
    /// `invalid_phone`, `missing_email`, `generate_failed`, ...
    pub error: String,
    /// The block's reason (e.g. `CUSTOMER_WILL_RECONTACT`).
    pub reason_code: Option<String>,
    /// The block explained for the rep.
    pub reason_message: Option<String>,
    /// "Generate anyway" may override the block.
    pub can_force_generate: Option<bool>,
}

fn refuse(status: StatusCode, error: &str) -> Response {
    (
        status,
        Json(CadenceError {
            error: error.to_string(),
            reason_code: None,
            reason_message: None,
            can_force_generate: None,
        }),
    )
        .into_response()
}

fn refusal(refusal: CadenceRefusal) -> Response {
    match refusal {
        CadenceRefusal::Invalid(token) => refuse(StatusCode::BAD_REQUEST, token),
        CadenceRefusal::Blocked {
            reason_code,
            reason_message,
            can_force_generate,
        } => (
            StatusCode::BAD_REQUEST,
            Json(CadenceError {
                error: "nothing_to_send".into(),
                reason_code: Some(reason_code),
                reason_message: Some(reason_message),
                can_force_generate: Some(can_force_generate),
            }),
        )
            .into_response(),
    }
}

fn generation_failed(err: GenerationError) -> ApiResult<Response> {
    match err {
        GenerationError::Database(err) => Err(ApiError::Database(err)),
        GenerationError::Parse => Ok(refuse(StatusCode::BAD_GATEWAY, "ai_followup_parse_failed")),
        GenerationError::Ai(err) => {
            lambda_http::tracing::warn!(%err, "ai follow-up generation failed");
            Ok(refuse(StatusCode::BAD_GATEWAY, "generate_failed"))
        }
    }
}

fn checked_target(target: &Target) -> Result<CadenceTarget, &'static str> {
    let phone = target
        .phone_digits
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty());
    if phone.is_some_and(|p| !valid_thread_phone(p)) {
        return Err("invalid_phone");
    }
    Ok(CadenceTarget {
        customer_id: target.customer_id.filter(|id| *id > 0),
        phone_digits: phone.map(str::to_string),
    })
}

/// Preview an AI follow-up
///
/// Plans the follow-up for the customer and has the model write it. A
/// customer who should not be followed up now comes back as 400
/// `nothing_to_send` with the reason (`force_generate` overrides most).
#[utoipa::path(
    post,
    path = "/v1/followups/ai-cadence/preview",
    operation_id = "preview_ai_cadence",
    tag = "followups",
    request_body = PreviewRequest,
    responses(
        (status = 200, description = "The written follow-up", body = PreviewResult),
        (status = 400, description = "Nothing to send, or no way to reach the customer", body = CadenceError),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 502, description = "The model failed", body = CadenceError),
    ),
    security(("session_cookie" = []))
)]
pub async fn preview(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(request): JsonBody<PreviewRequest>,
) -> ApiResult<Response> {
    let target = match checked_target(&request.target) {
        Ok(target) => target,
        Err(token) => return Ok(refuse(StatusCode::BAD_REQUEST, token)),
    };
    let written =
        match ai_cadence::preview(&state, &user, &target, request.force_generate, Utc::now()).await
        {
            Ok(written) => written,
            Err(err) => return generation_failed(err),
        };
    Ok(match written {
        Ok(CadencePreview {
            channel,
            last_contact_at,
            cadence_mode,
            preview,
        }) => Json(PreviewResult {
            channel: channel.into(),
            last_contact_at,
            remaining_step_count: preview.len(),
            cadence_mode: cadence_mode.as_str().to_string(),
            preview: preview.iter().map(PreviewMessage::from).collect(),
        })
        .into_response(),
        Err(refused) => refusal(refused),
    })
}

/// `POST /v1/followups/ai-cadence/regenerate`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct RegenerateRequest {
    #[serde(flatten)]
    pub target: Target,
    pub channel: CadenceChannel,
    /// The message's goal.
    pub goal: String,
    /// Its wait, in minutes.
    pub delay_minutes: i64,
    /// Its wait, as shown.
    pub delay_label: String,
    /// The draft being replaced.
    pub avoid_body: String,
    /// Its subject (email).
    pub avoid_subject: Option<String>,
    /// Earlier drafts of the message already turned down (the last 8 count).
    #[serde(default)]
    pub avoid_previous: Vec<String>,
    /// The message's angle.
    pub angle: Option<String>,
    /// The message's phase.
    pub phase: Option<String>,
    /// Write it even though the customer should not be followed up now.
    #[serde(default)]
    pub force_generate: bool,
}

/// A rewritten message.
#[derive(Debug, Serialize, ToSchema)]
pub struct Regenerated {
    /// The new text.
    pub body: String,
    /// Email messages: the subject.
    pub subject: Option<String>,
}

/// Rewrite one message
///
/// Writes the message again, different from the drafts the rep turned down
/// (400 `regenerate_too_similar` when the model keeps repeating itself).
#[utoipa::path(
    post,
    path = "/v1/followups/ai-cadence/regenerate",
    operation_id = "regenerate_ai_cadence_message",
    tag = "followups",
    request_body = RegenerateRequest,
    responses(
        (status = 200, description = "The new draft", body = Regenerated),
        (status = 400, description = "Invalid, or nothing to send", body = CadenceError),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 502, description = "The model failed", body = CadenceError),
    ),
    security(("session_cookie" = []))
)]
pub async fn regenerate(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(request): JsonBody<RegenerateRequest>,
) -> ApiResult<Response> {
    let target = match checked_target(&request.target) {
        Ok(target) => target,
        Err(token) => return Ok(refuse(StatusCode::BAD_REQUEST, token)),
    };
    if request.goal.trim().is_empty()
        || request.avoid_body.trim().is_empty()
        || !(0..=AI_HOLD_MAX_DELAY_MINUTES).contains(&request.delay_minutes)
    {
        return Ok(refuse(StatusCode::BAD_REQUEST, "validation_failed"));
    }
    let previous: Vec<String> = request
        .avoid_previous
        .iter()
        .filter(|body| !body.trim().is_empty())
        .cloned()
        .collect();
    #[allow(clippy::cast_precision_loss)]
    let message = RegenerateMessage {
        delay_minutes: request.delay_minutes as f64,
        delay_label: request.delay_label.clone(),
        goal: request.goal.clone(),
        channel: request.channel.into(),
        angle: request.angle.clone().filter(|a| !a.trim().is_empty()),
        phase: request.phase.clone().filter(|p| !p.trim().is_empty()),
        avoid_body: request.avoid_body.clone(),
        avoid_subject: request.avoid_subject.clone(),
        avoid_previous: previous[previous.len().saturating_sub(8)..].to_vec(),
        force_generate: request.force_generate,
    };
    match ai_cadence::regenerate(&state, &user, &target, &message, Utc::now()).await {
        Ok(Ok((body, subject))) => Ok(Json(Regenerated { body, subject }).into_response()),
        Ok(Err(token)) => Ok(refuse(StatusCode::BAD_REQUEST, token)),
        Err(err) => generation_failed(err),
    }
}

/// Start or switch to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum StartIntent {
    /// 409 `flow_already_active` when a flow runs.
    Start,
    /// Cancel the running flow first.
    Switch,
}

/// `POST /v1/followups/ai-cadence`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct StartRequest {
    #[serde(flatten)]
    pub target: Target,
    pub intent: StartIntent,
    /// The approved messages.
    pub preview: Vec<PreviewMessage>,
    /// Start it even though the customer should not be followed up now.
    #[serde(default)]
    pub force_generate: bool,
}

async fn visible_flow(
    state: &AppState,
    user: &SessionUser,
    target: &CadenceTarget,
) -> ApiResult<FlowState> {
    let active_flow = if let Some(customer_id) = target.customer_id {
        enrollments::summary_for_customer(&state.pool, user, customer_id).await?
    } else if let Some(phone) = &target.phone_digits {
        let provider = company_sms_provider(state, user.company_id)
            .await?
            .unwrap_or(SmsProvider::Cloudtalk);
        let phone10 = enrollments::canonical_phone10(phone);
        enrollments::summary_for_phone(&state.pool, provider, user, &phone10).await?
    } else {
        None
    };
    Ok(FlowState { active_flow })
}

/// Start an AI follow-up
///
/// Saves the approved messages as the rep's "Generated with AI" flow and
/// starts it for the customer (`switch`: replacing the running one). A
/// first message due now is sent before the response.
#[utoipa::path(
    post,
    path = "/v1/followups/ai-cadence",
    operation_id = "start_ai_cadence",
    tag = "followups",
    request_body = StartRequest,
    responses(
        (status = 200, description = "The flow now running", body = FlowState),
        (status = 400, description = "Refused (e.g. `nothing_to_send`)", body = CadenceError),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Not the caller's flow to replace", body = CadenceError),
        (status = 409, description = "A flow is already running", body = CadenceError),
        (status = 429, description = "Over ten starts a minute", body = CadenceError),
    ),
    security(("session_cookie" = []))
)]
pub async fn start(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(request): JsonBody<StartRequest>,
) -> ApiResult<Response> {
    let target = match checked_target(&request.target) {
        Ok(target) => target,
        Err(token) => return Ok(refuse(StatusCode::BAD_REQUEST, token)),
    };
    let valid = !request.preview.is_empty()
        && request
            .preview
            .iter()
            .all(|m| (0..=AI_HOLD_MAX_DELAY_MINUTES).contains(&m.delay_minutes));
    if !valid {
        return Ok(refuse(StatusCode::BAD_REQUEST, "validation_failed"));
    }
    if !take_start_slot(&state, user.id) {
        return Ok(refuse(StatusCode::TOO_MANY_REQUESTS, "rate_limited"));
    }
    let preview: Vec<AiCadencePreviewMessage> =
        request.preview.iter().map(PreviewMessage::engine).collect();
    let switch = request.intent == StartIntent::Switch;
    let started = ai_cadence::start(
        &state,
        &user,
        &target,
        &preview,
        switch,
        request.force_generate,
        Utc::now(),
    )
    .await?;
    let send_now = match started {
        Ok(send_now) => send_now,
        Err(token) => {
            let status = match token {
                "flow_already_active" => StatusCode::CONFLICT,
                "forbidden" => StatusCode::FORBIDDEN,
                _ => StatusCode::BAD_REQUEST,
            };
            return Ok(refuse(status, token));
        }
    };
    let flow = visible_flow(&state, &user, &target).await?;
    if send_now && let Some(running) = &flow.active_flow {
        process_due_enrollment(&state, running.enrollment_id).await;
        return Ok(Json(visible_flow(&state, &user, &target).await?).into_response());
    }
    Ok(Json(flow).into_response())
}
