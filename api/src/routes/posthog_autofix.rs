//! The PostHog auto-fix workflow's callbacks. They send
//! `Authorization: Bearer <POSTHOG_AUTOFIX_CALLBACK_SECRET>` instead of a session.
//!
//! - `POST /v1/posthog-autofix/runs/{id}/started` → the workflow run began
//! - `POST /v1/posthog-autofix/runs/{id}/result` → the workflow run finished
//!
//! The ledger and the state machine are in `crate::posthog_autofix`.

use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::posthog_autofix::callback::{
    Applied, CallbackError, authorized, record_result, record_started,
};
use crate::posthog_autofix::model::{RunReport, StartedReport};
use crate::state::AppState;
use axum::Json;
use axum::extract::{FromRequestParts, Path, State};
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use chrono::Utc;
use serde::Serialize;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(started))
        .routes(routes!(result))
}

/// What a callback did.
#[derive(Debug, Serialize, ToSchema)]
pub struct CallbackApplied {
    /// True when this call moved the run; false when the same report had
    /// already been recorded (a retry), which is also a success.
    pub changed: bool,
}

impl From<Applied> for CallbackApplied {
    fn from(applied: Applied) -> Self {
        Self {
            changed: applied == Applied::Changed,
        }
    }
}

/// Record that a fix run started
///
/// Called once by the `posthog-autofix` GitHub workflow when it begins,
/// before the agent runs. Moves the run from `dispatched` to `running` and
/// stores the GitHub run link. `issue_id` must be the run's PostHog issue id.
/// Repeating the same call (same `github_run_url`) answers 200 with
/// `changed: false`. Any other status (for example a run the intake never
/// dispatched) answers 409, and the workflow must stop. Uses the callback
/// secret, not a session.
#[utoipa::path(
    post,
    path = "/v1/posthog-autofix/runs/{id}/started",
    operation_id = "start_posthog_autofix_run",
    tag = "posthog-autofix",
    params(("id" = u64, Path, description = "Ledger run id, passed to the workflow as an input")),
    request_body = StartedReport,
    responses(
        (status = 200, description = "Recorded, or already recorded", body = CallbackApplied),
        (status = 400, description = "Malformed body, issue id or GitHub link", body = ErrorBody),
        (status = 401, description = "Missing or wrong callback secret", body = ErrorBody),
        (status = 404, description = "No such run, or its issue id differs", body = ErrorBody),
        (status = 409, description = "The run is not dispatched; the workflow must stop", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("autofix_callback" = []))
)]
pub async fn started(
    State(state): State<AppState>,
    _caller: Workflow,
    Path(id): Path<u64>,
    JsonBody(report): JsonBody<StartedReport>,
) -> ApiResult<Json<CallbackApplied>> {
    record_started(&state.pool, id, &report, Utc::now())
        .await
        .map(|applied| Json(applied.into()))
        .map_err(api_error)
}

/// Record a fix run's result
///
/// Called once by the `posthog-autofix` GitHub workflow when it ends. Closes
/// the run as `succeeded` (`conclusion` completed, with the agent's answer in
/// `agent`) or `failed`, stores the outcome, pull request, verification and
/// cost, and emails the developers. A `usage_limited` conclusion puts the run
/// back in the queue to be retried in an hour, with no email, until its
/// attempts run out; then it fails. A late result for a run the watchdog
/// timed out is still recorded. Repeating a result answers 200 with
/// `changed: false` and sends no second email. A result for a run that never
/// started answers 409. Uses the callback secret, not a session.
#[utoipa::path(
    post,
    path = "/v1/posthog-autofix/runs/{id}/result",
    operation_id = "finish_posthog_autofix_run",
    tag = "posthog-autofix",
    params(("id" = u64, Path, description = "Ledger run id, passed to the workflow as an input")),
    request_body = RunReport,
    responses(
        (status = 200, description = "Recorded, or already recorded", body = CallbackApplied),
        (status = 400, description = "Malformed body, issue id or link, or a completed run without `agent`", body = ErrorBody),
        (status = 401, description = "Missing or wrong callback secret", body = ErrorBody),
        (status = 404, description = "No such run, or its issue id differs", body = ErrorBody),
        (status = 409, description = "The run is in a status that cannot take a result", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("autofix_callback" = []))
)]
pub async fn result(
    State(state): State<AppState>,
    _caller: Workflow,
    Path(id): Path<u64>,
    JsonBody(report): JsonBody<RunReport>,
) -> ApiResult<Json<CallbackApplied>> {
    record_result(
        &state.pool,
        &state.webhooks,
        &*state.autofix.config().await,
        id,
        &report,
        Utc::now(),
    )
    .await
    .map(|applied| Json(applied.into()))
    .map_err(api_error)
}

/// The `posthog-autofix` workflow, by its callback secret. Checked from the
/// request parts, so a caller without the secret learns nothing from the body
/// validation.
pub struct Workflow;

impl FromRequestParts<AppState> for Workflow {
    type Rejection = ApiError;

    #[allow(clippy::unused_async_trait_impl)] // the trait's signature
    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let sent = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok());
        if authorized(&*state.autofix.config().await, sent) {
            Ok(Self)
        } else {
            Err(ApiError::Unauthorized)
        }
    }
}

fn api_error(error: CallbackError) -> ApiError {
    match error {
        CallbackError::Invalid(message) => ApiError::BadRequest(message),
        CallbackError::NotFound => ApiError::NotFound,
        CallbackError::Conflict(status) => {
            ApiError::Conflict(format!("run is {status}, not waiting for this callback"))
        }
        CallbackError::Database(error) => ApiError::Database(error),
    }
}
