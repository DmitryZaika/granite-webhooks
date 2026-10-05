use axum::http::{StatusCode, Uri};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fmt::Write;

use serde::Serialize;

#[derive(Serialize, Debug)]
pub struct PostHogEvent {
    pub api_key: String,
    pub event: String,       // "$exception" и т.п.
    pub distinct_id: String, // ВЕРХНИЙ УРОВЕНЬ
    #[serde(skip_serializing_if = "Option::is_none")]
    pub properties: Option<Properties>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>, // опускаем, если None
}

#[derive(Serialize, Debug, Default)]
pub struct Properties {
    #[serde(rename = "$exception_list", skip_serializing_if = "Option::is_none")]
    pub exception_list: Option<Vec<ExceptionItem>>,
    #[serde(
        rename = "$exception_fingerprint",
        skip_serializing_if = "String::is_empty"
    )]
    pub exception_fingerprint: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,

    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

#[derive(Serialize, Debug)]
pub struct ExceptionItem {
    #[serde(rename = "type")]
    pub exception_type: String,
    #[serde(rename = "value")]
    pub value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stacktrace: Option<StackTrace>,
}

#[derive(Serialize, Debug)]
pub struct StackTrace {
    #[serde(rename = "type")]
    pub kind: String, // "raw" | "resolved"
    pub frames: Vec<Frame>,
}

#[derive(Serialize, Debug)]
pub struct Frame {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lineno: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub colno: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_app: Option<bool>,
}

fn create_fingerprint(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value);
    let result = hasher.finalize();
    result.iter().fold(String::new(), |mut output, b| {
        let _ = write!(output, "{b:02X}");
        output
    })
}

/// True when `segment` is 8-4-4-4-12 hex (a UUID), case-insensitive.
fn is_uuid_like(segment: &str) -> bool {
    let bytes = segment.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    bytes.iter().enumerate().all(|(i, b)| match i {
        8 | 13 | 18 | 23 => *b == b'-',
        _ => b.is_ascii_hexdigit(),
    })
}

/// Reduces a URI to a grouping-friendly route pattern: path only (no scheme,
/// host or query), with any purely-numeric or UUID-shaped segment replaced by
/// `{id}`. An empty path becomes `/`.
pub fn route_pattern(uri: &Uri) -> String {
    let path = uri.path();
    if path.is_empty() || path == "/" {
        return "/".to_string();
    }

    path.split('/')
        .map(|segment| {
            if !segment.is_empty()
                && (segment.bytes().all(|b| b.is_ascii_digit()) || is_uuid_like(segment))
            {
                "{id}"
            } else {
                segment
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Build info attached to every event, read through a lookup closure so tests
/// never need to mutate process environment (unsafe in edition 2024).
fn build_info_extra<F>(lookup: F) -> HashMap<String, serde_json::Value>
where
    F: Fn(&str) -> Option<String>,
{
    let mut extra = HashMap::new();
    extra.insert(
        "lambda_function".to_string(),
        serde_json::Value::String(
            lookup("AWS_LAMBDA_FUNCTION_NAME").unwrap_or_else(|| "local".to_string()),
        ),
    );
    extra.insert(
        "lambda_version".to_string(),
        serde_json::Value::String(
            lookup("AWS_LAMBDA_FUNCTION_VERSION").unwrap_or_else(|| "local".to_string()),
        ),
    );
    extra.insert(
        "release".to_string(),
        serde_json::Value::String(option_env!("GIT_SHA").unwrap_or("unknown").to_string()),
    );
    extra
}

fn env_lookup(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

impl PostHogEvent {
    /// A plain named event (not `$exception`) from this Lambda. It creates no
    /// person profile for the shared `server-webhooks` distinct id.
    pub fn new_named_event(
        api_key: impl Into<String>,
        event: impl Into<String>,
        properties: HashMap<String, serde_json::Value>,
    ) -> Self {
        let mut extra = build_info_extra(env_lookup);
        extra.extend(properties);
        extra.insert(
            "$process_person_profile".to_string(),
            serde_json::Value::Bool(false),
        );
        Self {
            api_key: api_key.into(),
            event: event.into(),
            distinct_id: "server-webhooks".into(),
            properties: Some(Properties {
                extra,
                ..Properties::default()
            }),
            timestamp: None,
        }
    }

    pub fn should_report_http_exception(status: StatusCode) -> bool {
        status.is_server_error()
    }

    pub fn new_http_exception(
        api_key: impl Into<String>,
        value: impl Into<String>,
        status: StatusCode,
        uri: &Uri,
    ) -> Self {
        let value = value.into();
        let route = route_pattern(uri);
        let item = ExceptionItem {
            exception_type: "HTTPError".into(),
            value: value.clone(),
            stacktrace: Some(StackTrace {
                kind: "raw".into(),
                frames: vec![],
            }),
        };

        let fingerprint = create_fingerprint(&format!(
            "HTTPError|{}|{}|{}",
            status.as_u16(),
            route,
            value
        ));
        Self {
            api_key: api_key.into(),
            event: "$exception".into(),
            distinct_id: "server-webhooks".into(),
            properties: Some(Properties {
                exception_list: Some(vec![item]),
                exception_fingerprint: fingerprint,
                status: Some(status.as_u16()),
                path: Some(uri.to_string()),
                route: Some(route),
                extra: build_info_extra(env_lookup),
            }),
            timestamp: None,
        }
    }
    pub fn new_general_exception(
        api_key: impl Into<String>,
        value: impl Into<String> + std::fmt::Display,
        title: impl Into<String> + std::fmt::Display,
    ) -> Self {
        let fingerprint = create_fingerprint(&format!("{value}|{title}|"));
        let item = ExceptionItem {
            exception_type: title.into(),
            value: value.into(),
            stacktrace: Some(StackTrace {
                kind: "raw".into(),
                frames: vec![],
            }),
        };

        Self {
            api_key: api_key.into(),
            event: "$exception".into(),
            distinct_id: "server-webhooks".into(),
            properties: Some(Properties {
                exception_list: Some(vec![item]),
                exception_fingerprint: fingerprint,
                status: None,
                path: None,
                route: None,
                extra: build_info_extra(env_lookup),
            }),
            timestamp: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{StatusCode, Uri};

    fn fingerprint(event: &PostHogEvent) -> String {
        match &event.properties {
            Some(properties) => properties.exception_fingerprint.clone(),
            None => panic!("missing properties"),
        }
    }

    #[test]
    fn reports_only_server_errors() {
        assert!(!PostHogEvent::should_report_http_exception(
            StatusCode::NOT_FOUND
        ));
        assert!(!PostHogEvent::should_report_http_exception(
            StatusCode::BAD_REQUEST
        ));
        assert!(PostHogEvent::should_report_http_exception(
            StatusCode::INTERNAL_SERVER_ERROR
        ));
    }

    #[test]
    fn fingerprints_http_exceptions_by_response_body() {
        let uri: Uri = "/ses/receive-email".parse().unwrap();
        let insert = PostHogEvent::new_http_exception(
            "key",
            "Failed to insert email into the database",
            StatusCode::INTERNAL_SERVER_ERROR,
            &uri,
        );
        let s3_read = PostHogEvent::new_http_exception(
            "key",
            "Unable to read email content from S3",
            StatusCode::INTERNAL_SERVER_ERROR,
            &uri,
        );
        let insert_again = PostHogEvent::new_http_exception(
            "key",
            "Failed to insert email into the database",
            StatusCode::INTERNAL_SERVER_ERROR,
            &uri,
        );

        assert_ne!(fingerprint(&insert), fingerprint(&s3_read));
        assert_eq!(fingerprint(&insert), fingerprint(&insert_again));
    }

    #[test]
    fn fingerprints_equal_for_same_route_different_ids() {
        let uri_a: Uri = "https://abc123.lambda-url.us-east-2.on.aws/cloudtalk/sync/1/23"
            .parse()
            .unwrap();
        let uri_b: Uri = "https://abc123.lambda-url.us-east-2.on.aws/cloudtalk/sync/1/45"
            .parse()
            .unwrap();
        let a = PostHogEvent::new_http_exception(
            "key",
            "boom",
            StatusCode::INTERNAL_SERVER_ERROR,
            &uri_a,
        );
        let b = PostHogEvent::new_http_exception(
            "key",
            "boom",
            StatusCode::INTERNAL_SERVER_ERROR,
            &uri_b,
        );
        assert_eq!(fingerprint(&a), fingerprint(&b));
    }

    #[test]
    fn fingerprints_differ_for_different_bodies_same_route() {
        let uri: Uri = "https://abc123.lambda-url.us-east-2.on.aws/cloudtalk/sync/1/23"
            .parse()
            .unwrap();
        let a = PostHogEvent::new_http_exception(
            "key",
            "boom",
            StatusCode::INTERNAL_SERVER_ERROR,
            &uri,
        );
        let b = PostHogEvent::new_http_exception(
            "key",
            "kaboom",
            StatusCode::INTERNAL_SERVER_ERROR,
            &uri,
        );
        assert_ne!(fingerprint(&a), fingerprint(&b));
    }

    #[test]
    fn fingerprints_equal_for_different_hosts_same_path() {
        let uri_a: Uri = "https://staging.example.com/cloudtalk/sync/1/23"
            .parse()
            .unwrap();
        let uri_b: Uri = "https://production.example.com/cloudtalk/sync/1/23"
            .parse()
            .unwrap();
        let a = PostHogEvent::new_http_exception(
            "key",
            "boom",
            StatusCode::INTERNAL_SERVER_ERROR,
            &uri_a,
        );
        let b = PostHogEvent::new_http_exception(
            "key",
            "boom",
            StatusCode::INTERNAL_SERVER_ERROR,
            &uri_b,
        );
        assert_eq!(fingerprint(&a), fingerprint(&b));
    }

    #[test]
    fn route_pattern_normalizes_uuid_segment() {
        let uri: Uri = "https://example.com/leads/550e8400-E29B-41d4-A716-446655440000/assign"
            .parse()
            .unwrap();
        assert_eq!(route_pattern(&uri), "/leads/{id}/assign");
    }

    #[test]
    fn route_pattern_normalizes_numeric_segments() {
        let uri: Uri = "https://example.com/cloudtalk/sync/1/23".parse().unwrap();
        assert_eq!(route_pattern(&uri), "/cloudtalk/sync/{id}/{id}");
    }

    #[test]
    fn route_pattern_ignores_query_string() {
        let uri: Uri = "https://example.com/cloudtalk/sync/1/23?x=1&y=2"
            .parse()
            .unwrap();
        assert_eq!(route_pattern(&uri), "/cloudtalk/sync/{id}/{id}");
    }

    #[test]
    fn route_pattern_empty_path_is_root() {
        let uri: Uri = "https://example.com".parse().unwrap();
        assert_eq!(route_pattern(&uri), "/");
    }

    #[test]
    fn route_pattern_does_not_treat_short_hex_run_as_uuid() {
        // Same length digit run as the dash positions in a UUID would require,
        // but not actually UUID-shaped: must stay untouched.
        let uri: Uri = "https://example.com/users/abc-def".parse().unwrap();
        assert_eq!(route_pattern(&uri), "/users/abc-def");
    }

    #[test]
    fn events_carry_build_info_and_route() {
        let uri: Uri = "/x/1".parse().unwrap();
        let event = PostHogEvent::new_http_exception(
            "key",
            "boom",
            StatusCode::INTERNAL_SERVER_ERROR,
            &uri,
        );
        let props = event.properties.as_ref().expect("missing properties");
        assert!(props.extra.contains_key("lambda_function"));
        assert!(props.extra.contains_key("lambda_version"));
        assert!(props.extra.contains_key("release"));
        assert_eq!(props.route.as_deref(), Some("/x/{id}"));
        assert_eq!(props.path.as_deref(), Some("/x/1"));

        let general = PostHogEvent::new_general_exception("key", "boom", "Title");
        let general_props = general.properties.as_ref().expect("missing properties");
        assert!(general_props.extra.contains_key("lambda_function"));
        assert!(general_props.extra.contains_key("lambda_version"));
        assert!(general_props.extra.contains_key("release"));
        assert_eq!(general_props.route, None);
    }

    #[test]
    fn build_info_extra_uses_lookup_without_touching_process_env() {
        let extra = build_info_extra(|key| match key {
            "AWS_LAMBDA_FUNCTION_NAME" => Some("webhooks-prod".to_string()),
            "AWS_LAMBDA_FUNCTION_VERSION" => Some("7".to_string()),
            _ => None,
        });
        assert_eq!(
            extra.get("lambda_function").and_then(|v| v.as_str()),
            Some("webhooks-prod")
        );
        assert_eq!(
            extra.get("lambda_version").and_then(|v| v.as_str()),
            Some("7")
        );

        let defaults = build_info_extra(|_| None);
        assert_eq!(
            defaults.get("lambda_function").and_then(|v| v.as_str()),
            Some("local")
        );
        assert_eq!(
            defaults.get("lambda_version").and_then(|v| v.as_str()),
            Some("local")
        );
    }
}
