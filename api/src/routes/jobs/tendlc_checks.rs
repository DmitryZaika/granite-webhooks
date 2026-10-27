//! `POST /v1/jobs/tendlc-checks`: the AI check of 10DLC texting
//! registrations a Manager marked ready.
//!
//! See `crate::phone::tendlc_check`. A check reads the company's pages and
//! waits on a reasoning model for minutes, so the time-triggered Lambda
//! starts this every minute on the long-running deployment without waiting
//! for it; each run checks one registration, and overlapping runs take
//! different ones.

use crate::auth::job::JobCaller;
use crate::error::{ApiResult, ErrorBody};
use crate::openapi::ToSchema;
use crate::phone::tendlc_check;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use serde::Serialize;

/// What a run did.
#[derive(Debug, Default, Serialize, ToSchema)]
pub struct TenDlcChecksRun {
    /// Registrations checked (0 or 1).
    pub processed: usize,
}

/// Run the AI check of a texting registration (10DLC)
///
/// Takes the registration that has waited longest for its AI check (or one
/// whose check stopped midway, after 15 minutes), reads its quote form,
/// privacy policy, terms and website, and has the AI check them and the
/// form the way the carriers do. The Manager who marked it ready is
/// emailed what to change, or that it passed; one that passed is ready and
/// the superusers are told (bell and email). After 3 runs where the AI
/// could not answer it goes to the superusers unchecked.
#[crate::openapi::path(
    post,
    path = "/v1/jobs/tendlc-checks",
    operation_id = "run_tendlc_checks",
    tag = "jobs",
    responses(
        (status = 200, description = "How many registrations were checked", body = TenDlcChecksRun),
        (status = 401, description = "Missing or wrong job key", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("job_key" = []))
)]
pub async fn run(
    State(state): State<AppState>,
    _job: JobCaller,
) -> ApiResult<Json<TenDlcChecksRun>> {
    let run = tendlc_check::run_job(&state).await?;
    Ok(Json(TenDlcChecksRun {
        processed: run.checked,
    }))
}
