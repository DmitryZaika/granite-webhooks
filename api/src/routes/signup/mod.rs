//! Self-serve signup, public (no session): the plan catalog and the signup
//! steps of payments.granite-manager.com.
//!
//! - `GET /v1/public/plans` → catalog and a quote
//! - `POST /v1/public/signups` → start (emails a code)
//! - `GET /v1/public/signups/{id}`, `PUT .../plan`
//! - `POST .../code`, `POST .../verification` → the email code
//! - `GET .../consents` → the checkbox texts the server owns
//! - `POST .../payment-setup` → Stripe customer and SetupIntent
//! - `POST .../completion` → provisions the company and signs the owner in
//!
//! The signup id is a bearer secret. The flow is documented in
//! `crate::signup`.

mod handlers;
pub mod schemas;

use crate::state::AppState;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(handlers::plans))
        .routes(routes!(handlers::create))
        .routes(routes!(handlers::get_signup))
        .routes(routes!(handlers::update_plan))
        .routes(routes!(handlers::resend_code))
        .routes(routes!(handlers::verify_code))
        .routes(routes!(handlers::consents))
        .routes(routes!(handlers::payment_setup))
        .routes(routes!(handlers::complete))
}
