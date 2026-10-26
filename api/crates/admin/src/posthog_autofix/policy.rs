//! The intake's decisions as plain functions: which bugs may start a run,
//! which web error is the copy of an api error, how many runs may start now,
//! and when to retry.

use super::config::Config;
use super::model::{SkipReason, Source};
use chrono::{DateTime, Duration, NaiveDateTime, Utc};

/// A settled bug, as the intake read it from its events and PostHog.
#[derive(Debug, Clone, Copy)]
pub struct Candidate<'a> {
    /// The PostHog status now of the issue of the bug's latest event:
    /// `active`, `resolved`, `suppressed`...
    pub issue_status: &'a str,
    pub source: Source,
    pub exception_type: &'a str,
    pub message: &'a str,
    /// The browser's `$host` (web events only).
    pub host: Option<&'a str>,
    /// Events found for the bug.
    pub occurrences: u64,
    /// The event's `error_source` (`toast`, `client`, `api`...).
    pub error_source: Option<&'a str>,
    /// The event's `api_status`: the HTTP status of a failed API call.
    pub api_status: Option<u16>,
}

const PRODUCTION_DOMAIN: &str = "granite-manager.com";
const RETRY_BASE_MINUTES: i64 = 5;
const RETRY_CAP_MINUTES: i64 = 4 * 60;

/// The SPA's toast and API error types.
const TOAST: &str = "ErrorToast";
const GRANITE_API_ERROR: &str = "GraniteApiError";
/// A toast with one of these words reports a failure, not a hint.
const FAILURE_WORDS: [&str; 2] = ["could not", "server"];
/// The api's bodies for a 5xx, which say nothing the api did not report.
const GENERIC_API_BODIES: [&str; 3] = ["database error", "internal error", "internal server error"];
/// axum's rejections, and the api's own refusals of data the client built:
/// the client and the api disagree on the contract.
const CONTRACT_REJECTIONS: [&str; 8] = [
    "invalid url",
    "invalid body",
    "failed to deserialize",
    "unknown variant",
    "length limit",
    "missing field",
    "cannot parse",
    "could not be read",
];

/// Which code raised the error, from an event's `distinct_id` and `$lib`.
pub fn source_of(distinct_id: Option<&str>, lib: Option<&str>) -> Source {
    match (distinct_id, lib) {
        (Some("granite-api"), _) => Source::Api,
        (Some("server-webhooks"), _) => Source::Webhooks,
        (Some("server-time-triggered"), _) => Source::TimeTriggered,
        (_, Some("web")) => Source::Web,
        (_, Some("posthog-node")) => Source::LegacyNode,
        _ => Source::Unknown,
    }
}

/// `Ok` when the bug may start a run, else why it is skipped.
pub fn eligibility(candidate: &Candidate<'_>, exclude: &[String]) -> Result<(), SkipReason> {
    match candidate.issue_status {
        "suppressed" => return Err(SkipReason::Suppressed),
        "active" => {}
        _ => return Err(SkipReason::Resolved),
    }
    if candidate.occurrences == 0 {
        return Err(SkipReason::NoEvents);
    }
    if candidate.source == Source::LegacyNode {
        return Err(SkipReason::LegacyRuntime);
    }
    if candidate.source == Source::Web && candidate.host.is_some_and(is_non_production_host) {
        return Err(SkipReason::NonProduction);
    }
    if is_excluded(candidate, exclude) {
        return Err(SkipReason::Excluded);
    }
    noise(candidate).map_or(Ok(()), Err)
}

/// The gates on the bug's own text: a toast that only guides the user, the
/// browser's copy of an API 500, an API 4xx that enforced a business rule.
fn noise(candidate: &Candidate<'_>) -> Option<SkipReason> {
    let message = candidate.message.to_lowercase();
    if candidate.source == Source::Web {
        let toast = candidate.exception_type == TOAST || candidate.error_source == Some("toast");
        let generic =
            matches!(candidate.exception_type, "Error" | TOAST) && is_generic_api_body(&message);
        if generic || (toast && message.contains("database error")) {
            return Some(SkipReason::Symptom);
        }
        if toast && !FAILURE_WORDS.iter().any(|word| message.contains(word)) {
            return Some(SkipReason::HintToast);
        }
    }
    if candidate.exception_type == GRANITE_API_ERROR {
        let status = api_status(
            candidate.exception_type,
            candidate.message,
            candidate.api_status,
        );
        let body = granite_api_call(candidate.message).map_or("", |call| call.body);
        let body_lower = body.to_lowercase();
        let rule = status.is_some_and(|status| (400..500).contains(&status));
        let contract = CONTRACT_REJECTIONS
            .iter()
            .any(|text| body_lower.contains(text));
        let missing_route = matches!(status, Some(404 | 405)) && body.is_empty();
        if rule && !contract && !missing_route {
            return Some(SkipReason::BusinessRule4xx);
        }
    }
    None
}

/// `database error`, `Error: internal error`...: the whole message is the
/// api's generic body.
fn is_generic_api_body(lowercase_message: &str) -> bool {
    let text = lowercase_message.trim();
    let text = text.strip_prefix("error:").map_or(text, str::trim_start);
    GENERIC_API_BODIES.contains(&text)
}

/// A failed API call as the SPA's `GraniteApiError` words it:
/// `Granite API 500 on /v1/deals?x=1: {"error":"database error"}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GraniteApiCall<'a> {
    pub status: u16,
    /// The path, with its query string.
    pub path: &'a str,
    /// The response body; empty when the api sent none.
    pub body: &'a str,
}

pub fn granite_api_call(message: &str) -> Option<GraniteApiCall<'_>> {
    let rest = message.trim_start().strip_prefix("Granite API ")?;
    let (status, rest) = rest.split_once(' ')?;
    if status.len() != 3 {
        return None;
    }
    let status = status.parse().ok()?;
    let rest = rest.strip_prefix("on ")?;
    let (path, body) = rest.split_once(": ").map_or_else(
        || (rest.trim_end().trim_end_matches(':'), ""),
        |(path, body)| (path, body.trim()),
    );
    Some(GraniteApiCall { status, path, body })
}

/// The HTTP status of a failed API call: the event's `api_status`, else the
/// one a `GraniteApiError` message names.
pub fn api_status(exception_type: &str, message: &str, api_status: Option<u16>) -> Option<u16> {
    api_status.or_else(|| {
        (exception_type == GRANITE_API_ERROR)
            .then(|| granite_api_call(message))
            .flatten()
            .map(|call| call.status)
    })
}

/// The path of a web `GraniteApiError` with a 5xx: the call whose failure the
/// api reports itself, under the same route.
pub fn web_5xx_path<'a>(
    source: Source,
    exception_type: &str,
    message: &'a str,
    status: Option<u16>,
    route: Option<&'a str>,
) -> Option<&'a str> {
    if source != Source::Web || exception_type != GRANITE_API_ERROR {
        return None;
    }
    let call = granite_api_call(message);
    let status = status.or_else(|| call.map(|call| call.status))?;
    if status < 500 {
        return None;
    }
    route.or_else(|| call.map(|call| call.path))
}

/// Whether `path` is a request to the api route `template`, segment by
/// segment: `{param}` takes any one segment, query strings are ignored.
pub fn route_matches(template: &str, path: &str) -> bool {
    let segments = |text: &str| -> Vec<String> {
        let text = text.split(['?', '#']).next().unwrap_or("");
        text.trim_end_matches('/')
            .split('/')
            .map(str::to_owned)
            .collect()
    };
    let (template, path) = (segments(template), segments(path));
    template.len() == path.len()
        && template.iter().zip(&path).all(|(expected, actual)| {
            let param = expected.len() > 2 && expected.starts_with('{') && expected.ends_with('}');
            if param {
                !actual.is_empty()
            } else {
                expected == actual
            }
        })
}

/// Localhost, IPs and `.local`/`.test` hosts all fail the domain check.
/// An empty host counts as missing.
pub fn is_non_production_host(host: &str) -> bool {
    let host = host.trim().to_ascii_lowercase();
    if host.is_empty() {
        return false;
    }
    let name = host
        .split_once(':')
        .map_or(host.as_str(), |(name, _port)| name);
    let subdomain_suffix = format!(".{PRODUCTION_DOMAIN}");
    name != PRODUCTION_DOMAIN && !name.ends_with(&subdomain_suffix)
}

fn is_excluded(candidate: &Candidate<'_>, exclude: &[String]) -> bool {
    let text = format!("{}: {}", candidate.exception_type, candidate.message).to_lowercase();
    exclude
        .iter()
        .filter(|pattern| !pattern.is_empty())
        .any(|pattern| text.contains(&pattern.to_lowercase()))
}

/// How many runs may be dispatched now.
pub fn dispatch_slots(in_flight: u32, dispatched_last_day: u32, config: &Config) -> u32 {
    config
        .max_concurrent
        .saturating_sub(in_flight)
        .min(config.max_per_day.saturating_sub(dispatched_last_day))
}

/// How long a queued row waits after its `attempts`-th failed dispatch.
pub fn retry_delay(attempts: u8) -> Duration {
    let Some(exponent) = attempts.checked_sub(1) else {
        return Duration::zero();
    };
    let minutes = RETRY_BASE_MINUTES
        .saturating_mul(3_i64.saturating_pow(u32::from(exponent)))
        .min(RETRY_CAP_MINUTES);
    Duration::minutes(minutes)
}

/// A fixed issue that PostHog reopened: resolved before, active now.
pub fn is_regression(previous: Option<&str>, current: &str) -> bool {
    previous == Some("resolved") && current == "active"
}

/// The start of the `minutes`-long window holding `now` (UTC, aligned to the
/// epoch), the key for once-per-window claims.
pub fn window_start(now: DateTime<Utc>, minutes: i64) -> NaiveDateTime {
    let seconds = minutes.max(1).saturating_mul(60);
    let start = now.timestamp().div_euclid(seconds) * seconds;
    DateTime::from_timestamp(start, 0).map_or_else(|| now.naive_utc(), |time| time.naive_utc())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn candidate() -> Candidate<'static> {
        Candidate {
            issue_status: "active",
            source: Source::Web,
            exception_type: "TypeError",
            message: "Failed to fetch",
            host: Some("app.granite-manager.com"),
            occurrences: 3,
            error_source: None,
            api_status: None,
        }
    }

    fn check(candidate: &Candidate<'_>) -> Result<(), SkipReason> {
        eligibility(candidate, &[])
    }

    #[test]
    fn sources_come_from_distinct_id_then_lib() {
        let cases = [
            (Some("granite-api"), Some("web"), Source::Api),
            (Some("server-webhooks"), None, Source::Webhooks),
            (Some("server-time-triggered"), None, Source::TimeTriggered),
            (
                Some("server-time-triggered"),
                Some("web"),
                Source::TimeTriggered,
            ),
            (Some("user-1"), Some("web"), Source::Web),
            (None, Some("posthog-node"), Source::LegacyNode),
            (Some("user-1"), Some("other"), Source::Unknown),
            (None, None, Source::Unknown),
        ];
        for (distinct_id, lib, expected) in cases {
            assert_eq!(
                source_of(distinct_id, lib),
                expected,
                "{distinct_id:?} {lib:?}"
            );
        }
    }

    #[test]
    fn a_normal_production_issue_is_eligible() {
        assert_eq!(check(&candidate()), Ok(()));
        let api = Candidate {
            source: Source::Api,
            host: None,
            ..candidate()
        };
        assert_eq!(check(&api), Ok(()));
        let no_host = Candidate {
            host: None,
            ..candidate()
        };
        assert_eq!(check(&no_host), Ok(()));
    }

    #[test]
    fn status_decides_before_anything_else() {
        let worst = Candidate {
            source: Source::LegacyNode,
            occurrences: 0,
            ..candidate()
        };
        let suppressed = Candidate {
            issue_status: "suppressed",
            ..worst
        };
        let resolved = Candidate {
            issue_status: "resolved",
            ..worst
        };
        let pending = Candidate {
            issue_status: "pending_release",
            ..worst
        };
        assert_eq!(check(&suppressed), Err(SkipReason::Suppressed));
        assert_eq!(check(&resolved), Err(SkipReason::Resolved));
        assert_eq!(check(&pending), Err(SkipReason::Resolved));
    }

    #[test]
    fn eligibility_checks_run_in_order() {
        let exclude = vec!["Failed To Fetch".to_owned()];
        let all_bad = Candidate {
            source: Source::LegacyNode,
            occurrences: 0,
            host: Some("localhost"),
            ..candidate()
        };
        assert_eq!(eligibility(&all_bad, &exclude), Err(SkipReason::NoEvents));
        let legacy = Candidate {
            occurrences: 1,
            ..all_bad
        };
        assert_eq!(
            eligibility(&legacy, &exclude),
            Err(SkipReason::LegacyRuntime)
        );
        let local = Candidate {
            source: Source::Web,
            ..legacy
        };
        assert_eq!(
            eligibility(&local, &exclude),
            Err(SkipReason::NonProduction)
        );
        let excluded = Candidate {
            host: None,
            ..local
        };
        assert_eq!(eligibility(&excluded, &exclude), Err(SkipReason::Excluded));
        assert_eq!(eligibility(&excluded, &[]), Ok(()));
    }

    #[test]
    fn only_web_issues_are_checked_for_their_host() {
        let api = Candidate {
            source: Source::Api,
            host: Some("localhost"),
            ..candidate()
        };
        assert_eq!(check(&api), Ok(()));
    }

    #[test]
    fn hosts_outside_production_are_skipped() {
        for host in [
            "localhost",
            "localhost:3000",
            "127.0.0.1",
            "[::1]",
            "dev.local",
            "app.test",
            "example.com",
            "evilgranite-manager.com",
            "granite-manager.com.evil.io",
        ] {
            let web = Candidate {
                host: Some(host),
                ..candidate()
            };
            assert_eq!(check(&web), Err(SkipReason::NonProduction), "{host}");
        }
        for host in [
            "granite-manager.com",
            "Acme.Granite-Manager.com",
            "a.granite-manager.com:443",
        ] {
            let web = Candidate {
                host: Some(host),
                ..candidate()
            };
            assert_eq!(check(&web), Ok(()), "{host}");
        }
    }

    #[test]
    fn exclude_matches_type_and_message_case_insensitively() {
        let exclude = vec![String::new(), "typeerror: failed to".to_owned()];
        assert_eq!(
            eligibility(&candidate(), &exclude),
            Err(SkipReason::Excluded)
        );
        assert_eq!(eligibility(&candidate(), &[String::new()]), Ok(()));
    }

    /// A bug as one of its events reported it.
    fn event(
        source: Source,
        exception_type: &'static str,
        message: &'static str,
        error_source: Option<&'static str>,
        api_status: Option<u16>,
    ) -> Candidate<'static> {
        Candidate {
            source,
            exception_type,
            message,
            error_source,
            api_status,
            host: (source == Source::Web).then_some("granitedepotindy.granite-manager.com"),
            ..candidate()
        }
    }

    fn web_api(message: &'static str, status: Option<u16>) -> Candidate<'static> {
        event(
            Source::Web,
            "GraniteApiError",
            message,
            Some("client"),
            status,
        )
    }

    fn toast(message: &'static str) -> Candidate<'static> {
        event(Source::Web, "ErrorToast", message, Some("toast"), None)
    }

    fn api(message: &'static str) -> Candidate<'static> {
        event(Source::Api, "ApiError", message, Some("api"), Some(500))
    }

    fn judge(candidate: &Candidate<'_>) -> Result<(), SkipReason> {
        eligibility(candidate, &Config::default().exclude)
    }

    // Every REAL message of the 14-day replay (October 2026), as PostHog has it.
    #[test]
    fn the_real_api_call_failures_of_the_replay_pass_the_gates() {
        let real = [
            // The quote editor's autosave: the api cannot parse the drawing the SPA sent.
            web_api(
                r#"Granite API 400 on /v1/quotes/7031/autosave: {"error":"The drawing could not be read."}"#,
                Some(400),
            ),
            web_api(
                r#"Granite API 500 on /v1/schedule/settings: {"error":"database error"}"#,
                Some(500),
            ),
            web_api(
                r#"Granite API 500 on /v1/customers: {"error":"database error"}"#,
                Some(500),
            ),
            web_api(
                r#"Granite API 500 on /v1/notifications/done: {"error":"database error"}"#,
                Some(500),
            ),
            web_api(
                r#"Granite API 500 on /v1/deals?group_id=1&status=open: {"error":"database error"}"#,
                Some(500),
            ),
            web_api(
                r#"Granite API 500 on /v1/stats/overview?from_date=&to_date=&sales_rep=All: {"error":"database error"}"#,
                Some(500),
            ),
            web_api(
                r#"Granite API 400 on /v1/uploads: {"error":"invalid body: Failed to deserialize the JSON body into the target type: folder: unknown variant `quote-files`, expected one of `stones`, `sinks`, `faucets`, `deals`, `emails`, `events`, `quotes`, `template-images`, `documents`, `images`, `supports`, `bug-"#,
                Some(400),
            ),
            web_api(
                "Granite API 400 on /v1/customers/page?tab=walkin&page=1&page_size=20: Invalid URL: Cannot parse `page` to a `i32`",
                Some(400),
            ),
            web_api(
                "Granite API 400 on /v1/emails/-<phone>174/images: Invalid URL: Cannot parse `-<phone>174` to a `i32`",
                None,
            ),
            web_api(
                "Granite API 400 on /v1/sms/all/sales-reps: Invalid URL: unknown variant `all`, expected `cloudtalk` or `ringcentral`",
                Some(400),
            ),
            web_api(
                "Granite API 400 on /v1/sms/all/threads?limit=20&offset=0&scope=all: Invalid URL: unknown variant `all`, expected `cloudtalk` or `ringcentral`",
                Some(400),
            ),
            web_api(
                r#"Granite API 400 on /v1/ai/design: {"error":"invalid body: Failed to buffer the request body: length limit exceeded"}"#,
                None,
            ),
            web_api("Granite API 404 on /v1/schedule/unscheduled: ", Some(404)),
            web_api("Granite API 404 on /v1/schedule/unscheduled:", None),
            web_api("Granite API 404 on /v1/phone/admin/numbers: ", Some(404)),
            web_api("Granite API 404 on /v1/quotes/52/files: ", Some(404)),
            web_api("Granite API 404 on /v1/schedule/notifications: ", Some(404)),
        ];
        for bug in &real {
            assert_eq!(judge(bug), Ok(()), "{}", bug.message);
        }
    }

    #[test]
    fn the_real_server_and_browser_bugs_of_the_replay_pass_the_gates() {
        let real = [
            api(
                r#"Database(ColumnDecode { index: "0", source: "mismatched types; Rust type `i32` (as SQL type `INT`) is not compatible with SQL type `BIGINT UNSIGNED`" })"#,
            ),
            api(
                r#"Database(Database(MySqlDatabaseError { code: Some("42S22"), number: 1054, message: "Unknown column 'co.estimates_calendar_id' in 'where clause'" }))"#,
            ),
            api(
                r#"Database(Database(MySqlDatabaseError { code: Some("42000"), number: 1064, message: "You have an error in your SQL syntax; check the manual that corresponds to your MySQL server version for the right syntax to use near 'manual,\n                COUNT(c.id) AS total\n           FROM customers c\"#,
            ),
            api(
                r#"Database(Database(MySqlDatabaseError { code: Some("42S02"), number: 1146, message: "Table 'main.schedule_notification_flows' doesn't exist" }))"#,
            ),
            api(
                r#"Database(Database(MySqlDatabaseError { code: Some("HY000"), number: 3024, message: "Query execution was interrupted, maximum statement execution time exceeded" }))"#,
            ),
            api("Database(PoolTimedOut)"),
            toast("Failed to attach: Could not load images"),
            toast(
                "Failed to add files: The server could not save the files. Try again; if it keeps failing, contact support.",
            ),
            event(
                Source::Web,
                "DOMException",
                "SecurityError: Attempt to use history.replaceState() more than 100 times per 10 seconds",
                Some("client"),
                None,
            ),
            event(
                Source::Web,
                "DOMException",
                r#"SecurityError: Failed to read a named property 'document' from 'Window': Blocked a frame with origin "https://granitedepotindy.granite-manager.com" from accessing a cross-origin frame."#,
                Some("client"),
                None,
            ),
            event(
                Source::Web,
                "TypeError",
                "Cannot read properties of undefined (reading 'completed_at')",
                Some("client_render"),
                None,
            ),
            event(
                Source::Web,
                "TypeError",
                "Cannot destructure property 'getFieldState' of 'oe(...)' as it is null.",
                Some("client_render"),
                None,
            ),
        ];
        for bug in &real {
            assert_eq!(
                judge(bug),
                Ok(()),
                "{}: {}",
                bug.exception_type,
                bug.message
            );
        }
    }

    // The browser's copies of an API 500: the api's own error passes above.
    #[test]
    fn the_browser_copy_of_an_api_500_is_a_symptom() {
        for copy in [
            event(Source::Web, "Error", "database error", Some("client"), None),
            event(
                Source::Web,
                "Error",
                "Error: Internal Server Error",
                None,
                None,
            ),
            toast("Error: database error"),
            toast("Failed to save the deal: database error"),
        ] {
            assert_eq!(judge(&copy), Err(SkipReason::Symptom), "{}", copy.message);
        }
        // Only the web's copy: the api's own message is the bug.
        let api_copy = event(Source::Api, "Error", "database error", None, Some(500));
        assert_eq!(judge(&api_copy), Ok(()));
    }

    // The NOISE of the replay that gates G1b, G2, G3 and G4 stop.
    #[test]
    fn the_hint_toasts_of_the_replay_are_skipped() {
        let hint = [
            "Cannot send email: Please fix the highlighted fields before sending.",
            "Error: 3 units are installed on sales; units cannot go below that",
            "Error: At least one slab is required in each room",
            "Error: Customer needs a phone number and email before selling",
            "Error: Slab number 21717-16 is already used on this stone",
            "Failure: No deals found for this customer",
            "Microphone needed to call: Allow the microphone for this site (the icon in the address bar), then try again.",
            "No deal found: There is no open deal for this customer.",
        ];
        for message in hint {
            assert_eq!(
                judge(&toast(message)),
                Err(SkipReason::HintToast),
                "{message}"
            );
        }
        let toast_source = event(Source::Web, "Error", "No deals found", Some("toast"), None);
        assert_eq!(judge(&toast_source), Err(SkipReason::HintToast));
    }

    #[test]
    fn the_business_rule_4xx_of_the_replay_are_skipped() {
        let business = [
            (
                r#"Granite API 400 on /v1/sinks/42: {"error":"3 units are installed on sales; units cannot go below that"}"#,
                Some(400),
            ),
            (
                r#"Granite API 400 on /v1/schedule/notifications/templates/1: {"error":"Remove it from these flows first: Reminders"}"#,
                Some(400),
            ),
            (
                r#"Granite API 400 on /v1/stones/23/slabs: {"error":"Slab number On the way is already used on this stone"}"#,
                Some(400),
            ),
            (
                r#"Granite API 400 on /v1/slabs/3288: {"error":"Slab number 21717-16 is already used on this stone"}"#,
                None,
            ),
            (
                r#"Granite API 404 on /v1/quotes/52: {"error":"not found"}"#,
                Some(404),
            ),
            (
                r#"Granite API 404 on /v1/customers/124990: {"error":"not found"}"#,
                Some(404),
            ),
        ];
        for (message, status) in business {
            assert_eq!(
                judge(&web_api(message, status)),
                Err(SkipReason::BusinessRule4xx),
                "{message}"
            );
        }
    }

    #[test]
    fn the_third_party_and_known_noise_of_the_replay_is_excluded() {
        let excluded = [
            event(
                Source::Webhooks,
                "Marketing Authorization Error",
                "Failed to parse bearer UUID",
                None,
                None,
            ),
            event(
                Source::Webhooks,
                "HTTPError",
                r#"{"error":"cloudtalk_400","ids":[],"unsent_s3_keys":[]}"#,
                None,
                None,
            ),
            event(
                Source::Webhooks,
                "HTTPError",
                r#"{"error":"ringcentral_401","ids":[],"unsent_s3_keys":[]}"#,
                None,
                None,
            ),
            event(
                Source::Api,
                "HttpError",
                "POST /v1/sms/cloudtalk/messages answered 502",
                Some("api"),
                Some(502),
            ),
            api(
                r#"Upstream("Failed to generate AI reply: OpenAI request failed: error sending request")"#,
            ),
            event(
                Source::Web,
                "Error",
                r#"No result found for routeId "routes/employee.deals""#,
                Some("client"),
                None,
            ),
            event(
                Source::Web,
                "Error",
                "Calendly widget failed to load",
                Some("client"),
                None,
            ),
            event(
                Source::Web,
                "SigningError",
                "This signing link is not valid.",
                Some("client"),
                None,
            ),
            event(
                Source::Web,
                "Error",
                "Failed to load notifications",
                Some("client"),
                None,
            ),
            web_api(
                r#"Granite API 401 on /v1/sms/cloudtalk/desktop-alerts: {"error":"unauthorized"}"#,
                None,
            ),
            web_api(
                r#"Granite API 429 on /v1/phone/web/devices/4/token: {"error":"rate_limited","retryAfterSecs":59}"#,
                Some(429),
            ),
        ];
        for noise in &excluded {
            assert_eq!(judge(noise), Err(SkipReason::Excluded), "{}", noise.message);
        }
    }

    #[test]
    fn granite_api_messages_split_into_status_path_and_body() {
        assert_eq!(
            granite_api_call(
                r#"Granite API 500 on /v1/deals?group_id=1: {"error":"database error"}"#
            ),
            Some(GraniteApiCall {
                status: 500,
                path: "/v1/deals?group_id=1",
                body: r#"{"error":"database error"}"#,
            })
        );
        let empty = granite_api_call("Granite API 404 on /v1/schedule/unscheduled: ");
        assert_eq!(
            empty.map(|call| (call.path, call.body)),
            Some(("/v1/schedule/unscheduled", ""))
        );
        assert_eq!(granite_api_call("Granite API 5000 on /v1/x: y"), None);
        assert_eq!(granite_api_call("TypeError: x"), None);
        assert_eq!(
            api_status("GraniteApiError", "Granite API 404 on /v1/x: ", None),
            Some(404)
        );
        assert_eq!(
            api_status("GraniteApiError", "Granite API 404 on /v1/x: ", Some(500)),
            Some(500)
        );
        assert_eq!(
            api_status("TypeError", "Granite API 404 on /v1/x: ", None),
            None
        );
    }

    #[test]
    fn a_web_5xx_names_the_path_its_api_error_is_on() {
        let message = r#"Granite API 500 on /v1/customers/7?x=1: {"error":"database error"}"#;
        let path =
            |source, status, route| web_5xx_path(source, "GraniteApiError", message, status, route);
        assert_eq!(path(Source::Web, None, None), Some("/v1/customers/7?x=1"));
        assert_eq!(
            path(Source::Web, Some(500), Some("/v1/customers/7")),
            Some("/v1/customers/7")
        );
        assert_eq!(path(Source::Web, Some(404), None), None);
        assert_eq!(path(Source::Api, Some(500), None), None);
        assert_eq!(
            web_5xx_path(Source::Web, "Error", message, Some(500), None),
            None
        );
    }

    #[test]
    fn routes_match_segment_by_segment() {
        assert!(route_matches(
            "/v1/schedule/settings",
            "/v1/schedule/settings"
        ));
        assert!(route_matches("/v1/customers/{id}", "/v1/customers/123"));
        assert!(route_matches(
            "/v1/sms/{provider}/threads",
            "/v1/sms/cloudtalk/threads?limit=20"
        ));
        assert!(route_matches(
            "/v1/deals",
            "/v1/deals?group_id=1&status=open"
        ));
        assert!(route_matches("/v1/deals/", "/v1/deals"));
        assert!(!route_matches("/v1/customers/{id}", "/v1/customers"));
        assert!(!route_matches(
            "/v1/customers/{id}",
            "/v1/customers/1/notes"
        ));
        assert!(!route_matches("/v1/customers", "/v1/customers/page"));
        assert!(!route_matches(
            "/v1/schedule/settings",
            "/v1/schedule/events"
        ));
    }

    #[test]
    fn slots_are_the_tighter_cap_and_never_negative() {
        let config = Config {
            max_concurrent: 3,
            max_per_day: 10,
            ..Config::default()
        };
        assert_eq!(dispatch_slots(0, 0, &config), 3);
        assert_eq!(dispatch_slots(2, 0, &config), 1);
        assert_eq!(dispatch_slots(0, 9, &config), 1);
        assert_eq!(dispatch_slots(5, 0, &config), 0);
        assert_eq!(dispatch_slots(0, 12, &config), 0);
        assert_eq!(dispatch_slots(5, 12, &config), 0);
    }

    #[test]
    fn retries_back_off_threefold_up_to_four_hours() {
        let minutes = |attempts| retry_delay(attempts).num_minutes();
        assert_eq!(minutes(0), 0);
        assert_eq!(minutes(1), 5);
        assert_eq!(minutes(2), 15);
        assert_eq!(minutes(3), 45);
        assert_eq!(minutes(4), 135);
        assert_eq!(minutes(5), 240);
        assert_eq!(minutes(u8::MAX), 240);
    }

    #[test]
    fn a_regression_is_resolved_then_active() {
        assert!(is_regression(Some("resolved"), "active"));
        assert!(!is_regression(Some("active"), "active"));
        assert!(!is_regression(Some("resolved"), "resolved"));
        assert!(!is_regression(None, "active"));
    }

    #[test]
    fn windows_align_to_the_epoch() {
        let at = |h, m, s| Utc.with_ymd_and_hms(2026, 10, 20, h, m, s).unwrap();
        let start = |time, minutes| window_start(time, minutes).to_string();
        assert_eq!(start(at(10, 7, 31), 5), "2026-10-20 10:05:00");
        assert_eq!(start(at(10, 5, 0), 5), "2026-10-20 10:05:00");
        assert_eq!(start(at(10, 59, 59), 60), "2026-10-20 10:00:00");
        assert_eq!(start(at(10, 7, 31), 1), "2026-10-20 10:07:00");
        assert_eq!(start(at(10, 7, 31), 0), "2026-10-20 10:07:00");
        assert_eq!(start(at(10, 7, 31), -3), "2026-10-20 10:07:00");
    }
}
