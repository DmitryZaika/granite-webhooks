//! A bug's signature: issues with the same signature are the same bug.
//!
//! PostHog splits one bug into many issues (one "Failed to fetch" became 110
//! issues in a month, split by per-deploy minified chunk frames), so the
//! signature leaves out everything that changes between occurrences or
//! deploys: ids, numbers, URLs, chunk hashes, line numbers, and minified
//! frames altogether.

use super::model::Source;
use regex::{Captures, Regex};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::sync::LazyLock;

/// What identifies a bug, independent of the PostHog issue it landed in.
#[derive(Debug, Clone, Copy)]
pub struct Fingerprint<'a> {
    /// The exception type (the issue's `name`), e.g. `TypeError`.
    pub exception_type: &'a str,
    /// The exception message (the issue's `description`).
    pub message: &'a str,
    pub source: Source,
    /// From [`top_frame`]; `None` when no frame is symbolicated.
    pub top_frame: Option<&'a str>,
}

const MAX_MESSAGE_CHARS: usize = 300;

// The patterns are literals, covered by the tests below.
static URL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"https?://\S+").expect("url regex"));
static QUERY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\?[^\s:]+").expect("query regex"));
static UUID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b")
        .expect("uuid regex")
});
static EMAIL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\w.+-]+@[\w-]+(?:\.[\w-]+)+").expect("email regex"));
static HEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b[0-9a-f]{8,}\b").expect("hex regex"));
static PATH_NUMBER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/\d+\b").expect("path number regex"));
static NUMBER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\d+(?:\.\d+)?").expect("number regex"));
static CHUNK_HASH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"-[A-Za-z0-9_]{8,}(\.[A-Za-z0-9]+)$").expect("chunk hash regex"));
static LINE_COLUMN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?::\d+){1,2}$").expect("line column regex"));

/// The message with its variable parts replaced by placeholders.
pub fn normalize_message(message: &str) -> String {
    let text = mask_quoted(message);
    let text = URL.replace_all(&text, "<url>");
    let text = QUERY.replace_all(&text, "");
    let text = UUID.replace_all(&text, "<uuid>");
    let text = EMAIL.replace_all(&text, "<email>");
    let text = HEX.replace_all(&text, |caps: &Captures<'_>| {
        if caps[0].bytes().any(|b| b.is_ascii_digit()) {
            "<hex>".to_owned()
        } else {
            caps[0].to_owned()
        }
    });
    let text = PATH_NUMBER.replace_all(&text, "/<n>");
    let text = mask_numbers(&text);
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(MAX_MESSAGE_CHARS).collect()
}

/// Replaces the inside of quoted values that vary (ids, minified names,
/// sentences) and keeps short identifiers, which name the bug.
fn mask_quoted(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let quote = chars[i];
        let opens =
            matches!(quote, '\'' | '"' | '`') && (i == 0 || !chars[i - 1].is_alphanumeric());
        let close = opens.then(|| closing_quote(&chars, i)).flatten();
        let Some(close) = close else {
            out.push(quote);
            i += 1;
            continue;
        };
        let inner: String = chars[i + 1..close].iter().collect();
        out.push(quote);
        // A quote inside means a sentence around other values: look inside.
        if inner.contains(['\'', '"', '`']) {
            out.push_str(&mask_quoted(&inner));
        } else if is_variable(&inner) {
            out.push_str("<v>");
        } else {
            out.push_str(&inner);
        }
        out.push(quote);
        i = close + 1;
    }
    out
}

/// The next `chars[open]` that is not an apostrophe inside a word.
fn closing_quote(chars: &[char], open: usize) -> Option<usize> {
    (open + 1..chars.len()).find(|&j| {
        chars[j] == chars[open] && chars.get(j + 1).is_none_or(|next| !next.is_alphanumeric())
    })
}

fn is_variable(inner: &str) -> bool {
    let len = inner.chars().count();
    len <= 2
        || len > 40
        || inner
            .chars()
            .any(|c| c.is_ascii_digit() || c.is_whitespace() || matches!(c, '(' | '/' | '@'))
}

/// Numbers become `<n>`; error codes (`#418`) and HTTP statuses stay.
fn mask_numbers(text: &str) -> String {
    NUMBER
        .replace_all(text, |caps: &Captures<'_>| {
            let found = &caps[0];
            let Some(whole) = caps.get(0) else {
                return found.to_owned();
            };
            let before = text[..whole.start()].chars().next_back();
            let after = text[whole.end()..].chars().next();
            let is_word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
            let keep = before == Some('#')
                || (is_http_status(found) && !is_word(before) && !is_word(after));
            if keep {
                found.to_owned()
            } else {
                "<n>".to_owned()
            }
        })
        .into_owned()
}

fn is_http_status(digits: &str) -> bool {
    digits.len() == 3
        && digits
            .parse::<u16>()
            .is_ok_and(|n| (100..=599).contains(&n))
}

/// The top in-app frame of the first exception in `$exception_list`.
///
/// Returned as `function@file` without chunk hash, line or column, or `None`
/// when that frame is not resolved (minified frames change with every deploy).
pub fn top_frame(exception_list: &Value) -> Option<String> {
    let frames = exception_list
        .get(0)?
        .pointer("/stacktrace/frames")?
        .as_array()?;
    // Frames run outermost first, so the last in-app frame is the top.
    let frame = frames
        .iter()
        .rev()
        .find(|frame| frame.get("in_app").and_then(Value::as_bool) == Some(true))?;
    if frame.get("resolved").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let function = first_str(frame, &["resolved_name", "name", "function"])?;
    let file = clean_file(first_str(frame, &["source", "filename"])?);
    (!file.is_empty()).then(|| format!("{function}@{file}"))
}

fn first_str<'a>(frame: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .filter_map(|key| frame.get(key).and_then(Value::as_str))
        .find(|value| !value.is_empty())
}

fn clean_file(source: &str) -> String {
    let no_query = source.split(['?', '#']).next().unwrap_or(source);
    let no_origin = no_query.split_once("://").map_or(no_query, |(_, rest)| {
        rest.split_once('/').map_or("", |(_, path)| path)
    });
    let no_position = LINE_COLUMN.replace(no_origin, "");
    let path = no_position.trim_start_matches('/');
    CHUNK_HASH.replace(path, "$1").into_owned()
}

/// Hex characters of a signature in a bug id.
const BUG_ID_CHARS: usize = 12;

/// The short id people see for a bug (emails, pull requests, branch names):
/// the start of its signature.
pub fn bug_id(signature: &str) -> &str {
    signature.get(..BUG_ID_CHARS).unwrap_or(signature)
}

/// SHA-256 (lowercase hex) of the normalized fingerprint.
pub fn signature(fingerprint: &Fingerprint<'_>) -> String {
    let material = [
        fingerprint.exception_type.trim(),
        &normalize_message(fingerprint.message),
        fingerprint.source.as_str(),
        fingerprint.top_frame.unwrap_or(""),
    ]
    .join("\n");
    Sha256::digest(material.as_bytes())
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashSet;

    fn sig(exception_type: &str, message: &str, source: Source) -> String {
        signature(&Fingerprint {
            exception_type,
            message,
            source,
            top_frame: None,
        })
    }

    fn api(message: &str) -> String {
        sig("Error", message, Source::Api)
    }

    /// The 110 PostHog issues differ only in their minified, per-deploy frames.
    #[test]
    fn a_hundred_failed_to_fetch_issues_are_one_bug() {
        let chunks = [
            "removable-DmdtJoVB",
            "useQuery-DKu96_Yo",
            "DelayedSendButton-CQ0nMSvo",
        ];
        let signatures: HashSet<String> = (0..110)
            .map(|issue| {
                let source = format!(
                    "https://granitedepotindy.granite-manager.com/assets/{}.js",
                    chunks[issue % chunks.len()]
                );
                let frame = json!({
                    "in_app": true, "resolved": false, "mangled_name": "?",
                    "column": 6624 + issue, "line": 1, "source": source,
                });
                let top = top_frame(&list(&[frame]));
                signature(&Fingerprint {
                    exception_type: "TypeError",
                    message: "Failed to fetch",
                    source: Source::Web,
                    top_frame: top.as_deref(),
                })
            })
            .collect();
        assert_eq!(signatures.len(), 1);
    }

    #[test]
    fn the_source_tells_bugs_apart() {
        assert_ne!(
            sig("TypeError", "Failed to fetch", Source::Web),
            sig("TypeError", "Failed to fetch", Source::Api)
        );
    }

    #[test]
    fn different_messages_differ() {
        assert_ne!(
            sig("Error", "CloudTalk API credentials not found", Source::Api),
            sig("Error", "Failed to fetch", Source::Api)
        );
    }

    #[test]
    fn api_errors_ignore_query_ids_but_not_paths_or_statuses() {
        let a = api(r#"Granite API 500 on /v1/slabs?id=6074: {"error":"database error"}"#);
        let b = api(r#"Granite API 500 on /v1/slabs?id=7001: {"error":"database error"}"#);
        let other_path = api(r#"Granite API 500 on /v1/quotes/12: {"error":"database error"}"#);
        let other_status =
            api(r#"Granite API 400 on /v1/slabs?id=6074: {"error":"database error"}"#);
        assert_eq!(a, b);
        assert_ne!(a, other_path);
        assert_ne!(a, other_status);
    }

    #[test]
    fn a_missing_table_is_told_by_its_name() {
        let message = |table: &str| {
            format!(
                "Database(Database(MySqlDatabaseError {{ code: Some(\"42S02\"), number: 1146, \
                 message: \"Table '{table}' doesn't exist\" }}))"
            )
        };
        assert_ne!(
            api(&message("main.schedule_notification_flows")),
            api(&message("main.other_table"))
        );
        assert_eq!(
            api(&message("main.other_table")),
            api(&message("main.other_table"))
        );
    }

    #[test]
    fn quoted_identifiers_stay_and_minified_names_go() {
        let read = |name: &str| {
            api(&format!(
                "Cannot read properties of undefined (reading '{name}')"
            ))
        };
        assert_ne!(read("getFieldState"), read("foo"));
        let destructure = |name: &str| {
            api(&format!(
                "Cannot destructure property 'getFieldState' of '{name}(...)' as it is null."
            ))
        };
        assert_eq!(destructure("oe"), destructure("Xe"));
    }

    #[test]
    fn react_error_codes_stay() {
        let react = |code: u16| {
            api(&format!(
                "Minified React error #{code}; visit https://react.dev/errors/{code}?args[]=text for the full message"
            ))
        };
        assert_ne!(react(418), react(423));
        assert_eq!(react(418), react(418));
    }

    #[test]
    fn normalizes_each_kind_of_value() {
        let cases = [
            ("see https://a.b/c?d=1 now", "see <url> now"),
            (
                "id 123e4567-e89b-12d3-a456-426614174000 bad",
                "id <uuid> bad",
            ),
            ("mail bob@example.com failed", "mail <email> failed"),
            ("token deadbeef12 and deadbeef", "token <hex> and deadbeef"),
            ("GET /slabs/42/edit", "GET /slabs/<n>/edit"),
            ("got 5 of 1234 (HTTP 404)", "got <n> of <n> (HTTP 404)"),
            ("value 1500 and 2.50", "value <n> and <n>"),
            ("a   b \n c", "a b c"),
        ];
        for (input, expected) in cases {
            assert_eq!(normalize_message(input), expected, "{input}");
        }
    }

    #[test]
    fn quoted_values_follow_the_masking_rules() {
        let cases = [
            ("'oe(...)'", "'<v>'"),
            ("\"42S02\"", "\"<v>\""),
            ("'ab'", "'<v>'"),
            ("'getFieldState'", "'getFieldState'"),
            ("`some words`", "`<v>`"),
            ("doesn't work, can't either", "doesn't work, can't either"),
        ];
        for (input, expected) in cases {
            assert_eq!(normalize_message(input), expected, "{input}");
        }
    }

    #[test]
    fn long_messages_are_cut_on_a_char_boundary() {
        let long = "é".repeat(500);
        assert_eq!(normalize_message(&long).chars().count(), 300);
    }

    fn frame(extra: &Value) -> Value {
        let mut base = json!({"in_app": true, "resolved": true});
        if let (Some(base), Some(extra)) = (base.as_object_mut(), extra.as_object()) {
            base.extend(extra.clone());
        }
        base
    }

    fn list(frames: &[Value]) -> Value {
        json!([{"type": "TypeError", "value": "x", "stacktrace": {"frames": frames}}])
    }

    #[test]
    fn an_unresolved_frame_has_no_top_frame() {
        let minified = json!({
            "column": 6624, "in_app": true, "lang": "javascript", "line": 1,
            "mangled_name": "?", "resolved": false,
            "source": "https://granitedepotindy.granite-manager.com/assets/removable-DmdtJoVB.js"
        });
        assert_eq!(top_frame(&list(&[minified])), None);
    }

    #[test]
    fn a_resolved_frame_loses_origin_hash_and_position() {
        let resolved = frame(&json!({
            "resolved_name": "name",
            "source": "https://granitedepotindy.granite-manager.com/assets/removable-DmdtJoVB.js:12:34?v=1",
        }));
        assert_eq!(
            top_frame(&list(&[resolved])).as_deref(),
            Some("name@assets/removable.js")
        );
    }

    #[test]
    fn the_last_in_app_frame_is_the_top() {
        let frames = [
            frame(&json!({"name": "outer", "filename": "app/a.ts"})),
            frame(&json!({"name": "inner", "filename": "app/b.ts"})),
            json!({"in_app": false, "resolved": true, "name": "lib", "filename": "node_modules/x.js"}),
        ];
        assert_eq!(top_frame(&list(&frames)).as_deref(), Some("inner@app/b.ts"));
    }

    #[test]
    fn malformed_exception_lists_give_none() {
        for value in [
            json!(null),
            json!("garbage"),
            json!([]),
            json!([{"stacktrace": 5}]),
            json!([{"stacktrace": {"frames": "x"}}]),
            list(&[json!(7), frame(&json!({"name": 3}))]),
        ] {
            assert_eq!(top_frame(&value), None, "{value}");
        }
    }

    #[test]
    fn the_signature_is_lowercase_sha256_hex() {
        let value = sig("TypeError", "Failed to fetch", Source::Web);
        assert_eq!(value.len(), 64);
        assert!(
            value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        );
    }
}
