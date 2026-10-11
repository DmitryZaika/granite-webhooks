//! Request extractors shared by routes.

use crate::error::ApiError;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use serde::de::DeserializeOwned;

/// Query string parser that, unlike `axum::extract::Query`, accepts repeated
/// keys as a list (`?type=granite&type=quartz` into `Vec<String>`) and
/// rejects malformed input with the API's JSON 400 body.
pub struct MultiQuery<T>(pub T);

impl<T, S> FromRequestParts<S> for MultiQuery<T>
where
    T: DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = ApiError;

    fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> impl Future<Output = Result<Self, ApiError>> + Send {
        let parsed = serde_html_form::from_str(parts.uri.query().unwrap_or_default())
            .map(Self)
            .map_err(|err| ApiError::BadRequest(format!("invalid query string: {err}")));
        std::future::ready(parsed)
    }
}
