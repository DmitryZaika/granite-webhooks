//! `POST /v1/jobs/onboarding-commitments`: the daily onboarding call run.
//! See `crate::onboarding_call::job`.

use super::JobCaller;
use crate::error::{ApiResult, ErrorBody};
use crate::onboarding_call::job::{self, OnboardingRun};
use crate::state::AppState;
use axum::Json;
use axum::extract::State;

/// Run the onboarding commitments (reminders, tolling, fee notice and invoice)
///
/// Called by the `time-triggered` Lambda every minute with
/// `Authorization: <LAMBDA_KEY>`. First records today's slot check (once per
/// UTC day: did Dima's Calendly offer a bookable slot within the next 5
/// business days; a day without one moves every open deadline by a day).
/// Then takes up to 25 companies with an open onboarding commitment whose own
/// run is due (every 23 hours) and starts no new company after 15 seconds.
/// Per company: refreshes the booked call from Calendly (rescheduled,
/// cancelled, no-show) or finds one booked outside the app, completes the
/// commitment 48 hours after the call, suspends or resumes it on a
/// cancellation request, voids it when the subscription ended, emails the
/// day 7, 21 and 27 reminders and the fee notice to the company's Managers
/// (each once), and creates the onboarding fee invoice 7 days after the
/// notice (only with `ONBOARDING_FEE_CHARGING=1`; otherwise the fee is
/// skipped for good). Emails are skipped while the mail service is not
/// configured. Returns the counts.
#[utoipa::path(
    post,
    path = "/v1/jobs/onboarding-commitments",
    operation_id = "run_onboarding_commitments",
    tag = "jobs",
    responses(
        (status = 200, description = "What the run did", body = OnboardingRun),
        (status = 401, description = "Missing or wrong job key", body = ErrorBody),
    ),
    security(("job_key" = []))
)]
pub async fn run(
    State(state): State<AppState>,
    _caller: JobCaller,
) -> ApiResult<Json<OnboardingRun>> {
    Ok(Json(job::run_at(&state, crate::billing::now()).await))
}
