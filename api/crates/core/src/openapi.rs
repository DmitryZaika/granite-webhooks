//! The route macros, used by every api crate.
//!
//! Paths and schemas of the OpenAPI document (`api::openapi`) come from the
//! `#[crate::openapi::path]` and `ToSchema` annotations on each route.
//!
//! utoipa only runs with the `openapi` feature (`make api-openapi` and the
//! spec tests): it is a large share of the crates' compile time. Without it
//! the annotations expand to nothing but each route's method and path, and
//! [`OpenApiRouter`] registers routes straight with axum.

pub use api_macros::path;
#[cfg(not(feature = "openapi"))]
pub use api_macros::{IntoParams, ToSchema, routes};
#[cfg(not(feature = "openapi"))]
pub use plain::OpenApiRouter;
#[cfg(feature = "openapi")]
pub use utoipa::{IntoParams, ToSchema};
#[cfg(feature = "openapi")]
pub use utoipa_axum::{router::OpenApiRouter, routes};

/// Name of the `__session` cookie scheme every route lists under `security`.
pub const SESSION_SECURITY: &str = "session_cookie";

/// Name of the Granite Phone app's `X-Granite-Device` token scheme.
pub const DEVICE_SECURITY: &str = "device_token";

#[cfg(not(feature = "openapi"))]
mod plain {
    use axum::Router;
    use axum::extract::Request;
    use axum::response::IntoResponse;
    use axum::routing::{MethodRouter, Route};
    use std::convert::Infallible;
    use tower_layer::Layer;
    use tower_service::Service;

    /// `utoipa_axum`'s `OpenApiRouter` without the document: the routes of
    /// [`api_macros::routes!`] go straight to axum.
    pub struct OpenApiRouter<S = ()>(Router<S>);

    impl<S: Clone + Send + Sync + 'static> OpenApiRouter<S> {
        pub fn new() -> Self {
            Self(Router::new())
        }

        /// Registers the handlers of one `routes!`, which share a path.
        #[must_use]
        pub fn routes<const N: usize>(self, routes: [(&'static str, MethodRouter<S>); N]) -> Self {
            debug_assert!(
                routes.iter().all(|(path, _)| *path == routes[0].0),
                "the handlers of one routes! must share a path"
            );
            Self(
                routes
                    .into_iter()
                    .fold(self.0, |axum_router, (path, method_router)| {
                        axum_router.route(if path.is_empty() { "/" } else { path }, method_router)
                    }),
            )
        }

        #[must_use]
        pub fn merge(self, other: Self) -> Self {
            Self(self.0.merge(other.0))
        }

        #[must_use]
        pub fn layer<L>(self, layer: L) -> Self
        where
            L: Layer<Route> + Clone + Send + Sync + 'static,
            L::Service: Service<Request> + Clone + Send + Sync + 'static,
            <L::Service as Service<Request>>::Response: IntoResponse + 'static,
            <L::Service as Service<Request>>::Error: Into<Infallible> + 'static,
            <L::Service as Service<Request>>::Future: Send + 'static,
        {
            Self(self.0.layer(layer))
        }

        pub fn into_router(self) -> Router<S> {
            self.0
        }
    }

    impl<S: Clone + Send + Sync + 'static> Default for OpenApiRouter<S> {
        fn default() -> Self {
            Self::new()
        }
    }
}
