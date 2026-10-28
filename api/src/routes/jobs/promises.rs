//! `POST /v1/jobs/promises`: what reps promised customers on calls, texts
//! and emails, with the message that keeps each (see `crate::promises`).
//!
//! A run transcribes calls and waits on the AI, so the time-triggered Lambda
//! starts this every minute on the long-running deployment without waiting
//! for it; overlapping runs take different calls and messages.

use crate::auth::job::JobCaller;
use crate::error::{ApiResult, ErrorBody};
use crate::openapi::ToSchema;
use crate::promises;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use serde::Serialize;

/// What a run did.
#[derive(Debug, Default, Serialize, ToSchema)]
pub struct PromisesRun {
    /// Calls, texts and emails read.
    pub processed: usize,
    /// Calls, texts and emails newly queued to read.
    pub queued: u64,
    /// Promises and quote changes saved.
    pub promises: usize,
    /// Due promises the reps were reminded of.
    pub reminded: usize,
    /// Morning lists sent.
    pub digests: usize,
}

/// Read reps' calls and messages for promises
///
/// For the reps AI promises is on for: reminds them of promises that came
/// due, sends their morning list (once a day from 8 AM their time), queues
/// their calls, texts and emails of the last 3 days not read yet, then reads
/// up to 4 (a call is transcribed first): saves what the rep promised the
/// customer and by when, closes the open promises it keeps, saves the quote
/// changes the customer asked for, and writes the text or email that keeps
/// each promise a message can keep, in the rep's own style.
#[crate::openapi::path(
    post,
    path = "/v1/jobs/promises",
    operation_id = "run_promises",
    tag = "jobs",
    responses(
        (status = 200, description = "What the run did", body = PromisesRun),
        (status = 401, description = "Missing or wrong job key", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("job_key" = []))
)]
pub async fn run(State(state): State<AppState>, _job: JobCaller) -> ApiResult<Json<PromisesRun>> {
    let run = promises::run_job(&state).await?;
    Ok(Json(PromisesRun {
        processed: run.read,
        queued: run.queued,
        promises: run.promises,
        reminded: run.reminded,
        digests: run.digests,
    }))
}
