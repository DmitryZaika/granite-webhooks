//! `/ingest/*`: the browser's PostHog traffic, relayed so ad blockers that
//! block PostHog's hosts still let it through. Port of the Remix
//! `ingest.$` route. Not part of the API spec: it is PostHog's protocol.

use crate::state::AppState;
use axum::body::{Body, Bytes};
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};

const API_HOST: &str = "https://us.i.posthog.com";
const ASSET_HOST: &str = "https://us-assets.i.posthog.com";

/// Request headers not passed on: the app's session cookie never goes to
/// PostHog, and an encoded answer would come back mislabelled.
const DROPPED_REQUEST: [header::HeaderName; 4] = [
    header::COOKIE,
    header::HOST,
    header::ACCEPT_ENCODING,
    header::CONTENT_LENGTH,
];

const DROPPED_RESPONSE: [header::HeaderName; 5] = [
    header::CONTENT_ENCODING,
    header::CONTENT_LENGTH,
    header::TRANSFER_ENCODING,
    header::SET_COOKIE,
    header::CONNECTION,
];

/// The PostHog URL for `/ingest/<rest>?<query>`.
pub fn upstream_url(rest: &str, query: Option<&str>) -> String {
    let host = if rest.starts_with("static/") {
        ASSET_HOST
    } else {
        API_HOST
    };
    query.filter(|q| !q.is_empty()).map_or_else(
        || format!("{host}/{rest}"),
        |query| format!("{host}/{rest}?{query}"),
    )
}

pub async fn relay(
    State(state): State<AppState>,
    method: Method,
    Path(rest): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let url = upstream_url(&rest, query.as_deref());
    let mut request = state.telephony_http.request(method.clone(), url);
    for (name, value) in &headers {
        if !DROPPED_REQUEST.contains(name) {
            request = request.header(name, value);
        }
    }
    if method != Method::GET && method != Method::HEAD {
        request = request.body(body);
    }
    let Ok(response) = request
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
    else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    let status = response.status();
    let mut out = HeaderMap::new();
    for (name, value) in response.headers() {
        if !DROPPED_RESPONSE.contains(name) {
            out.append(name, value.clone());
        }
    }
    let bytes = response.bytes().await.unwrap_or_default();
    (status, out, Body::from(bytes)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_files_come_from_the_asset_host() {
        assert_eq!(
            upstream_url("static/array.js", None),
            "https://us-assets.i.posthog.com/static/array.js"
        );
        assert_eq!(
            upstream_url("e/", Some("ip=1&_=2")),
            "https://us.i.posthog.com/e/?ip=1&_=2"
        );
        assert_eq!(
            upstream_url("decide/", Some("")),
            "https://us.i.posthog.com/decide/"
        );
    }
}
