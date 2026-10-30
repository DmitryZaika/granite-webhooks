//! Scheduled jobs the `time-triggered` Lambda runs every few minutes. They
//! send `Authorization: <LAMBDA_KEY>` instead of a session.
//!
//! - `POST /v1/jobs/appointment-reminders` → due customer and rep reminders
//! - `POST /v1/jobs/maintenance-reminders` → the 9 AM shop maintenance digest
//! - `POST /v1/jobs/survey-notifications` → post-installation surveys
//! - `POST /v1/jobs/call-points` → outbound calls into call points
//! - `POST /v1/jobs/call-history` → `CloudTalk` and `RingCentral` calls into
//!   the saved call history
//! - `POST /v1/jobs/signature-reminders` → native e-signature expiry, reminders
//!   and notifications
//! - `POST /v1/jobs/sms-followups` → due flow steps, lead-list drip texts and
//!   scheduled emails and texts
//! - `POST /v1/jobs/supplier-files` → supplier price files into chunks
//! - `POST /v1/jobs/contact-sync` → customers into `RingCentral` and
//!   `CloudTalk` contacts, and `RingCentral` SMS subscriptions onto the
//!   current webhooks token
//! - `POST /v1/jobs/phone` → Granite Phone upkeep: sign-outs, recordings,
//!   AI notes, call points, old Telnyx callbacks
//! - `POST /v1/jobs/posthog-autofix` → new PostHog errors into fix runs
//! - `POST /v1/jobs/tendlc-checks` → the AI check of a 10DLC texting
//!   registration marked ready
//! - `POST /v1/jobs/promises` → what reps promised customers on calls, texts
//!   and emails, with the message that keeps each
//!
//! Replaces the Remix `api.*.process` cron routes.

mod appointment_reminders;
pub mod call_history;
pub mod call_points;
pub mod contact_sync;
pub mod maintenance_reminders;
pub mod phone;
pub mod posthog_autofix;
pub mod promises;
pub mod signature_reminders;
pub mod sms_followups;
pub mod supplier_files;
pub mod survey_notifications;
pub mod tendlc_checks;

use crate::openapi::{OpenApiRouter, routes};
use crate::state::AppState;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(appointment_reminders::run))
        .routes(routes!(maintenance_reminders::run))
        .routes(routes!(survey_notifications::run))
        .routes(routes!(call_points::run))
        .routes(routes!(call_history::run))
        .routes(routes!(signature_reminders::run))
        .routes(routes!(sms_followups::run))
        .routes(routes!(supplier_files::run))
        .routes(routes!(contact_sync::run))
        .routes(routes!(phone::run))
        .routes(routes!(posthog_autofix::run))
        .routes(routes!(tendlc_checks::run))
        .routes(routes!(promises::run))
}
