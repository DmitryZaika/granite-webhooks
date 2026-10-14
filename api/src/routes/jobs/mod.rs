//! Scheduled jobs the `time-triggered` Lambda runs every few minutes. They
//! send `Authorization: <LAMBDA_KEY>` instead of a session.
//!
//! - `POST /v1/jobs/appointment-reminders` → due customer and rep reminders
//! - `POST /v1/jobs/maintenance-reminders` → the 9 AM shop maintenance digest
//! - `POST /v1/jobs/survey-notifications` → post-installation surveys
//! - `POST /v1/jobs/call-points` → outbound calls into call points
//! - `POST /v1/jobs/signature-reminders` → native e-signature expiry, reminders
//!   and notifications
//! - `POST /v1/jobs/sms-followups` → due flow steps, lead-list drip texts and
//!   scheduled emails and texts
//! - `POST /v1/jobs/supplier-files` → supplier price files into chunks
//!
//! Replaces the Remix `api.*.process` cron routes.

mod appointment_reminders;
pub mod call_points;
pub mod maintenance_reminders;
pub mod signature_reminders;
pub mod sms_followups;
pub mod supplier_files;
pub mod survey_notifications;

use crate::error::ApiError;
use crate::state::AppState;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(appointment_reminders::run))
        .routes(routes!(maintenance_reminders::run))
        .routes(routes!(survey_notifications::run))
        .routes(routes!(call_points::run))
        .routes(routes!(signature_reminders::run))
        .routes(routes!(sms_followups::run))
        .routes(routes!(supplier_files::run))
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
