//! `POST /v1/jobs/posthog-autofix`: one tick of the PostHog auto-fix intake.
//!
//! See `crate::posthog_autofix`.

use super::JobCaller;
use crate::error::{ApiResult, ErrorBody};
use crate::posthog_autofix::github::GitHub;
use crate::posthog_autofix::intake::{Deps, TickReport, tick};
use crate::posthog_autofix::posthog::PostHog;
use crate::posthog_autofix::writeback::PostHogWriter;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use chrono::Utc;

/// Run one tick of the PostHog auto-fix intake
///
/// Gives up on fix runs that never reported back, then (at most once per
/// poll window, `POSTHOG_AUTOFIX_POLL_MINUTES`) records new PostHog error
/// issues created since the cutover, emails one digest when they arrive in a
/// storm, judges issues that have settled (skipped as noise, folded into an
/// earlier run of the same bug, or queued), emails when a fixed issue is
/// reopened, and (with `POSTHOG_AUTOFIX_WRITE_KEY` and `_ASSIGNEE` set)
/// assigns the issues of opened fixes in PostHog. Last, in live mode, it starts queued fix runs within the
/// concurrency and daily caps; in dry-run mode it emails the runs it would
/// have started instead. With `POSTHOG_AUTOFIX_MODE` off it does nothing.
/// When another tick is running it returns at once with `busy`. PostHog or
/// GitHub failures do not fail the call: they are listed in `errors` and
/// retried on the next tick.
#[crate::openapi::path(
    post,
    path = "/v1/jobs/posthog-autofix",
    operation_id = "run_posthog_autofix",
    tag = "jobs",
    responses(
        (status = 200, description = "What the tick did", body = TickReport),
        (status = 401, description = "Missing or wrong job key", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("job_key" = []))
)]
pub async fn run(State(state): State<AppState>, _job: JobCaller) -> ApiResult<Json<TickReport>> {
    let config = state.autofix.config().await;
    let config = &*config;
    let http = &state.autofix.http;
    let deps = Deps {
        pool: &state.pool,
        config,
        source: &PostHog { config, http },
        dispatcher: &GitHub { config, http },
        mailer: &state.webhooks,
        writer: &PostHogWriter { config, http },
    };
    Ok(Json(tick(&deps, Utc::now()).await?))
}
