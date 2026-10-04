//! The signed-in user.
//!
//! - `GET /v1/me`           → effective session user (no extra query beyond auth)
//! - `GET /v1/me/positions` → position ids in the company the request acts in

mod handlers;
pub mod queries;

use crate::state::AppState;
use axum::{Router, routing::get};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v1/me", get(handlers::me))
        .route("/v1/me/positions", get(handlers::positions))
}
