//! `POST /v1/jobs/posthog-autofix`: one tick of the PostHog auto-fix intake.
//!
//! See `crate::posthog_autofix`.

use crate::auth::job::JobCaller;
use crate::error::{ApiResult, ErrorBody};
use crate::posthog_autofix::Autofix;
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
/// poll window, `POSTHOG_AUTOFIX_POLL_MINUTES`) reads the PostHog
/// `$exception` events since the last read (never before the cutover) and
/// records them as bugs, one per signature whatever issue they are in;
/// emails one digest when new bugs arrive in a storm; judges bugs that have
/// settled (skipped as noise, a browser 5xx folded into the API error of the
/// same route, or queued); emails when a fixed issue is reopened; and (with
/// `POSTHOG_AUTOFIX_WRITE_KEY` and `_ASSIGNEE` set) assigns the issues of
/// opened fixes in PostHog. Last, in live mode, it starts queued fix runs
/// (bugs seen in the last day first, then the most widespread) within the
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
    let autofix = Autofix::of(&state);
    let config = autofix.config().await;
    let config = &*config;
    let http = &autofix.http;
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
