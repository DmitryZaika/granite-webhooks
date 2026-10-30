//! Dima's Calendly (API v2): the onboarding booking page, a booked call's
//! invitee and event, bookings made outside the widget, the daily slot check
//! and the marketing demo page.
//!
//! Every request goes to `CALENDLY_BASE_URL` (default
//! `https://api.calendly.com`) with `CALENDLY_API_TOKEN`. A URI that came
//! from a browser or a stored row is accepted only as
//! `https://api.calendly.com/<path>` or `<base>/<path>`, and only `<base>` +
//! its path is requested, so no request ever goes to another host. Errors
//! carry the status code only.

use super::commitment::Booking;
use super::{UTM_CAMPAIGN, utm_content};
use chrono::{DateTime, NaiveDateTime, SubsecRound};
use lambda_http::tracing;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const API_BASE: &str = "https://api.calendly.com";
const TIMEOUT: Duration = Duration::from_secs(10);
/// How long the account (`users/me`) is kept in memory; failures are not kept.
const ACCOUNT_CACHE: Duration = Duration::from_secs(3600);
/// Event types the slot check asks about, at most.
pub const MAX_SLOT_EVENT_TYPES: usize = 5;
/// Longest id in a booking URI (Calendly's are UUIDs).
const MAX_ID_LEN: usize = 64;
/// Longest API path accepted (the URI columns hold 255 characters).
const MAX_PATH_LEN: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CalendlyError {
    #[error("Calendly is not configured")]
    NotConfigured,
    #[error("not a Calendly API URI")]
    BadUri,
    #[error("Calendly answered {0}")]
    Status(u16),
    #[error("Calendly could not be reached")]
    Request,
    #[error("Calendly sent an unexpected answer")]
    Invalid,
    /// Not asked: the run's time or its calls for this company are used up.
    #[error("the run's Calendly budget is spent")]
    Budget,
}

impl CalendlyError {
    /// Safe to log and store: `http_404`, `request_failed`, ...
    pub fn code(&self) -> String {
        match self {
            Self::NotConfigured => "not_configured".into(),
            Self::BadUri => "bad_uri".into(),
            Self::Status(status) => format!("http_{status}"),
            Self::Request => "request_failed".into(),
            Self::Invalid => "invalid_response".into(),
            Self::Budget => "budget_spent".into(),
        }
    }
}

/// The Calendly account the token belongs to (`users/me`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub uri: String,
    /// The account's own booking page.
    pub scheduling_url: Option<String>,
}

/// An active event type of the account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventType {
    pub uri: String,
    pub scheduling_url: Option<String>,
}

/// Who cancelled, and when.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancellation {
    /// `canceler_type` is `host`: Granite cancelled.
    pub by_host: bool,
    pub at: Option<NaiveDateTime>,
}

/// One invitee of a scheduled event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invitee {
    pub uri: String,
    /// `status` is `active` (not `canceled`).
    pub active: bool,
    pub email: String,
    pub name: Option<String>,
    pub utm_campaign: Option<String>,
    pub utm_content: Option<String>,
    pub reschedule_url: Option<String>,
    pub cancel_url: Option<String>,
    /// Cancelled by a reschedule; `new_invitee` is the new booking.
    pub rescheduled: bool,
    pub new_invitee: Option<String>,
    pub cancellation: Option<Cancellation>,
    /// When the host marked the invitee a no-show.
    pub no_show_at: Option<NaiveDateTime>,
    /// The scheduled event's URI.
    pub event_uri: String,
    pub created_at: Option<NaiveDateTime>,
}

impl Invitee {
    /// Booked through our widget or our links for this company:
    /// `utm_campaign=granite-onboarding` and `utm_content=company-<id>`.
    pub fn carries_tag(&self, company_id: i32) -> bool {
        self.utm_campaign.as_deref() == Some(UTM_CAMPAIGN)
            && self.utm_content.as_deref() == Some(utm_content(company_id).as_str())
    }
}

/// A scheduled event (the call).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduledEvent {
    pub uri: String,
    /// `status` is `active` (not `canceled`).
    pub active: bool,
    pub start_at: NaiveDateTime,
    pub end_at: NaiveDateTime,
    pub event_type: Option<String>,
    pub cancellation: Option<Cancellation>,
}

/// Which event types the slot check asks about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotEventTypes {
    /// No booking page or no active event type.
    NotConfigured,
    /// Event type URIs (at most [`MAX_SLOT_EVENT_TYPES`]).
    Types(Vec<String>),
}

/// The booked call as the commitment stores it.
pub fn booking(invitee: &Invitee, event: &ScheduledEvent, booked_at: NaiveDateTime) -> Booking {
    Booking {
        invitee_uri: invitee.uri.clone(),
        event_uri: event.uri.clone(),
        start_at: event.start_at,
        end_at: event.end_at,
        reschedule_url: invitee.reschedule_url.clone(),
        cancel_url: invitee.cancel_url.clone(),
        booked_at,
    }
}

/// A Calendly page URL without query or trailing slash, or `None`.
pub fn scheduling_url(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let (scheme, rest) = trimmed
        .strip_prefix("https://")
        .map(|rest| ("https://", rest))
        .or_else(|| {
            trimmed
                .strip_prefix("http://")
                .map(|rest| ("http://", rest))
        })?;
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

/// `2026-11-20T15:00:00.000000Z`, as Calendly writes and reads times.
pub fn calendly_time(at: NaiveDateTime) -> String {
    at.format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string()
}

fn time(value: &Value) -> Option<NaiveDateTime> {
    DateTime::parse_from_rfc3339(value.as_str()?)
        .ok()
        .map(|at| at.naive_utc().trunc_subsecs(0))
}

fn text(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn collection(body: &Value) -> &[Value] {
    body["collection"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
}

/// `users/me`.
pub fn parse_account(body: &Value) -> Option<Account> {
    let resource = &body["resource"];
    Some(Account {
        uri: text(&resource["uri"])?,
        scheduling_url: text(&resource["scheduling_url"]),
    })
}

/// `event_types`: the active ones.
pub fn parse_event_types(body: &Value) -> Vec<EventType> {
    collection(body)
        .iter()
        .filter(|item| item["active"].as_bool() != Some(false))
        .filter_map(|item| {
            Some(EventType {
                uri: text(&item["uri"])?,
                scheduling_url: text(&item["scheduling_url"]),
            })
        })
        .collect()
}

fn parse_cancellation(value: &Value) -> Option<Cancellation> {
    value.is_object().then(|| Cancellation {
        by_host: value["canceler_type"].as_str() == Some("host"),
        at: time(&value["created_at"]),
    })
}

/// An invitee resource (`resource` of a single read, or a collection item).
pub fn parse_invitee(resource: &Value) -> Option<Invitee> {
    let tracking = &resource["tracking"];
    Some(Invitee {
        uri: text(&resource["uri"])?,
        active: resource["status"].as_str() == Some("active"),
        email: text(&resource["email"]).unwrap_or_default(),
        name: text(&resource["name"]),
        utm_campaign: text(&tracking["utm_campaign"]),
        utm_content: text(&tracking["utm_content"]),
        reschedule_url: text(&resource["reschedule_url"]),
        cancel_url: text(&resource["cancel_url"]),
        rescheduled: resource["rescheduled"].as_bool().unwrap_or(false),
        new_invitee: text(&resource["new_invitee"]),
        cancellation: parse_cancellation(&resource["cancellation"]),
        no_show_at: resource["no_show"]
            .is_object()
            .then(|| time(&resource["no_show"]["created_at"]))
            .flatten(),
        event_uri: text(&resource["event"])?,
        created_at: time(&resource["created_at"]),
    })
}

/// A scheduled event resource.
pub fn parse_event(resource: &Value) -> Option<ScheduledEvent> {
    Some(ScheduledEvent {
        uri: text(&resource["uri"])?,
        active: resource["status"].as_str() == Some("active"),
        start_at: time(&resource["start_time"])?,
        end_at: time(&resource["end_time"])?,
        event_type: text(&resource["event_type"]),
        cancellation: parse_cancellation(&resource["cancellation"]),
    })
}

/// `event_type_available_times`: the start of every bookable slot.
pub fn parse_available_times(body: &Value) -> Vec<NaiveDateTime> {
    collection(body)
        .iter()
        .filter(|slot| {
            slot["status"]
                .as_str()
                .is_none_or(|status| status == "available")
        })
        .filter_map(|slot| time(&slot["start_time"]))
        .collect()
}

/// The event types whose page is the onboarding page; else (the page is the
/// account's own, or none matches) every active one. At most
/// [`MAX_SLOT_EVENT_TYPES`].
pub fn pick_slot_event_types(types: &[EventType], page: &str, own: Option<&str>) -> SlotEventTypes {
    fn first_uris<'a>(types: impl Iterator<Item = &'a EventType>) -> Vec<String> {
        types
            .map(|kind| kind.uri.clone())
            .take(MAX_SLOT_EVENT_TYPES)
            .collect()
    }
    let matching = if own == Some(page) {
        Vec::new()
    } else {
        first_uris(types.iter().filter(|kind| {
            kind.scheduling_url
                .as_deref()
                .and_then(scheduling_url)
                .as_deref()
                == Some(page)
        }))
    };
    let chosen = if matching.is_empty() {
        first_uris(types.iter())
    } else {
        matching
    };
    if chosen.is_empty() {
        SlotEventTypes::NotConfigured
    } else {
        SlotEventTypes::Types(chosen)
    }
}

fn is_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_ID_LEN
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// Dima's Calendly; disabled without `CALENDLY_API_TOKEN`.
#[derive(Clone)]
pub struct Calendly {
    token: Option<Arc<str>>,
    base: Arc<str>,
    /// `CALENDLY_ONBOARDING_URL`, when it is a Calendly page.
    onboarding_url: Option<Arc<str>>,
    http: reqwest::Client,
    account: Arc<Mutex<Option<(Instant, Account)>>>,
}

impl Calendly {
    pub fn disabled() -> Self {
        Self::new(None, API_BASE, None)
    }

    /// `CALENDLY_API_TOKEN` (the existing token, never a second one),
    /// `CALENDLY_BASE_URL` and `CALENDLY_ONBOARDING_URL`.
    pub fn from_env() -> Self {
        let var = |name: &str| {
            std::env::var(name)
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        };
        let onboarding = var("CALENDLY_ONBOARDING_URL");
        if onboarding
            .as_deref()
            .is_some_and(|url| scheduling_url(url).is_none())
        {
            tracing::warn!("CALENDLY_ONBOARDING_URL is not a Calendly page; ignored");
        }
        Self::new(
            var("CALENDLY_API_TOKEN").as_deref(),
            var("CALENDLY_BASE_URL").as_deref().unwrap_or(API_BASE),
            onboarding.as_deref(),
        )
    }

    /// `base` is `https://api.calendly.com` (a stub in tests); an
    /// `onboarding_url` that is not a Calendly page is ignored.
    pub fn new(token: Option<&str>, base: &str, onboarding_url: Option<&str>) -> Self {
        Self {
            token: token
                .map(str::trim)
                .filter(|token| !token.is_empty())
                .map(Arc::from),
            base: Arc::from(base.trim().trim_end_matches('/')),
            onboarding_url: onboarding_url.and_then(scheduling_url).map(Arc::from),
            http: common::http::client(),
            account: Arc::default(),
        }
    }

    pub const fn is_enabled(&self) -> bool {
        self.token.is_some()
    }

    /// The path of a Calendly API URI: `https://api.calendly.com/<path>` or
    /// `<base>/<path>`, letters, digits, `-`, `_` and `/` only.
    pub fn api_path(&self, uri: &str) -> Option<String> {
        let uri = uri.trim();
        let path = uri
            .strip_prefix(API_BASE)
            .or_else(|| uri.strip_prefix(&*self.base))?;
        let valid = path.len() > 1
            && path.len() <= MAX_PATH_LEN
            && path.starts_with('/')
            && !path.contains("//")
            && path
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_'));
        valid.then(|| path.to_string())
    }

    /// The path of a booking's invitee URI:
    /// `/scheduled_events/{id}/invitees/{id}` only.
    pub fn invitee_path(&self, uri: &str) -> Option<String> {
        let path = self.api_path(uri)?;
        let parts: Vec<&str> = path.split('/').collect();
        match parts.as_slice() {
            ["", "scheduled_events", event, "invitees", invitee]
                if is_id(event) && is_id(invitee) =>
            {
                Some(path)
            }
            _ => None,
        }
    }

    async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value, CalendlyError> {
        let token = self.token.as_deref().ok_or(CalendlyError::NotConfigured)?;
        let query = serde_html_form::to_string(query).map_err(|_| CalendlyError::BadUri)?;
        let url = if query.is_empty() {
            format!("{}{path}", self.base)
        } else {
            format!("{}{path}?{query}", self.base)
        };
        let response = self
            .http
            .get(url)
            .bearer_auth(token)
            .timeout(TIMEOUT)
            .send()
            .await
            .map_err(|_| CalendlyError::Request)?;
        let status = response.status();
        if !status.is_success() {
            return Err(CalendlyError::Status(status.as_u16()));
        }
        response
            .json::<Value>()
            .await
            .map_err(|_| CalendlyError::Invalid)
    }

    /// The account read within the last hour, without asking Calendly.
    pub fn cached_account(&self) -> Option<Account> {
        self.account.lock().ok().and_then(|cache| {
            cache
                .as_ref()
                .filter(|(at, _)| at.elapsed() < ACCOUNT_CACHE)
                .map(|(_, account)| account.clone())
        })
    }

    /// `users/me`, kept for an hour.
    pub async fn account(&self) -> Result<Account, CalendlyError> {
        if let Some(account) = self.cached_account() {
            return Ok(account);
        }
        let account =
            parse_account(&self.get("/users/me", &[]).await?).ok_or(CalendlyError::Invalid)?;
        if let Ok(mut cache) = self.account.lock() {
            *cache = Some((Instant::now(), account.clone()));
        }
        Ok(account)
    }

    /// The account's active event types (the first 100).
    pub async fn active_event_types(
        &self,
        user_uri: &str,
    ) -> Result<Vec<EventType>, CalendlyError> {
        let body = self
            .get(
                "/event_types",
                &[
                    ("user", user_uri.into()),
                    ("active", "true".into()),
                    ("count", "100".into()),
                ],
            )
            .await?;
        Ok(parse_event_types(&body))
    }

    pub async fn invitee(&self, uri: &str) -> Result<Invitee, CalendlyError> {
        let path = self.api_path(uri).ok_or(CalendlyError::BadUri)?;
        let body = self.get(&path, &[]).await?;
        parse_invitee(&body["resource"]).ok_or(CalendlyError::Invalid)
    }

    pub async fn scheduled_event(&self, uri: &str) -> Result<ScheduledEvent, CalendlyError> {
        let path = self.api_path(uri).ok_or(CalendlyError::BadUri)?;
        let body = self.get(&path, &[]).await?;
        parse_event(&body["resource"]).ok_or(CalendlyError::Invalid)
    }

    /// The account's active events starting from `min_start` with an invitee
    /// of this email (the first 20, soonest first).
    pub async fn active_events_with_invitee(
        &self,
        user_uri: &str,
        email: &str,
        min_start: NaiveDateTime,
    ) -> Result<Vec<ScheduledEvent>, CalendlyError> {
        let body = self
            .get(
                "/scheduled_events",
                &[
                    ("user", user_uri.into()),
                    ("invitee_email", email.into()),
                    ("status", "active".into()),
                    ("min_start_time", calendly_time(min_start)),
                    ("sort", "start_time:asc".into()),
                    ("count", "20".into()),
                ],
            )
            .await?;
        Ok(collection(&body).iter().filter_map(parse_event).collect())
    }

    /// The event's invitees with this email.
    pub async fn event_invitees(
        &self,
        event_uri: &str,
        email: &str,
    ) -> Result<Vec<Invitee>, CalendlyError> {
        let path = self.api_path(event_uri).ok_or(CalendlyError::BadUri)?;
        let body = self
            .get(
                &format!("{path}/invitees"),
                &[("email", email.into()), ("count", "20".into())],
            )
            .await?;
        Ok(collection(&body).iter().filter_map(parse_invitee).collect())
    }

    /// Bookable slot starts of an event type in `[start, end)`; Calendly
    /// takes at most 7 days, starting in the future.
    pub async fn available_slots(
        &self,
        event_type_uri: &str,
        start: NaiveDateTime,
        end: NaiveDateTime,
    ) -> Result<Vec<NaiveDateTime>, CalendlyError> {
        let body = self
            .get(
                "/event_type_available_times",
                &[
                    ("event_type", event_type_uri.into()),
                    ("start_time", calendly_time(start)),
                    ("end_time", calendly_time(end)),
                ],
            )
            .await?;
        Ok(parse_available_times(&body))
    }

    /// The onboarding booking page: `CALENDLY_ONBOARDING_URL`, else the
    /// account's own page; `None` when neither can be had.
    pub async fn onboarding_page(&self) -> Result<Option<String>, CalendlyError> {
        if let Some(url) = &self.onboarding_url {
            return Ok(Some(url.to_string()));
        }
        if !self.is_enabled() {
            return Ok(None);
        }
        Ok(self
            .account()
            .await?
            .scheduling_url
            .as_deref()
            .and_then(scheduling_url))
    }

    /// The event types the daily slot check asks about.
    pub async fn slot_event_types(&self) -> Result<SlotEventTypes, CalendlyError> {
        if !self.is_enabled() {
            return Ok(SlotEventTypes::NotConfigured);
        }
        let account = self.account().await?;
        let own = account.scheduling_url.as_deref().and_then(scheduling_url);
        let Some(page) = self
            .onboarding_url
            .as_deref()
            .map(str::to_string)
            .or_else(|| own.clone())
        else {
            return Ok(SlotEventTypes::NotConfigured);
        };
        let types = self.active_event_types(&account.uri).await?;
        Ok(pick_slot_event_types(&types, &page, own.as_deref()))
    }

    /// The marketing demo page: the account's first active event type, else
    /// its own page; `None` without a token or when Calendly fails.
    pub async fn demo_page(&self) -> Option<String> {
        if !self.is_enabled() {
            return None;
        }
        let account = self.account().await.ok()?;
        let own = account.scheduling_url.as_deref().and_then(scheduling_url);
        let Ok(body) = self
            .get(
                "/event_types",
                &[
                    ("user", account.uri.clone()),
                    ("active", "true".into()),
                    ("count", "20".into()),
                ],
            )
            .await
        else {
            return own;
        };
        collection(&body)
            .iter()
            .find_map(|kind| kind["scheduling_url"].as_str().and_then(scheduling_url))
            .or(own)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::RawQuery;
    use axum::http::StatusCode;
    use axum::routing::get;
    use axum::{Json, Router};
    use serde_json::json;

    const BASE: &str = "http://127.0.0.1:9";

    fn at(text: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S").unwrap()
    }

    fn client() -> Calendly {
        Calendly::new(Some("token"), BASE, None)
    }

    #[test]
    fn only_api_uris_on_calendly_or_the_base_pass_the_guard() {
        let calendly = client();
        assert_eq!(
            calendly
                .api_path("https://api.calendly.com/scheduled_events/E1")
                .as_deref(),
            Some("/scheduled_events/E1")
        );
        assert_eq!(
            calendly.api_path("http://127.0.0.1:9/users/me").as_deref(),
            Some("/users/me")
        );
        for bad in [
            "https://api.calendly.com.evil.test/scheduled_events/E1",
            "https://evil.test/scheduled_events/E1",
            "http://127.0.0.1:99/scheduled_events/E1",
            "https://api.calendly.com",
            "https://api.calendly.com/",
            "https://api.calendly.com//evil.test/x",
            "https://api.calendly.com/scheduled_events/E1?x=1",
            "https://api.calendly.com/scheduled_events/../users/me",
            "https://api.calendly.com/scheduled_events/E1#x",
            "https://api.calendly.com@evil.test/x",
            "ftp://api.calendly.com/x",
        ] {
            assert_eq!(calendly.api_path(bad), None, "{bad}");
        }
    }

    #[test]
    fn bookings_take_only_the_invitee_path_shape() {
        let calendly = client();
        assert_eq!(
            calendly
                .invitee_path(" https://api.calendly.com/scheduled_events/ab-12/invitees/CD-34 ")
                .as_deref(),
            Some("/scheduled_events/ab-12/invitees/CD-34")
        );
        assert!(
            calendly
                .invitee_path("http://127.0.0.1:9/scheduled_events/a/invitees/b")
                .is_some()
        );
        let long = "a".repeat(65);
        for bad in [
            "https://api.calendly.com/scheduled_events/a/invitees".to_string(),
            "https://api.calendly.com/scheduled_events/a/invitees/b/c".to_string(),
            "https://api.calendly.com/users/a/invitees/b".to_string(),
            "https://api.calendly.com/scheduled_events/a_b/invitees/c".to_string(),
            format!("https://api.calendly.com/scheduled_events/{long}/invitees/b"),
            "https://evil.test/scheduled_events/a/invitees/b".to_string(),
        ] {
            assert_eq!(calendly.invitee_path(&bad), None, "{bad}");
        }
    }

    #[test]
    fn errors_carry_the_status_code_only() {
        assert_eq!(CalendlyError::Status(404).code(), "http_404");
        assert_eq!(
            CalendlyError::Status(503).to_string(),
            "Calendly answered 503"
        );
        assert_eq!(CalendlyError::Request.code(), "request_failed");
        assert_eq!(CalendlyError::Budget.code(), "budget_spent");
    }

    fn invitee_json() -> Value {
        json!({ "resource": {
            "uri": "https://api.calendly.com/scheduled_events/E1/invitees/I1",
            "status": "canceled",
            "email": "owen@example.test",
            "name": "Owen",
            "tracking": { "utm_campaign": "granite-onboarding", "utm_content": "company-7" },
            "reschedule_url": "https://calendly.com/reschedulings/I1",
            "cancel_url": "https://calendly.com/cancellations/I1",
            "rescheduled": true,
            "new_invitee": "https://api.calendly.com/scheduled_events/E2/invitees/I2",
            "cancellation": { "canceled_by": "Dima", "canceler_type": "host",
                              "created_at": "2026-11-01T10:00:00.123456Z" },
            "no_show": { "uri": "https://api.calendly.com/invitee_no_shows/N1",
                         "created_at": "2026-11-02T11:30:00.000000Z" },
            "event": "https://api.calendly.com/scheduled_events/E1",
            "created_at": "2026-10-30T09:00:00.000000Z",
        }})
    }

    #[test]
    fn an_invitee_is_read_with_its_tag_reschedule_cancellation_and_no_show() {
        let invitee = parse_invitee(&invitee_json()["resource"]).unwrap();
        assert!(!invitee.active);
        assert!(invitee.carries_tag(7));
        assert!(!invitee.carries_tag(70));
        assert!(invitee.rescheduled);
        assert_eq!(
            invitee.new_invitee.as_deref(),
            Some("https://api.calendly.com/scheduled_events/E2/invitees/I2")
        );
        assert_eq!(
            invitee.cancellation,
            Some(Cancellation {
                by_host: true,
                at: Some(at("2026-11-01 10:00:00"))
            })
        );
        assert_eq!(invitee.no_show_at, Some(at("2026-11-02 11:30:00")));
        assert_eq!(
            invitee.event_uri,
            "https://api.calendly.com/scheduled_events/E1"
        );
        assert_eq!(invitee.created_at, Some(at("2026-10-30 09:00:00")));

        let plain = parse_invitee(&json!({
            "uri": "u", "status": "active", "email": "a@b.test", "event": "e",
            "tracking": { "utm_campaign": null }, "no_show": null, "cancellation": null,
            "new_invitee": null,
        }))
        .unwrap();
        assert!(plain.active && !plain.rescheduled);
        assert_eq!(plain.cancellation, None);
        assert_eq!(plain.no_show_at, None);
        assert!(!plain.carries_tag(7));
        assert_eq!(parse_invitee(&json!({ "uri": "u" })), None);
    }

    #[test]
    fn events_event_types_and_slots_are_read() {
        let event = parse_event(&json!({
            "uri": "https://api.calendly.com/scheduled_events/E1",
            "status": "canceled",
            "start_time": "2026-11-20T15:00:00.000000Z",
            "end_time": "2026-11-20T16:00:00Z",
            "event_type": "https://api.calendly.com/event_types/T1",
            "cancellation": { "canceler_type": "invitee" },
        }))
        .unwrap();
        assert!(!event.active);
        assert_eq!(event.start_at, at("2026-11-20 15:00:00"));
        assert_eq!(event.end_at, at("2026-11-20 16:00:00"));
        assert_eq!(
            event.cancellation,
            Some(Cancellation {
                by_host: false,
                at: None
            })
        );
        assert_eq!(
            parse_event(&json!({ "uri": "e", "status": "active" })),
            None
        );

        let types = parse_event_types(&json!({ "collection": [
            { "uri": "T1", "scheduling_url": "https://calendly.com/dima/onboarding", "active": true },
            { "uri": "T2", "scheduling_url": "https://calendly.com/dima/old", "active": false },
            { "scheduling_url": "https://calendly.com/dima/x" },
        ], "pagination": { "next_page": null } }));
        assert_eq!(
            types,
            vec![EventType {
                uri: "T1".into(),
                scheduling_url: Some("https://calendly.com/dima/onboarding".into())
            }]
        );

        let slots = parse_available_times(&json!({ "collection": [
            { "status": "available", "start_time": "2026-11-20T15:00:00Z" },
            { "status": "unavailable", "start_time": "2026-11-20T16:00:00Z" },
            { "status": "available", "start_time": "garbage" },
        ]}));
        assert_eq!(slots, vec![at("2026-11-20 15:00:00")]);
        assert_eq!(parse_available_times(&json!({})), Vec::new());
        assert_eq!(
            calendly_time(at("2026-11-20 15:00:00")),
            "2026-11-20T15:00:00.000000Z"
        );
    }

    fn kind(uri: &str, page: &str) -> EventType {
        EventType {
            uri: uri.into(),
            scheduling_url: Some(page.into()),
        }
    }

    #[test]
    fn slot_checks_ask_about_the_onboarding_page_else_every_type() {
        let types = vec![
            kind("T1", "https://calendly.com/dima/demo"),
            kind("T2", "https://calendly.com/dima/onboarding/"),
        ];
        let own = Some("https://calendly.com/dima");
        assert_eq!(
            pick_slot_event_types(&types, "https://calendly.com/dima/onboarding", own),
            SlotEventTypes::Types(vec!["T2".into()])
        );
        assert_eq!(
            pick_slot_event_types(&types, "https://calendly.com/dima", own),
            SlotEventTypes::Types(vec!["T1".into(), "T2".into()])
        );
        assert_eq!(
            pick_slot_event_types(&types, "https://calendly.com/dima/other", own),
            SlotEventTypes::Types(vec!["T1".into(), "T2".into()])
        );
        let many: Vec<EventType> = (0..8)
            .map(|i| kind(&format!("T{i}"), "https://calendly.com/x"))
            .collect();
        assert!(matches!(
            pick_slot_event_types(&many, "https://calendly.com/dima", own),
            SlotEventTypes::Types(uris) if uris.len() == MAX_SLOT_EVENT_TYPES
        ));
        assert_eq!(
            pick_slot_event_types(&[], "https://calendly.com/dima", own),
            SlotEventTypes::NotConfigured
        );
    }

    #[test]
    fn the_onboarding_url_must_be_a_calendly_page() {
        let set = Calendly::new(None, BASE, Some("https://calendly.com/dima/onboarding?x=1"));
        assert_eq!(
            set.onboarding_url.as_deref(),
            Some("https://calendly.com/dima/onboarding")
        );
        let ignored = Calendly::new(None, BASE, Some("https://evil.test/x"));
        assert_eq!(ignored.onboarding_url, None);
    }

    async fn serve(router: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        format!("http://{address}")
    }

    fn me() -> Value {
        json!({ "resource": {
            "uri": "https://api.calendly.com/users/U1",
            "scheduling_url": "https://calendly.com/dima",
        }})
    }

    #[tokio::test]
    async fn the_demo_page_is_the_first_active_event_type_else_the_account_page() {
        let base = serve(
            Router::new()
                .route("/users/me", get(|| async { Json(me()) }))
                .route(
                    "/event_types",
                    get(|RawQuery(query): RawQuery| async move {
                        assert!(query.unwrap_or_default().contains("active=true"));
                        Json(json!({ "collection": [
                            { "uri": "T1", "scheduling_url": "https://calendly.com/dima/demo?x=1" },
                        ]}))
                    }),
                ),
        )
        .await;
        let calendly = Calendly::new(Some("token"), &base, None);
        assert_eq!(
            calendly.demo_page().await.as_deref(),
            Some("https://calendly.com/dima/demo")
        );

        let no_types = serve(
            Router::new()
                .route("/users/me", get(|| async { Json(me()) }))
                .route(
                    "/event_types",
                    get(|| async { StatusCode::INTERNAL_SERVER_ERROR }),
                ),
        )
        .await;
        let calendly = Calendly::new(Some("token"), &no_types, None);
        assert_eq!(
            calendly.demo_page().await.as_deref(),
            Some("https://calendly.com/dima")
        );
        assert_eq!(
            calendly.onboarding_page().await,
            Ok(Some("https://calendly.com/dima".into()))
        );
        assert_eq!(Calendly::disabled().demo_page().await, None);
        assert_eq!(Calendly::disabled().onboarding_page().await, Ok(None));
    }

    #[tokio::test]
    async fn a_failed_account_read_is_not_cached() {
        let calls = Arc::new(Mutex::new(0));
        let counted = Arc::clone(&calls);
        let base = serve(Router::new().route(
            "/users/me",
            get(move || {
                let counted = Arc::clone(&counted);
                async move {
                    let mut calls = counted.lock().unwrap();
                    *calls += 1;
                    if *calls == 1 {
                        Err(StatusCode::BAD_GATEWAY)
                    } else {
                        Ok(Json(me()))
                    }
                }
            }),
        ))
        .await;
        let calendly = Calendly::new(Some("token"), &base, None);
        assert_eq!(calendly.account().await, Err(CalendlyError::Status(502)));
        assert!(calendly.account().await.is_ok());
        assert!(calendly.account().await.is_ok());
        assert_eq!(*calls.lock().unwrap(), 2);
    }
}
