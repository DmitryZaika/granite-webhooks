//! `GET /v1/users/call-activity`: the `CloudTalk` and `RingCentral` calls of
//! chosen reps over the last 90 days, for the admin users-activity page.
//! Remix: `api.users.activity.calls.tsx`.

use super::activity::{customers_by_phone, last10};
use crate::auth::AdminUser;
use crate::error::{ApiResult, ErrorBody};
use crate::routes::calls::ringcentral::map_record;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Query, State};
use common::telephony::{cloudtalk, ringcentral};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use utoipa::{IntoParams, ToSchema};

const PAGE_SIZE: usize = 200;
const MAX_PAGES_PER_AGENT: usize = 5;
const DAYS_BACK: i64 = 90;
const RINGCENTRAL_PAGE_SIZE: usize = 1000;
const RINGCENTRAL_MAX_PAGES: usize = 5;

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CallActivityQuery {
    /// Comma-separated user ids.
    pub user_ids: String,
}

/// One call, as a row of the users-activity feed.
#[derive(Debug, Serialize, ToSchema)]
pub struct CallActivity {
    /// The `CloudTalk` call id; 0 for `RingCentral`, whose ids are not numbers.
    pub id: i64,
    /// The provider's call id as text.
    pub call_id: String,
    /// `cloudtalk_call` or `ringcentral_call`.
    pub source: &'static str,
    /// The rep.
    pub user_id: i32,
    /// The rep's name.
    pub user_name: String,
    /// `Made a call`, `Received a call` or `Missed a call`.
    pub action: &'static str,
    /// When the call started, as `CloudTalk` reports it.
    pub created_at: String,
    /// The `CloudTalk` contact's name, or for `RingCentral` the customer with
    /// that number (else the caller-ID name).
    pub customer_name: Option<String>,
    /// The customer with that number (`RingCentral` only).
    pub customer_id: Option<i64>,
    /// The other party's number.
    pub contact: Option<String>,
    /// Direction and outcome, like `Incoming · 2m 5s`.
    pub summary: String,
    /// The call's notes, one per line.
    pub body: Option<String>,
    /// Direction, outcome, `Recorded`, then `CloudTalk` tags.
    pub tags: Vec<String>,
    /// Whether there is a recording.
    pub recorded: bool,
    /// `CloudTalk`'s recording link, or a `RingCentral` media token (`rec_<id>`).
    pub recording_link: Option<String>,
    /// The caller left a voicemail.
    pub is_voicemail: bool,
    /// Seconds talked; 0 when unanswered.
    pub talk_seconds: i64,
}

fn text(value: &Value) -> String {
    match value {
        Value::String(text) => text.trim().to_string(),
        Value::Number(number) => number.to_string(),
        _ => String::new(),
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0),
        Value::String(text) => !text.is_empty(),
        _ => false,
    }
}

fn non_empty(value: &Value) -> Option<String> {
    Some(text(value)).filter(|text| !text.is_empty())
}

fn duration(seconds: i64) -> String {
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let (minutes, rest) = (seconds / 60, seconds % 60);
    if rest > 0 {
        format!("{minutes}m {rest}s")
    } else {
        format!("{minutes}m")
    }
}

/// Maps one `calls/index.json` item.
pub fn activity(item: &Value, user_id: i32, user_name: &str) -> CallActivity {
    let cdr = &item["Cdr"];
    let kind = cdr["type"].as_str().unwrap_or("");
    let voicemail = truthy(&cdr["is_voicemail"]);
    let talk_seconds = text(&cdr["talking_time"]).parse::<f64>().unwrap_or(0.0);
    #[allow(clippy::cast_possible_truncation)]
    let talk_seconds = if talk_seconds.is_finite() && talk_seconds > 0.0 {
        talk_seconds as i64
    } else {
        0
    };
    let direction = match kind {
        "incoming" => "Incoming",
        "internal" => "Internal",
        _ => "Outgoing",
    };
    let outcome = if talk_seconds > 0 {
        duration(talk_seconds)
    } else if voicemail {
        "Voicemail".into()
    } else if kind == "incoming" {
        "Missed".into()
    } else {
        "No answer".into()
    };
    let action = if kind != "incoming" {
        "Made a call"
    } else if voicemail || talk_seconds == 0 {
        "Missed a call"
    } else {
        "Received a call"
    };
    let recorded = truthy(&cdr["recorded"]);
    let mut tags = vec![direction.to_string(), outcome.clone()];
    if recorded {
        tags.push("Recorded".into());
    }
    let names = |key: &str| -> Vec<String> {
        item[key]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|entry| non_empty(&entry["name"]))
            .collect()
    };
    tags.extend(names("Tags"));
    let notes = names("Notes");
    CallActivity {
        id: text(&cdr["id"]).parse().unwrap_or(0),
        call_id: text(&cdr["id"]),
        source: "cloudtalk_call",
        user_id,
        user_name: user_name.to_string(),
        action,
        created_at: text(&cdr["started_at"]),
        customer_name: non_empty(&item["Contact"]["name"]),
        customer_id: None,
        contact: non_empty(&cdr["public_external"]),
        summary: format!("{direction} · {outcome}"),
        body: (!notes.is_empty()).then(|| notes.join("\n")),
        tags,
        recorded,
        recording_link: non_empty(&cdr["recording_link"]),
        is_voicemail: voicemail,
        talk_seconds,
    }
}

/// Every call of one agent since `date_from`; empty when `CloudTalk` fails.
pub async fn agent_calls(
    state: &AppState,
    company_id: i32,
    agent_id: &str,
    date_from: &str,
) -> Vec<Value> {
    let api = cloudtalk::Api {
        pool: &state.pool,
        client: &state.telephony_http,
        base_url: &state.cloudtalk_base,
        company_id,
    };
    let agent = percent_encoding::utf8_percent_encode(agent_id, percent_encoding::NON_ALPHANUMERIC);
    let mut seen = HashSet::new();
    let mut items = Vec::new();
    for page in 1..=MAX_PAGES_PER_AGENT {
        let path = format!(
            "calls/index.json?limit={PAGE_SIZE}&pageNumber={page}&date_from={date_from}&user_id={agent}"
        );
        let Ok(body) = cloudtalk::request(api, Method::GET, &path, None, None).await else {
            return Vec::new();
        };
        let page_items = body["responseData"]["data"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let count = page_items.len();
        let mut added = 0;
        for item in page_items {
            if seen.insert(text(&item["Cdr"]["id"])) {
                items.push(item);
                added += 1;
            }
        }
        if count < PAGE_SIZE || added == 0 {
            break;
        }
    }
    items
}

/// The rep's extension on a `RingCentral` call-log row.
fn ringcentral_extension(record: &Value) -> String {
    [
        &record["extension"]["id"],
        &record["from"]["extensionId"],
        &record["to"]["extensionId"],
    ]
    .into_iter()
    .map(text)
    .find(|id| !id.is_empty())
    .unwrap_or_default()
}

/// Maps one `RingCentral` call-log row. Like the call-points job, only a
/// connected call (or a voicemail the rep left) has talk time; the rest of
/// `duration` is ringing.
pub fn ringcentral_activity(record: &Value, user_id: i32, user_name: &str) -> CallActivity {
    let item = ringcentral_item(record);
    let mut call = activity(&item, user_id, user_name);
    call.id = 0;
    call.source = "ringcentral_call";
    call
}

/// One `RingCentral` call-log row as a `CloudTalk` `calls/index.json` item,
/// with talk time only on a connected call and the other party as `Contact`.
pub fn ringcentral_item(record: &Value) -> Value {
    let mut record = record.clone();
    let outbound = text(&record["direction"]).eq_ignore_ascii_case("outbound");
    let result = text(&record["result"]).to_ascii_lowercase();
    let connected = matches!(result.as_str(), "call connected" | "accepted")
        || (outbound && result == "voicemail");
    if !connected {
        record["duration"] = json!(0);
    }
    let mut item = map_record(&record, None);
    item["Contact"]["name"] = json!(text(&record[if outbound { "to" } else { "from" }]["name"]));
    item
}

/// The `RingCentral` call log since `date_from` (RFC 3339) of the whole
/// company, or of one extension; empty when `RingCentral` fails.
pub async fn company_ringcentral_calls(
    state: &AppState,
    company_id: i32,
    extension: Option<&str>,
    date_from: &str,
) -> Vec<Value> {
    let encode = |value: &str| {
        percent_encoding::utf8_percent_encode(value, percent_encoding::NON_ALPHANUMERIC).to_string()
    };
    let from = encode(date_from);
    let scope = extension.map_or_else(
        || "account/~".to_string(),
        |extension| format!("account/~/extension/{}", encode(extension)),
    );
    let mut path =
        format!("{scope}/call-log?view=Detailed&dateFrom={from}&perPage={RINGCENTRAL_PAGE_SIZE}");
    let mut seen = HashSet::new();
    let mut records = Vec::new();
    for _ in 0..RINGCENTRAL_MAX_PAGES {
        let Ok(body) = ringcentral::request(
            &state.pool,
            &state.telephony_http,
            company_id,
            Method::GET,
            &path,
            &ringcentral::Body::Empty,
            None,
        )
        .await
        else {
            return Vec::new();
        };
        for record in body["records"].as_array().into_iter().flatten() {
            if seen.insert(text(&record["id"])) {
                records.push(record.clone());
            }
        }
        // The next page is a full URI; keep what follows the API root.
        let Some(next) = body["navigation"]["nextPage"]["uri"]
            .as_str()
            .and_then(|uri| uri.split_once("/restapi/v1.0/"))
            .map(|(_, rest)| rest.to_string())
        else {
            break;
        };
        path = next;
    }
    records
}

/// List reps' calls
///
/// The `CloudTalk` and `RingCentral` calls of the given users of the caller's
/// company over the last 90 days, newest first. Users without a `CloudTalk`
/// agent or `RingCentral` extension, and companies without either, have none.
#[utoipa::path(
    get,
    path = "/v1/users/call-activity",
    operation_id = "list_user_call_activity",
    tag = "users",
    params(CallActivityQuery),
    responses(
        (status = 200, description = "Calls, newest first", body = [CallActivity]),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn call_activity(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Query(query): Query<CallActivityQuery>,
) -> ApiResult<Json<Vec<CallActivity>>> {
    let ids: Vec<i32> = query
        .user_ids
        .split(',')
        .filter_map(|id| id.trim().parse().ok())
        .filter(|id| *id > 0)
        .collect();
    if ids.is_empty() {
        return Ok(Json(Vec::new()));
    }
    let mut builder = sqlx::QueryBuilder::<sqlx::MySql>::new(
        "SELECT id, name, cloudtalk_agent_id, ringcentral_extension_id FROM users
          WHERE is_deleted = 0 AND company_id = ",
    );
    builder.push_bind(user.company_id).push(" AND id IN (");
    let mut list = builder.separated(", ");
    for id in &ids {
        list.push_bind(id);
    }
    builder.push(")");
    let reps: Vec<(i32, Option<String>, Option<String>, Option<String>)> =
        builder.build_query_as().fetch_all(&state.pool).await?;
    let since = chrono::Utc::now() - chrono::Duration::days(DAYS_BACK);
    let date_from = since.format("%Y-%m-%d").to_string();
    let trimmed = |value: Option<String>| {
        value
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    };
    let mut tasks = Vec::new();
    // RingCentral extension → (user id, name).
    let mut extensions = HashMap::new();
    for (id, name, agent, extension) in reps {
        let name = name.unwrap_or_default();
        if let Some(extension) = trimmed(extension) {
            extensions.insert(extension, (id, name.clone()));
        }
        let Some(agent) = trimmed(agent) else {
            continue;
        };
        let (state, date_from) = (state.clone(), date_from.clone());
        let company_id = user.company_id;
        tasks.push(tokio::spawn(async move {
            agent_calls(&state, company_id, &agent, &date_from)
                .await
                .iter()
                .map(|item| activity(item, id, &name))
                .collect::<Vec<_>>()
        }));
    }
    let mut calls = Vec::new();
    if !extensions.is_empty() {
        let date_from = since.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let records = company_ringcentral_calls(&state, user.company_id, None, &date_from).await;
        let mut ringcentral_calls: Vec<CallActivity> = records
            .iter()
            .filter_map(|record| {
                let (id, name) = extensions.get(&ringcentral_extension(record))?;
                Some(ringcentral_activity(record, *id, name))
            })
            .collect();
        if !ringcentral_calls.is_empty() {
            let customers = customers_by_phone(&state.pool, user.company_id).await?;
            for call in &mut ringcentral_calls {
                let customer = call
                    .contact
                    .as_deref()
                    .and_then(last10)
                    .and_then(|key| customers.get(&key));
                if let Some(customer) = customer {
                    call.customer_id = Some(customer.id);
                    if customer.name.is_some() {
                        call.customer_name.clone_from(&customer.name);
                    }
                }
            }
        }
        calls.extend(ringcentral_calls);
    }
    for task in tasks {
        calls.extend(task.await.unwrap_or_default());
    }
    calls.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(Json(calls))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_ringcentral_calls() {
        let answered = json!({
            "id": "Y3s3vKBq", "direction": "Inbound", "result": "Accepted", "duration": 125,
            "startTime": "2026-05-01T10:00:00.000Z", "extension": { "id": 601 },
            "from": { "phoneNumber": "+13175550100", "name": "ALICE LEAD" },
            "to": { "phoneNumber": "+13175550998", "name": "Rita Rep" },
            "recording": { "id": "77" },
        });
        assert_eq!(ringcentral_extension(&answered), "601");
        let call = ringcentral_activity(&answered, 100, "Rita Rep");
        assert_eq!(call.source, "ringcentral_call");
        assert_eq!(call.call_id, "Y3s3vKBq");
        assert_eq!(call.action, "Received a call");
        assert_eq!(call.summary, "Incoming · 2m 5s");
        assert_eq!(call.customer_name.as_deref(), Some("ALICE LEAD"));
        assert_eq!(call.contact.as_deref(), Some("+13175550100"));
        assert_eq!(call.recording_link.as_deref(), Some("rec_77"));
        assert!(call.recorded);

        // Ringing time is not talk time.
        let missed = json!({
            "id": "a1", "direction": "Inbound", "result": "Missed", "duration": 20,
            "startTime": "2026-05-02T10:00:00.000Z",
            "from": { "phoneNumber": "+13175550100" }, "to": { "extensionId": "601" },
        });
        assert_eq!(ringcentral_extension(&missed), "601");
        let call = ringcentral_activity(&missed, 100, "Rita Rep");
        assert_eq!(call.action, "Missed a call");
        assert_eq!(call.talk_seconds, 0);

        let unanswered = json!({
            "id": "a2", "direction": "Outbound", "result": "No Answer", "duration": 30,
            "startTime": "2026-05-03T10:00:00.000Z",
            "from": { "extensionId": "601", "name": "Rita Rep" },
            "to": { "phoneNumber": "+13175550100", "name": "Alice" },
        });
        let call = ringcentral_activity(&unanswered, 100, "Rita Rep");
        assert_eq!(call.action, "Made a call");
        assert_eq!(call.summary, "Outgoing · No answer");
        assert_eq!(call.customer_name.as_deref(), Some("Alice"));
    }
}
