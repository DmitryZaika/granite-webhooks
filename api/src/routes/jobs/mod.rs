//! Scheduled jobs the `time-triggered` Lambda runs every few minutes. They
//! send `Authorization: <LAMBDA_KEY>` instead of a session.
//!
//! - `POST /v1/jobs/appointment-reminders` → due customer and rep reminders
//!
//! Replaces the Remix `api.*.process` cron routes.

mod appointment_reminders;

use crate::error::ApiError;
use crate::state::AppState;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(appointment_reminders::run))
}

/// The `time-triggered` Lambda, by its key.
pub struct JobCaller;

impl FromRequestParts<AppState> for JobCaller {
    type Rejection = ApiError;

    #[allow(clippy::unused_async_trait_impl)] // the trait's signature
    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let sent = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok());
        match (&state.job_key, sent) {
            (Some(key), Some(sent)) if constant_time_eq(key.as_bytes(), sent.as_bytes()) => {
                Ok(Self)
            }
            _ => Err(ApiError::Unauthorized),
        }
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
