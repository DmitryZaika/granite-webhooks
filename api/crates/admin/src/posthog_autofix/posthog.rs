//! Reading `$exception` events and error tracking issues from PostHog (HogQL
//! over the query API, with a personal API key that has `query:read`).

use super::config::Config;
use chrono::{DateTime, Utc};
use lambda_http::tracing;
use serde_json::{Value, json};

/// A row of `system.error_tracking_issues`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    /// The issue UUID.
    pub id: String,
    pub created_at: DateTime<Utc>,
    /// `active`, `resolved`, `suppressed`, ...
    pub status: String,
    /// The exception type.
    pub name: String,
    /// The exception message.
    pub description: String,
}

/// What the intake needs from an issue's `$exception` events.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IssueEvents {
    pub issue_id: String,
    pub occurrences: u64,
    /// Distinct `distinct_id`s (server errors share one, so this is 1 there).
    pub users: u64,
    /// The latest event's `distinct_id`, `$lib` and `$host`.
    pub distinct_id: Option<String>,
    pub lib: Option<String>,
    pub host: Option<String>,
    /// The latest event's `$exception_list`.
    pub exception_list: Option<Value>,
    /// The page of the latest browser error (`$current_url`).
    pub url: Option<String>,
    /// The latest API error's request, as `GET /v1/customers/7`.
    pub request: Option<String>,
}

/// `$exception` events of one time span that share an issue, a first
/// exception (type and message), a sender and the API call they failed on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EventGroup {
    pub issue_id: String,
    /// The first exception's type and message (cut to 300 characters).
    pub exception_type: String,
    pub message: String,
    /// `granite-api`, `server-webhooks` or `server-time-triggered`; `None`
    /// for a browser or anything else.
    pub sender: Option<String>,
    pub lib: Option<String>,
    /// `api_status`: the HTTP status of the failed API call.
    pub api_status: Option<u16>,
    /// `api_route` (else `api_path`) of a server error, `api_path` of a
    /// browser one.
    pub route: Option<String>,
    /// `error_source`: `toast`, `client`, `api`...
    pub error_source: Option<String>,
    pub occurrences: u64,
    /// Distinct `distinct_id`s.
    pub users: u64,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    /// The latest event's `uuid`, `$host`, `$exception_list`, page and method.
    pub event_uuid: Option<String>,
    pub host: Option<String>,
    pub exception_list: Option<Value>,
    pub url: Option<String>,
    pub method: Option<String>,
}

/// PostHog could not be read; the tick retries on the next poll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceError(pub String);

impl std::fmt::Display for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[allow(async_fn_in_trait)]
pub trait IssueSource: Sync {
    /// `$exception` events with `from <= timestamp < to`, grouped, the most
    /// frequent groups first, at most `limit` groups.
    async fn event_groups(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<EventGroup>, SourceError>;
    /// The current rows of these issues.
    async fn issues(&self, ids: &[String]) -> Result<Vec<Issue>, SourceError>;
    /// Event aggregates of these issues, over events since `since`.
    async fn events(
        &self,
        ids: &[String],
        since: DateTime<Utc>,
    ) -> Result<Vec<IssueEvents>, SourceError>;
}

/// The PostHog query API.
pub struct PostHog<'a> {
    pub config: &'a Config,
    pub http: &'a reqwest::Client,
}

impl IssueSource for PostHog<'_> {
    async fn event_groups(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<EventGroup>, SourceError> {
        Ok(parse_groups(
            &self.query(&groups_query(from, to, limit)).await?,
        ))
    }

    async fn issues(&self, ids: &[String]) -> Result<Vec<Issue>, SourceError> {
        let Some(list) = id_list(ids) else {
            return Ok(Vec::new());
        };
        // HogQL caps a query without LIMIT at 100 rows.
        let hogql = format!(
            "SELECT {ISSUE_COLUMNS} FROM system.error_tracking_issues \
             WHERE id IN ({list}) LIMIT {}",
            ids.len()
        );
        Ok(parse_issues(&self.query(&hogql).await?))
    }

    async fn events(
        &self,
        ids: &[String],
        since: DateTime<Utc>,
    ) -> Result<Vec<IssueEvents>, SourceError> {
        let Some(list) = id_list(ids) else {
            return Ok(Vec::new());
        };
        let hogql = format!(
            "SELECT issue_id, count(), uniq(distinct_id), argMax(distinct_id, timestamp), \
             argMax(properties.$lib, timestamp), argMax(properties.$host, timestamp), \
             argMax(properties.$exception_list, timestamp), \
             argMax(properties.$current_url, timestamp), \
             argMax(properties.api_method, timestamp), argMax(properties.api_path, timestamp) \
             FROM events WHERE event = '$exception' AND timestamp >= {} \
             AND issue_id IN ({list}) GROUP BY issue_id LIMIT {}",
            datetime_literal(since),
            ids.len()
        );
        Ok(parse_events(&self.query(&hogql).await?))
    }
}

/// The grouping [`parse_groups`] reads, in its SELECT order. A browser's
/// `api_path` keeps its ids; groups of one bug merge by signature later.
fn groups_query(from: DateTime<Utc>, to: DateTime<Utc>, limit: u32) -> String {
    format!(
        "SELECT issue_id, \
         properties.$exception_types[1] AS exception_type, \
         left(toString(properties.$exception_values[1]), {MESSAGE_CHARS}) AS message, \
         multiIf(distinct_id IN ('granite-api', 'server-webhooks', 'server-time-triggered'), \
                 distinct_id, '') AS sender, \
         properties.$lib AS lib, \
         properties.api_status AS api_status, \
         if(sender = '', properties.api_path, \
            coalesce(properties.api_route, properties.api_path)) AS route, \
         properties.error_source AS error_source, \
         count() AS occurrences, uniq(distinct_id), min(timestamp), max(timestamp), \
         argMax(uuid, timestamp), argMax(properties.$host, timestamp), \
         argMax(properties.$exception_list, timestamp), \
         argMax(properties.$current_url, timestamp), \
         argMax(properties.api_method, timestamp) \
         FROM events WHERE event = '$exception' AND timestamp >= {} AND timestamp < {} \
         GROUP BY issue_id, exception_type, message, sender, lib, api_status, route, error_source \
         ORDER BY occurrences DESC LIMIT {limit}",
        datetime_literal(from),
        datetime_literal(to)
    )
}

/// Characters of an exception message read.
const MESSAGE_CHARS: usize = 300;
/// The columns [`parse_issues`] reads, in this order.
const ISSUE_COLUMNS: &str = "id, created_at, status, name, description";
/// Characters of an error response kept in a [`SourceError`].
const BODY_EXCERPT: usize = 200;

impl PostHog<'_> {
    /// Runs one HogQL query and returns the response body.
    async fn query(&self, hogql: &str) -> Result<Value, SourceError> {
        let key = self
            .config
            .posthog_key
            .as_deref()
            .ok_or_else(|| SourceError("POSTHOG_AUTOFIX_POSTHOG_KEY is not set".into()))?;
        let response = self
            .http
            .post(query_url(self.config))
            .bearer_auth(key)
            .json(&query_body(hogql))
            .send()
            .await
            .map_err(|error| {
                SourceError(format!("PostHog unreachable: {}", error.without_url()))
            })?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            let excerpt: String = body.chars().take(BODY_EXCERPT).collect();
            return Err(SourceError(format!(
                "PostHog query failed: {status} {excerpt}"
            )));
        }
        response
            .json()
            .await
            .map_err(|error| SourceError(format!("PostHog answered with bad JSON: {error}")))
    }
}

fn query_url(config: &Config) -> String {
    format!(
        "{}/api/projects/{}/query/",
        config.posthog_host, config.posthog_project_id
    )
}

fn query_body(hogql: &str) -> Value {
    json!({
        "query": { "kind": "HogQLQuery", "query": hogql },
        "name": "posthog-autofix",
    })
}

/// A HogQL timestamp. The query API takes no bind parameters, so only values
/// formatted here or checked by [`is_issue_id`] are ever interpolated.
fn datetime_literal(at: DateTime<Utc>) -> String {
    format!(
        "toDateTime64('{}', 3, 'UTC')",
        at.format("%Y-%m-%d %H:%M:%S%.3f")
    )
}

/// A lowercase UUID, the only shape of a PostHog issue id.
pub fn is_issue_id(id: &str) -> bool {
    id.len() == 36
        && id.char_indices().all(|(at, c)| match at {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_digit() || ('a'..='f').contains(&c),
        })
}

/// A PostHog event `uuid` has the same shape as an issue id.
pub fn is_event_uuid(id: &str) -> bool {
    is_issue_id(id)
}

/// `'id', 'id'` for an `IN (...)`, or `None` when no id is valid. Invalid ids
/// are dropped: they would be interpolated into HogQL.
fn id_list(ids: &[String]) -> Option<String> {
    let valid: Vec<String> = ids
        .iter()
        .filter(|id| {
            let valid = is_issue_id(id);
            if !valid {
                tracing::warn!(id = %id, "not a PostHog issue id; left out of the query");
            }
            valid
        })
        .map(|id| format!("'{id}'"))
        .collect();
    (!valid.is_empty()).then(|| valid.join(", "))
}

fn rows(body: &Value) -> &[Value] {
    body["results"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
}

/// A cell as text; `None` for null and blank.
fn text(cell: &Value) -> Option<String> {
    match cell {
        Value::String(text) if !text.trim().is_empty() => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// ClickHouse may send a `UInt64` as a number or as a string.
fn count(cell: &Value) -> u64 {
    cell.as_u64()
        .or_else(|| cell.as_str().and_then(|text| text.parse().ok()))
        .unwrap_or(0)
}

/// `2026-10-20T10:47:51.135000Z`, `2026-10-20T10:47:51.135+00:00`, or a bare
/// `2026-10-20 10:47:51.135`, which is UTC.
fn timestamp(cell: &Value) -> Option<DateTime<Utc>> {
    let text = cell.as_str()?;
    DateTime::parse_from_rfc3339(text)
        .map(|at| at.with_timezone(&Utc))
        .or_else(|_| {
            chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S%.f")
                .map(|at| at.and_utc())
        })
        .ok()
}

/// A JSON property, which events store as a string holding JSON (sometimes
/// encoded twice).
fn json_cell(cell: &Value) -> Option<Value> {
    let mut value = cell.clone();
    for _ in 0..2 {
        let Value::String(encoded) = &value else {
            break;
        };
        value = serde_json::from_str(encoded).ok()?;
    }
    (!value.is_null()).then_some(value)
}

/// Rows of `SELECT {ISSUE_COLUMNS}`; a row without a valid id or creation
/// time is dropped and logged.
fn parse_issues(body: &Value) -> Vec<Issue> {
    rows(body)
        .iter()
        .filter_map(|row| {
            let issue = parse_issue(row);
            if issue.is_none() {
                tracing::warn!(row = %row, "unreadable PostHog issue row");
            }
            issue
        })
        .collect()
}

fn parse_issue(row: &Value) -> Option<Issue> {
    let id = row[0].as_str().filter(|id| is_issue_id(id))?.to_string();
    Some(Issue {
        id,
        created_at: timestamp(&row[1])?,
        status: text(&row[2]).unwrap_or_default(),
        name: text(&row[3]).unwrap_or_default(),
        description: text(&row[4]).unwrap_or_default(),
    })
}

/// Rows of the `events` aggregate, in its SELECT order.
fn parse_events(body: &Value) -> Vec<IssueEvents> {
    rows(body)
        .iter()
        .filter_map(|row| {
            Some(IssueEvents {
                issue_id: text(&row[0])?,
                occurrences: count(&row[1]),
                users: count(&row[2]),
                distinct_id: text(&row[3]),
                lib: text(&row[4]),
                host: text(&row[5]),
                exception_list: json_cell(&row[6]),
                url: text(&row[7]),
                request: request(&row[8], &row[9]),
            })
        })
        .collect()
}

/// Rows of [`groups_query`]; a row without a valid issue id or timestamps is
/// dropped and logged.
fn parse_groups(body: &Value) -> Vec<EventGroup> {
    rows(body)
        .iter()
        .filter_map(|row| {
            let group = parse_group(row);
            if group.is_none() {
                tracing::warn!("unreadable PostHog event group row");
            }
            group
        })
        .collect()
}

fn parse_group(row: &Value) -> Option<EventGroup> {
    let issue_id = row[0].as_str().filter(|id| is_issue_id(id))?.to_string();
    Some(EventGroup {
        issue_id,
        exception_type: text(&row[1]).unwrap_or_default(),
        message: text(&row[2]).unwrap_or_default(),
        sender: text(&row[3]),
        lib: text(&row[4]),
        api_status: http_status(&row[5]),
        route: text(&row[6]),
        error_source: text(&row[7]),
        occurrences: count(&row[8]),
        users: count(&row[9]),
        first_seen: timestamp(&row[10])?,
        last_seen: timestamp(&row[11])?,
        event_uuid: row[12]
            .as_str()
            .filter(|id| is_event_uuid(id))
            .map(str::to_string),
        host: text(&row[13]),
        exception_list: json_cell(&row[14]),
        url: text(&row[15]),
        method: text(&row[16]),
    })
}

/// `api_status` arrives as `500`, `500.0` or `"500"`.
fn http_status(cell: &Value) -> Option<u16> {
    let number = cell
        .as_f64()
        .or_else(|| cell.as_str().and_then(|text| text.trim().parse().ok()))?;
    (100.0..600.0)
        .contains(&number)
        .then(|| format!("{number:.0}").parse().ok())
        .flatten()
}

/// `GET /v1/customers/7` from the `api_method` and `api_path` cells; the
/// path alone without a method.
fn request(method: &Value, path: &Value) -> Option<String> {
    let path = text(path)?;
    Some(match text(method) {
        Some(method) => format!("{method} {path}"),
        None => path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const ISSUE: &str = "01a158ec-b8d5-7002-bb99-07e4e724acee";

    #[test]
    fn the_query_is_hogql_against_the_project() {
        assert_eq!(
            query_url(&Config::default()),
            "https://us.posthog.com/api/projects/141685/query/"
        );
        assert_eq!(
            query_body("SELECT 1"),
            json!({ "query": { "kind": "HogQLQuery", "query": "SELECT 1" }, "name": "posthog-autofix" })
        );
    }

    #[test]
    fn timestamps_are_formatted_not_passed_through() {
        let at = Utc.with_ymd_and_hms(2026, 10, 20, 10, 47, 51).unwrap()
            + chrono::Duration::milliseconds(135);
        assert_eq!(
            datetime_literal(at),
            "toDateTime64('2026-10-20 10:47:51.135', 3, 'UTC')"
        );
    }

    #[test]
    fn only_lowercase_uuids_reach_the_query() {
        assert!(is_issue_id(ISSUE));
        assert!(!is_issue_id(&ISSUE.to_uppercase()));
        assert!(!is_issue_id("01a158ec-b8d5-7002-bb99-07e4e724ace'"));
        assert!(!is_issue_id(
            "01a158ec-b8d5-7002-bb99-07e4e724acee' OR 1=1 --"
        ));
        assert!(!is_issue_id("01a158ecb-8d5-7002-bb99-07e4e724acee"));
        let ids = [ISSUE.to_string(), "x') OR ('1".to_string()];
        assert_eq!(id_list(&ids).as_deref(), Some(&*format!("'{ISSUE}'")));
        assert_eq!(id_list(&ids[1..]), None);
    }

    #[test]
    fn issue_rows_parse_both_timestamp_spellings() {
        let body = json!({
            "columns": ["id", "created_at", "status", "name", "description"],
            "results": [
                [ISSUE, "2026-10-20T10:47:51.135000Z", "active", "TypeError", "Failed to fetch"],
                ["01a158ec-b8d5-7002-bb99-07e4e724acef", "2026-10-20T10:47:52.5+00:00", "resolved", "ApiError", null],
                ["not-an-id", "2026-10-20T10:47:52Z", "active", "x", "y"],
                ["01a158ec-b8d5-7002-bb99-07e4e724ac00", "yesterday", "active", "x", "y"],
                ["01a158ec-b8d5-7002-bb99-07e4e724ac01", "2026-10-20 10:47:53.25", "active", "x", "y"],
            ],
        });
        let issues = parse_issues(&body);
        assert_eq!(issues.len(), 3);
        assert_eq!(
            issues[0],
            Issue {
                id: ISSUE.into(),
                created_at: Utc.with_ymd_and_hms(2026, 10, 20, 10, 47, 51).unwrap()
                    + chrono::Duration::milliseconds(135),
                status: "active".into(),
                name: "TypeError".into(),
                description: "Failed to fetch".into(),
            }
        );
        assert_eq!(
            issues[1].created_at,
            Utc.with_ymd_and_hms(2026, 10, 20, 10, 47, 52).unwrap()
                + chrono::Duration::milliseconds(500)
        );
        assert_eq!(issues[1].description, "");
        assert_eq!(
            issues[2].created_at,
            Utc.with_ymd_and_hms(2026, 10, 20, 10, 47, 53).unwrap()
                + chrono::Duration::milliseconds(250)
        );
        assert!(parse_issues(&json!({ "error": "nope" })).is_empty());
    }

    #[test]
    fn the_group_query_reads_a_half_open_span_of_formatted_times() {
        let from = Utc.with_ymd_and_hms(2026, 10, 26, 13, 40, 0).unwrap();
        let to = from + chrono::Duration::minutes(5);
        let hogql = groups_query(from, to, 500);
        assert!(hogql.contains(
            "timestamp >= toDateTime64('2026-10-26 13:40:00.000', 3, 'UTC') \
             AND timestamp < toDateTime64('2026-10-26 13:45:00.000', 3, 'UTC')"
        ));
        assert!(hogql.contains("properties.$exception_types[1] AS exception_type"));
        assert!(hogql.contains("left(toString(properties.$exception_values[1]), 300)"));
        assert!(hogql.ends_with("ORDER BY occurrences DESC LIMIT 500"));
    }

    #[test]
    fn group_rows_decode_as_posthog_sends_them() {
        let list = json!([{ "type": "ApiError", "value": "Database(PoolTimedOut)" }]);
        let body = json!({
            "results": [
                [ISSUE, "ApiError", "Database(PoolTimedOut)", "granite-api", null, 500.0,
                 "/v1/notifications/done", "api", 3, 1, "2026-10-26T13:45:26.569000Z",
                 "2026-10-26T13:57:28.314000Z", "01a17880-7a7a-740b-ac7c-30975f48657a", null,
                 list.to_string(), null, "GET"],
                ["01a17875-818e-75f0-b2ec-3527a8bd5283", "GraniteApiError",
                 "Granite API 404 on /v1/x: ", "", "web", "404", "/v1/x", "client", "2", "2",
                 "2026-10-26 13:45:27.017", "2026-10-26 13:45:27.017", "NOT-A-UUID",
                 "acme.granite-manager.com", null, "https://acme.granite-manager.com/x", null],
                ["not-an-id", "TypeError", "x", "", "web", null, null, null, 1, 1,
                 "2026-10-26T13:45:27Z", "2026-10-26T13:45:27Z", null, null, null, null, null],
            ],
        });
        let groups = parse_groups(&body);
        assert_eq!(groups.len(), 2);
        let api = &groups[0];
        assert_eq!(api.sender.as_deref(), Some("granite-api"));
        assert_eq!(
            (api.api_status, api.occurrences, api.users),
            (Some(500), 3, 1)
        );
        assert_eq!(api.route.as_deref(), Some("/v1/notifications/done"));
        assert_eq!(
            api.event_uuid.as_deref(),
            Some("01a17880-7a7a-740b-ac7c-30975f48657a")
        );
        assert_eq!(api.exception_list, Some(list));
        assert_eq!(api.method.as_deref(), Some("GET"));
        let web = &groups[1];
        assert_eq!(
            (web.sender.as_deref(), web.lib.as_deref()),
            (None, Some("web"))
        );
        assert_eq!(
            (web.api_status, web.occurrences, web.users),
            (Some(404), 2, 2)
        );
        assert_eq!(web.event_uuid, None, "only a lowercase uuid is kept");
        assert_eq!(web.first_seen, web.last_seen);
        assert_eq!(http_status(&json!(42)), None);
        assert_eq!(http_status(&json!("abc")), None);
    }

    #[test]
    fn event_rows_decode_counts_and_the_exception_list() {
        let list = json!([{ "type": "TypeError", "value": "Failed to fetch" }]);
        let encoded = list.to_string();
        let body = json!({
            "results": [
                [ISSUE, 12, 3, "user-1", "web", "granite-manager.com", encoded,
                 "https://granite-manager.com/employee/deals/7", null, null],
                ["01a158ec-b8d5-7002-bb99-07e4e724acef", "4", "1", "granite-api", "", null,
                 serde_json::to_string(&encoded).unwrap(), null, "GET", "/v1/slabs"],
                ["01a158ec-b8d5-7002-bb99-07e4e724ac00", 1, 1, null, null, null, "not json",
                 "", null, "/v1/deals"],
            ],
        });
        let events = parse_events(&body);
        assert_eq!(
            events[0],
            IssueEvents {
                issue_id: ISSUE.into(),
                occurrences: 12,
                users: 3,
                distinct_id: Some("user-1".into()),
                lib: Some("web".into()),
                host: Some("granite-manager.com".into()),
                exception_list: Some(list.clone()),
                url: Some("https://granite-manager.com/employee/deals/7".into()),
                request: None,
            }
        );
        assert_eq!((events[1].occurrences, events[1].users), (4, 1));
        assert_eq!(events[1].lib, None);
        assert_eq!(events[1].exception_list, Some(list));
        assert_eq!(events[1].request.as_deref(), Some("GET /v1/slabs"));
        assert_eq!(events[2].exception_list, None);
        assert_eq!(events[2].url, None);
        assert_eq!(events[2].request.as_deref(), Some("/v1/deals"));
    }
}
