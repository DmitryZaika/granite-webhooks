//! `GET /v1/public/calendly-url`: where the marketing pages book a demo.
//!
//! `CALENDLY_DEMO_URL`, else the account's first active event type
//! (`CALENDLY_API_TOKEN`), else the Granite Manager demo page. Replaces the
//! Remix `api.calendly.scheduling-url` route.

use crate::openapi::ToSchema;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use serde::Serialize;
use serde_json::Value;
use std::sync::Mutex;

const DEFAULT_DEMO_URL: &str = "https://calendly.com/granite-manager/demo";
static RESOLVED: Mutex<Option<String>> = Mutex::new(None);

/// `GET /v1/public/calendly-url`.
#[derive(Debug, Serialize, ToSchema)]
pub struct CalendlyUrl {
    /// The demo booking page.
    pub url: Option<String>,
}

/// A Calendly page URL without query or trailing slash, or `None`.
pub fn scheduling_url(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let rest = trimmed
        .strip_prefix("https://")
        .map(|rest| ("https://", rest))
        .or_else(|| {
            trimmed
                .strip_prefix("http://")
                .map(|rest| ("http://", rest))
        })?;
    let (scheme, rest) = rest;
    let end = rest.find(['?', '#']).unwrap_or(rest.len());
    let rest = &rest[..end];
    let (host, path) = rest
        .split_once('/')
        .map_or((rest, ""), |(host, path)| (host, path));
    let host = host.to_lowercase();
    if host != "calendly.com" && !host.ends_with(".calendly.com") {
        return None;
    }
    let url = format!("{scheme}{host}/{path}");
    Some(url.trim_end_matches('/').to_string())
}

async fn from_api(client: &reqwest::Client, token: &str) -> Option<String> {
    let get = |url: String| async move {
        client
            .get(url)
            .bearer_auth(token)
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?
            .json::<Value>()
            .await
            .ok()
    };
    let me = get("https://api.calendly.com/users/me".into()).await?;
    let own = me["resource"]["scheduling_url"]
        .as_str()
        .and_then(scheduling_url);
    let Some(uri) = me["resource"]["uri"].as_str() else {
        return own;
    };
    let encoded = percent_encoding::utf8_percent_encode(uri, percent_encoding::NON_ALPHANUMERIC);
    let Some(types) = get(format!(
        "https://api.calendly.com/event_types?user={encoded}&active=true&count=20"
    ))
    .await
    else {
        return own;
    };
    types["collection"]
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|kind| kind["scheduling_url"].as_str().and_then(scheduling_url))
        .or(own)
}

/// Get the demo booking page
///
/// The Calendly page the marketing site opens to book a demo.
#[crate::openapi::path(
    get,
    path = "/v1/public/calendly-url",
    operation_id = "get_calendly_url",
    tag = "public",
    responses((status = 200, description = "Booking page", body = CalendlyUrl))
)]
pub async fn calendly_url(State(state): State<AppState>) -> Json<CalendlyUrl> {
    if let Some(url) = std::env::var("CALENDLY_DEMO_URL")
        .ok()
        .as_deref()
        .and_then(scheduling_url)
    {
        return Json(CalendlyUrl { url: Some(url) });
    }
    let cached = RESOLVED.lock().ok().and_then(|cached| cached.clone());
    if let Some(url) = cached {
        return Json(CalendlyUrl { url: Some(url) });
    }
    let token = std::env::var("CALENDLY_API_TOKEN").unwrap_or_default();
    let fetched = if token.trim().is_empty() {
        None
    } else {
        from_api(&state.telephony_http, token.trim()).await
    };
    if let (Some(url), Ok(mut cached)) = (&fetched, RESOLVED.lock()) {
        *cached = Some(url.clone());
    }
    Json(CalendlyUrl {
        url: Some(fetched.unwrap_or_else(|| DEFAULT_DEMO_URL.to_string())),
    })
}

#[cfg(test)]
mod tests {
    use super::scheduling_url;

    #[test]
    fn only_calendly_pages_are_kept() {
        assert_eq!(
            scheduling_url(" https://calendly.com/acme/demo/?utm=x ").as_deref(),
            Some("https://calendly.com/acme/demo")
        );
        assert_eq!(
            scheduling_url("https://team.Calendly.com/x").as_deref(),
            Some("https://team.calendly.com/x")
        );
        assert_eq!(scheduling_url("https://evil.com/calendly.com"), None);
        assert_eq!(scheduling_url("calendly.com/x"), None);
    }
}
