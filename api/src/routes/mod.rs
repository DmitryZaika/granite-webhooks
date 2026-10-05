//! One module per domain. Each module exposes `router()` with its full paths
//! (`/v1/<domain>/...`) so every route is greppable from its URL.

pub mod customers;
pub mod me;

use crate::state::AppState;
use axum::Router;

pub fn router() -> Router<AppState> {
    Router::new().merge(me::router()).merge(customers::router())
}
