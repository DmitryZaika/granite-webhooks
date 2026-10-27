//! The `time-triggered` Lambda (scheduled jobs) and the other Lambdas that
//! call the API with `Authorization: <LAMBDA_KEY>` instead of a session.

use super::constant_time_eq;
use crate::error::ApiError;
use crate::state::AppState;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;

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
