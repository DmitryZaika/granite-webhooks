//! One module per domain.
//!
//! Each module exposes `router()`, registering every handler with `routes!`
//! so it lands in the OpenAPI spec too. The full path
//! (`/v1/<domain>/...`) lives in the handler's `#[utoipa::path]`, so every
//! route is still greppable from its URL.

pub mod customers;
pub mod faucets;
pub mod me;
pub mod sinks;
pub mod stones;
pub mod users;

use crate::state::AppState;
use utoipa_axum::router::OpenApiRouter;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .merge(me::router())
        .merge(customers::router())
        .merge(users::router())
        .merge(stones::router())
        .merge(sinks::router())
        .merge(faucets::router())
}
