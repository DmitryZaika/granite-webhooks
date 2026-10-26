//! `POSTHOG_AUTOFIX_*` settings, read once when the Lambda starts.
//!
//! Every knob has a safe default, and the mode defaults to `off`: an api
//! deployed without these variables does nothing. A value that does not parse
//! keeps its default and is logged, so a typo cannot turn the caps off.

use super::writeback::Assignee;
use chrono::{DateTime, Utc};
use lambda_http::tracing;
use std::fmt;

/// What the intake job does on each tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Nothing.
    Off,
    /// Discover, deduplicate and email what would be fixed; dispatch nothing.
    DryRun,
    /// Dispatch fix runs within the caps.
    Live,
}

impl Mode {
    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" => Some(Self::Off),
            "dry_run" | "dry-run" => Some(Self::DryRun),
            "live" => Some(Self::Live),
            _ => None,
        }
    }
}

/// Error messages that are noise, not bugs.
///
/// Matched case-insensitively against `<type>: <message>`;
/// `POSTHOG_AUTOFIX_EXCLUDE` replaces this list. Each entry was seen in
/// production PostHog issues (October 2026).
pub const DEFAULT_EXCLUDE: &[&str] = &[
    // The network dropped or a deploy replaced a chunk mid-session.
    "TypeError: Failed to fetch",
    // The same lost connection, rethrown as a plain Error.
    "Error: Failed to fetch",
    "TypeError: NetworkError when attempting to fetch resource",
    "TypeError: Load failed",
    "TypeError: Importing a module script failed",
    "Failed to fetch dynamically imported module",
    "AbortError",
    "Unable to decode turbo-stream response",
    // The service worker offline or mid-update.
    "service-worker.js",
    "FetchEvent.respondWith received an error",
    // The browser or its extensions, not our code.
    "ResizeObserver loop",
    "window.ethereum",
    "Script error.",
    "runtime.sendMessage",
    "Object Not Found Matching Id",
    "NotReadableError: The I/O read operation failed",
    "Failed to read the 'localStorage' property",
    "The database connection is closing",
    // iOS closes IndexedDB under the offline checklist when it suspends a tab.
    "[ChecklistDB] Database connection terminated",
    // A deploy replaced the route's chunk; the SPA reloads the page itself.
    "Error loading route module",
    // A signed-out session: the app sends the user to the login page.
    "Granite API 401 on",
    // The same 401, rewrapped by the SPA's SMS flows and call list.
    "FlowActionError: unauthorized",
    "Telephony 401",
    // A rate limit: the caller waits for retryAfterSecs and tries again.
    "Granite API 429 on",
    // A marketing form posting with a wrong or missing bearer token.
    "Failed to parse bearer UUID",
    // CloudTalk and RingCentral refused a send (their 4xx, our account setup).
    "cloudtalk_4",
    "ringcentral_401",
    // The SMS provider answered 502 to the api.
    "answered 502",
    // OpenAI unreachable.
    "OpenAI request failed",
    // A page kept from before a deploy asked for a route that is gone.
    "No result found for routeId",
    // Calendly's script blocked or down.
    "Calendly widget failed to load",
    // An expired or already used signing link.
    "SigningError: This signing link is not valid",
    // The SPA's rewrap of a failed request; the request reports itself.
    "Failed to load notifications",
];

#[derive(Clone)]
pub struct Config {
    pub mode: Mode,
    /// The cutover: events before it are never read (the backlog).
    pub start_at: Option<DateTime<Utc>>,
    /// PostHog is read at most once per this many minutes.
    pub poll_minutes: i64,
    /// A bug waits this long after its first event before it is judged, so
    /// its first burst of events (and the api error behind a web one) has
    /// arrived.
    pub settle_minutes: i64,
    pub max_concurrent: u32,
    /// Runs dispatched in any rolling 24 hours.
    pub max_per_day: u32,
    /// More new bugs than this in one storm window sends one digest email.
    pub storm_threshold: u32,
    pub storm_window_minutes: i64,
    /// Events of a bug whose row was discovered this recently count on that
    /// row; after it, the bug gets a new row and is judged again.
    pub signature_window_days: i64,
    /// A dispatched or running run is given up after this long.
    pub run_timeout_minutes: i64,
    /// Dispatch attempts (and usage-limit retries) before a run fails.
    pub max_attempts: u8,
    pub exclude: Vec<String>,
    pub recipients: Vec<String>,
    pub posthog_host: String,
    pub posthog_project_id: u64,
    /// Personal API key with `query:read`.
    pub posthog_key: Option<String>,
    /// `owner/repo` holding the workflow.
    pub github_repo: String,
    pub github_workflow: String,
    pub github_ref: String,
    /// Fine-grained token that may only start workflows (Actions: write).
    pub github_token: Option<String>,
    /// Bearer secret the workflow sends to the callbacks.
    pub callback_secret: Option<String>,
    /// Personal API key with `error_tracking:write`, for the write-back.
    pub write_key: Option<String>,
    /// Who the PostHog issues of opened fixes are assigned to, in turn.
    pub assignees: Vec<Assignee>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mode: Mode::Off,
            start_at: None,
            poll_minutes: 5,
            settle_minutes: 30,
            max_concurrent: 2,
            max_per_day: 8,
            storm_threshold: 20,
            storm_window_minutes: 30,
            signature_window_days: 14,
            run_timeout_minutes: 120,
            max_attempts: 3,
            exclude: DEFAULT_EXCLUDE.iter().map(ToString::to_string).collect(),
            recipients: Vec::new(),
            posthog_host: "https://us.posthog.com".into(),
            posthog_project_id: 141_685,
            posthog_key: None,
            github_repo: "DmitryZaika/granite-webhooks".into(),
            github_workflow: "posthog-autofix.yml".into(),
            github_ref: "main".into(),
            github_token: None,
            callback_secret: None,
            write_key: None,
            assignees: Vec::new(),
        }
    }
}

// Secrets stay out of logs.
impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let set = |secret: &Option<String>| if secret.is_some() { "<set>" } else { "<unset>" };
        f.debug_struct("Config")
            .field("mode", &self.mode)
            .field("start_at", &self.start_at)
            .field("poll_minutes", &self.poll_minutes)
            .field("settle_minutes", &self.settle_minutes)
            .field("max_concurrent", &self.max_concurrent)
            .field("max_per_day", &self.max_per_day)
            .field("storm_threshold", &self.storm_threshold)
            .field("storm_window_minutes", &self.storm_window_minutes)
            .field("signature_window_days", &self.signature_window_days)
            .field("run_timeout_minutes", &self.run_timeout_minutes)
            .field("max_attempts", &self.max_attempts)
            .field("exclude", &self.exclude)
            .field("recipients", &self.recipients)
            .field("posthog_host", &self.posthog_host)
            .field("posthog_project_id", &self.posthog_project_id)
            .field("posthog_key", &set(&self.posthog_key))
            .field("github_repo", &self.github_repo)
            .field("github_workflow", &self.github_workflow)
            .field("github_ref", &self.github_ref)
            .field("github_token", &set(&self.github_token))
            .field("callback_secret", &set(&self.callback_secret))
            .field("write_key", &set(&self.write_key))
            .field("assignees", &self.assignees)
            .finish()
    }
}

impl Config {
    pub fn from_env() -> Self {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Reads `POSTHOG_AUTOFIX_<NAME>` through `get`; blank counts as unset.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let read = |name: &str| {
            get(&format!("POSTHOG_AUTOFIX_{name}"))
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        };
        let mut config = Self::default();
        parse_into(&read, "MODE", &mut config.mode, Mode::parse);
        config.start_at = read("START_AT").and_then(|value| {
            DateTime::parse_from_rfc3339(&value)
                .map(|at| at.with_timezone(&Utc))
                .map_err(|_| tracing::warn!(value, "POSTHOG_AUTOFIX_START_AT is not RFC 3339"))
                .ok()
        });
        parse_into(&read, "POLL_MINUTES", &mut config.poll_minutes, positive);
        parse_into(
            &read,
            "SETTLE_MINUTES",
            &mut config.settle_minutes,
            positive,
        );
        parse_into(
            &read,
            "MAX_CONCURRENT",
            &mut config.max_concurrent,
            positive,
        );
        parse_into(&read, "MAX_PER_DAY", &mut config.max_per_day, positive);
        parse_into(
            &read,
            "STORM_THRESHOLD",
            &mut config.storm_threshold,
            positive,
        );
        parse_into(
            &read,
            "STORM_WINDOW_MINUTES",
            &mut config.storm_window_minutes,
            positive,
        );
        parse_into(
            &read,
            "SIGNATURE_WINDOW_DAYS",
            &mut config.signature_window_days,
            positive,
        );
        parse_into(
            &read,
            "RUN_TIMEOUT_MINUTES",
            &mut config.run_timeout_minutes,
            positive,
        );
        parse_into(&read, "MAX_ATTEMPTS", &mut config.max_attempts, positive);
        if let Some(exclude) = read("EXCLUDE") {
            config.exclude = list(&exclude, ['\n', ';']);
        }
        if let Some(recipients) = read("RECIPIENTS") {
            config.recipients = list(&recipients, [',', ';']);
        }
        if let Some(host) = read("POSTHOG_HOST") {
            config.posthog_host = host.trim_end_matches('/').to_string();
        }
        parse_into(
            &read,
            "POSTHOG_PROJECT_ID",
            &mut config.posthog_project_id,
            positive,
        );
        config.posthog_key = read("POSTHOG_KEY");
        if let Some(repo) = read("GITHUB_REPO") {
            config.github_repo = repo;
        }
        if let Some(workflow) = read("GITHUB_WORKFLOW") {
            config.github_workflow = workflow;
        }
        if let Some(git_ref) = read("GITHUB_REF") {
            config.github_ref = git_ref;
        }
        config.github_token = read("GITHUB_TOKEN");
        config.callback_secret = read("CALLBACK_SECRET");
        config.write_key = read("WRITE_KEY");
        if let Some(assignees) = read("ASSIGNEE") {
            config.assignees = list(&assignees, [',', ';'])
                .iter()
                .filter_map(|value| {
                    let parsed = Assignee::parse(value);
                    if parsed.is_none() {
                        tracing::warn!(
                            value,
                            "POSTHOG_AUTOFIX_ASSIGNEE entry is not user:<id> or role:<uuid>"
                        );
                    }
                    parsed
                })
                .collect();
        }
        config
    }

    /// Why the intake cannot run in the configured mode, or `None` when it can.
    /// Off needs nothing; a dry run needs the cutover and PostHog; live also
    /// needs GitHub.
    pub const fn missing_for_mode(&self) -> Option<&'static str> {
        match self.mode {
            Mode::Off => None,
            _ if self.start_at.is_none() => Some("POSTHOG_AUTOFIX_START_AT"),
            _ if self.posthog_key.is_none() => Some("POSTHOG_AUTOFIX_POSTHOG_KEY"),
            Mode::Live if self.github_token.is_none() => Some("POSTHOG_AUTOFIX_GITHUB_TOKEN"),
            Mode::DryRun | Mode::Live => None,
        }
    }

    /// The issue's page in PostHog.
    pub fn issue_url(&self, issue_id: &str) -> String {
        format!(
            "{}/project/{}/error-tracking/{issue_id}",
            self.posthog_host, self.posthog_project_id
        )
    }
}

fn parse_into<T>(
    read: &impl Fn(&str) -> Option<String>,
    name: &str,
    field: &mut T,
    parse: impl Fn(&str) -> Option<T>,
) {
    let Some(value) = read(name) else { return };
    if let Some(parsed) = parse(&value) {
        *field = parsed;
    } else {
        tracing::warn!(
            value,
            "POSTHOG_AUTOFIX_{name} is invalid; keeping the default"
        );
    }
}

fn positive<T: std::str::FromStr + PartialOrd + Default>(value: &str) -> Option<T> {
    value.parse().ok().filter(|parsed| *parsed > T::default())
}

fn list<const N: usize>(value: &str, separators: [char; N]) -> Vec<String> {
    value
        .split(separators)
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn config(vars: &[(&str, &str)]) -> Config {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(name, value)| (format!("POSTHOG_AUTOFIX_{name}"), (*value).to_string()))
            .collect();
        Config::from_lookup(|name| vars.get(name).cloned())
    }

    #[test]
    fn nothing_set_is_off_with_safe_caps() {
        let config = config(&[]);
        assert_eq!(config.mode, Mode::Off);
        assert_eq!((config.max_concurrent, config.max_per_day), (2, 8));
        assert_eq!(config.missing_for_mode(), None);
        assert!(
            config
                .exclude
                .iter()
                .any(|item| item.contains("Failed to fetch"))
        );
    }

    #[test]
    fn values_parse_and_bad_ones_keep_the_default() {
        let config = config(&[
            ("MODE", "dry_run"),
            ("START_AT", "2026-10-21T00:00:00Z"),
            ("MAX_PER_DAY", "5"),
            ("MAX_CONCURRENT", "0"),
            ("SETTLE_MINUTES", "soon"),
            ("RECIPIENTS", " a@x.com, b@y.com ,"),
            ("EXCLUDE", "Foo\nBar;  "),
        ]);
        assert_eq!(config.mode, Mode::DryRun);
        assert_eq!(config.max_per_day, 5);
        assert_eq!(config.max_concurrent, 2);
        assert_eq!(config.settle_minutes, 30);
        assert_eq!(config.recipients, ["a@x.com", "b@y.com"]);
        assert_eq!(config.exclude, ["Foo", "Bar"]);
        assert_eq!(
            config.missing_for_mode(),
            Some("POSTHOG_AUTOFIX_POSTHOG_KEY")
        );
    }

    #[test]
    fn live_needs_the_cutover_posthog_and_github() {
        assert_eq!(
            config(&[("MODE", "live")]).missing_for_mode(),
            Some("POSTHOG_AUTOFIX_START_AT")
        );
        let ready = config(&[
            ("MODE", "live"),
            ("START_AT", "2026-10-21T00:00:00+03:00"),
            ("POSTHOG_KEY", "phx_x"),
        ]);
        assert_eq!(
            ready.missing_for_mode(),
            Some("POSTHOG_AUTOFIX_GITHUB_TOKEN")
        );
    }

    #[test]
    fn production_noise_is_excluded_but_real_errors_are_not() {
        let excluded = |text: &str| {
            Config::default()
                .exclude
                .iter()
                .any(|pattern| text.to_lowercase().contains(&pattern.to_lowercase()))
        };
        for noise in [
            "TypeError: Failed to fetch",
            "Marketing Authorization Error: Failed to parse bearer UUID",
            r#"HTTPError: {"error":"cloudtalk_400","ids":[],"unsent_s3_keys":[]}"#,
            r#"HTTPError: {"error":"ringcentral_401","ids":[],"unsent_s3_keys":[]}"#,
            "HttpError: POST /v1/sms/ringcentral/messages answered 502",
            r#"ApiError: Upstream("Failed to generate AI reply: OpenAI request failed: error sending request")"#,
            r#"GraniteApiError: Granite API 502 on /v1/ai/conversation-reply: {"error":"Failed to generate AI reply: OpenAI request failed: error sending request"}"#,
            r#"Error: No result found for routeId "routes/employee.deals""#,
            "Error: Calendly widget failed to load",
            "SigningError: This signing link is not valid.",
            "Error: Failed to load notifications",
            "Error: Invalid call to runtime.sendMessage(). Tab not found.",
            "UnhandledRejection: Non-Error promise rejection captured with value: Object Not Found Matching Id:2, MethodName:update, ParamCount:4",
            "DOMException: SecurityError: Failed to read the 'localStorage' property from 'Window': Access is denied for this document.",
            "DOMException: InvalidStateError: Failed to execute 'transaction' on 'IDBDatabase': The database connection is closing.",
            "Error: [ChecklistDB] Database connection terminated unexpectedly",
            "Error: Error loading route module `/assets/employee.deals-Bq3x.js`, reloading page...",
            "TypeError: Script https://granitedepotindy.granite-manager.com/service-worker.js load failed",
            r#"GraniteApiError: Granite API 401 on /v1/me: {"error":"unauthorized"}"#,
            "Error: Failed to fetch",
            "FlowActionError: unauthorized",
            "Error: Telephony 401: unauthorized",
            r#"GraniteApiError: Granite API 429 on /v1/phone/web/devices/3/token: {"error":"rate_limited","retryAfterSecs":39}"#,
        ] {
            assert!(excluded(noise), "{noise}");
        }
        for bug in [
            r#"GraniteApiError: Granite API 500 on /v1/notifications/done: {"error":"database error"}"#,
            "GraniteApiError: Granite API 400 on /v1/sms/all/sales-reps: Invalid URL: unknown variant `all`",
            "TypeError: Cannot read properties of undefined (reading 'title')",
            "ApiError: Database(PoolTimedOut)",
            "HTTPError: Failed to build payload",
            // Kept out on purpose: not known to be noise.
            r#"HTTPError: {"error":"Email failed to send"}"#,
            "HTTPError: Failed to sync CloudTalk contact",
            r#"GraniteApiError: Granite API 500 on /v1/sms/cloudtalk/threads: {"error":"database error"}"#,
        ] {
            assert!(!excluded(bug), "{bug}");
        }
    }

    #[test]
    fn secrets_are_not_logged() {
        let config = config(&[("CALLBACK_SECRET", "s3cret"), ("POSTHOG_KEY", "phx_key")]);
        let printed = format!("{config:?}");
        assert!(!printed.contains("s3cret") && !printed.contains("phx_key"));
        assert!(printed.contains("<set>"));
    }

    #[test]
    fn write_back_takes_a_key_and_one_or_more_assignees() {
        let config = config(&[("WRITE_KEY", "phx_w"), ("ASSIGNEE", "user:42, user:7")]);
        assert_eq!(config.assignees, [Assignee::User(42), Assignee::User(7)]);
        assert!(!format!("{config:?}").contains("phx_w"));
        assert!(super::Config::from_lookup(|_| None).assignees.is_empty());
        let typo = config_with("ASSIGNEE", "dima, user:7");
        assert_eq!(typo.assignees, [Assignee::User(7)]);
    }

    fn config_with(name: &str, value: &str) -> Config {
        config(&[(name, value)])
    }

    #[test]
    fn issue_links_use_the_error_tracking_path() {
        assert_eq!(
            Config::default().issue_url("01a158ec-b8d5-7002-bb99-07e4e724acee"),
            "https://us.posthog.com/project/141685/error-tracking/01a158ec-b8d5-7002-bb99-07e4e724acee"
        );
    }
}
