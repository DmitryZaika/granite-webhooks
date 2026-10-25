//! `POST /v1/jobs/supplier-files`: processes waiting supplier price files.
//!
//! See `crate::supplier_file_processor`. Replaces the Remix server's
//! one-minute sweep. A big PDF takes minutes, so the time-triggered Lambda
//! should call this on the long-running (Function URL) deployment.

use super::JobCaller;
use crate::error::{ApiResult, ErrorBody};
use crate::openapi::ToSchema;
use crate::state::AppState;
use crate::supplier_file_processor::{LiveExtractor, reclaim_stale, run_once};
use axum::Json;
use axum::extract::State;
use serde::Serialize;
use std::time::{Duration, Instant};

/// Stop claiming new files after this long, so a run ends well before a
/// 15-minute Lambda timeout.
const TIME_BUDGET: Duration = Duration::from_mins(10);

/// What a run did.
#[derive(Debug, Default, Serialize, ToSchema)]
pub struct SupplierFilesRun {
    /// Files claimed and processed (ready or failed).
    pub processed: u32,
}

/// Process waiting supplier price files
///
/// Returns files stuck in processing to the queue, then processes waiting
/// files (newest first) until none is left or ten minutes have passed.
#[crate::openapi::path(
    post,
    path = "/v1/jobs/supplier-files",
    operation_id = "run_supplier_files",
    tag = "jobs",
    responses(
        (status = 200, description = "How many files were processed", body = SupplierFilesRun),
        (status = 401, description = "Missing or wrong job key", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("job_key" = []))
)]
pub async fn run(
    State(state): State<AppState>,
    _job: JobCaller,
) -> ApiResult<Json<SupplierFilesRun>> {
    reclaim_stale(&state.pool).await?;
    let extractor = LiveExtractor { state: &state };
    let started = Instant::now();
    let mut run = SupplierFilesRun::default();
    while started.elapsed() < TIME_BUDGET && run_once(&state.pool, &extractor).await? {
        run.processed += 1;
    }
    Ok(Json(run))
}
