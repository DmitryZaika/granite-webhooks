//! `GET /v1/public/calendly-url`: where the marketing pages book a demo.
//!
//! `CALENDLY_DEMO_URL`, else the account's first active event type
//! (`CALENDLY_API_TOKEN`), else the account's own page, else the Granite
//! Manager demo page. Replaces the Remix `api.calendly.scheduling-url` route.

use crate::onboarding_call::calendly::scheduling_url;
use crate::openapi::ToSchema;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use serde::Serialize;
use std::sync::Mutex;

const DEFAULT_DEMO_URL: &str = "https://calendly.com/granite-manager/demo";
static RESOLVED: Mutex<Option<String>> = Mutex::new(None);

/// `GET /v1/public/calendly-url`.
#[derive(Debug, Serialize, ToSchema)]
pub struct CalendlyUrl {
    /// The demo booking page.
    pub url: Option<String>,
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
    let fetched = state.onboarding_call.calendly.demo_page().await;
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
