//! `POST /v1/jobs/subscription-maintenance`: self-serve signups, plan invite
//! acceptances and subscription seats. See `crate::signup::maintenance`.

use crate::auth::job::JobCaller;
use crate::error::{ApiResult, ErrorBody};
use crate::signup::maintenance::{self, MaintenanceRun};
use crate::state::AppState;
use axum::Json;
use axum::extract::State;

/// Resume stuck signups and plan moves, expire abandoned signups and reconcile seats
///
/// One run, which starts no new work after 20 seconds:
/// 1. Resumes up to 5 signups stuck in provisioning for 10 minutes or more
///    (oldest first; a signup is given up after 10 failed runs).
/// 2. Resumes up to 5 plan invites whose acceptance stopped before the
///    company switched to its plan, with the same wait; after 26 hours staff
///    are alerted instead.
/// 3. Expires up to 50 abandoned signups (24 hours after they started, or 11
///    days waiting for bank verification) and deletes their Stripe customers.
/// 4. Once every 23 hours, pushes each subscription company's extra-user
///    quantity to Stripe where it differs from its billable users.
///
/// Returns the counts; `processed` is `resumed` + `invites_resumed` +
/// `expired`.
#[crate::openapi::path(
    post,
    path = "/v1/jobs/subscription-maintenance",
    operation_id = "run_subscription_maintenance",
    tag = "jobs",
    responses(
        (status = 200, description = "What the run did", body = MaintenanceRun),
        (status = 401, description = "Missing or wrong job key", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("job_key" = []))
)]
pub async fn run(
    State(state): State<AppState>,
    _caller: JobCaller,
) -> ApiResult<Json<MaintenanceRun>> {
    Ok(Json(maintenance::run(&state).await?))
}
