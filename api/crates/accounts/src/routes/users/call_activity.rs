//! `GET /v1/users/call-activity`: the `CloudTalk`, `RingCentral` and Granite
//! Phone calls of chosen reps over the last 90 days, for the admin
//! users-activity page.
//! Remix: `api.users.activity.calls.tsx`.

use super::activity::{customers_by_phone, last10};
use crate::auth::AdminUser;
use crate::error::{ApiResult, ErrorBody};
use crate::openapi::{IntoParams, ToSchema};
use crate::phone::calls::{CallDirection, CallStatus};
use crate::routes::calls::ringcentral::map_record;
use crate::routes::calls::saved;
use crate::routes::calls::telnyx::{self, TelnyxCall};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Query, State};
use chrono::{DateTime, NaiveDateTime, Utc};
use common::telephony::{cloudtalk, ringcentral};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

const PAGE_SIZE: usize = 200;
const MAX_PAGES_PER_AGENT: usize = 5;
const DAYS_BACK: i64 = 90;
const RINGCENTRAL_PAGE_SIZE: usize = 1000;
const RINGCENTRAL_MAX_PAGES: usize = 5;
/// Most Granite Phone calls listed per rep (`CloudTalk` lists 1,000 an agent).
const TELNYX_CALLS_PER_REP: i64 = 1_000;
/// Most saved calls read when a provider's live call log fails, like the
/// live reads' page limits (`RingCentral`: 5 × 1,000).
const SAVED_CALLS_LIMIT: i64 = 5_000;

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CallActivityQuery {
    /// Comma-separated user ids.
    pub user_ids: String,
}

/// One call, as a row of the users-activity feed.
#[derive(Debug, Serialize, ToSchema)]
pub struct CallActivity {
    /// The `CloudTalk` call id, or our call id for a Granite Phone call; 0
    /// for `RingCentral`, whose ids are not numbers.
    pub id: i64,
    /// The provider's call id as text.
    pub call_id: String,
    /// `cloudtalk_call`, `ringcentral_call`, or `telnyx_call` for a Granite
    /// Phone call.
    pub source: &'static str,
    /// The rep (for a Granite Phone call nobody answered: the owner of the
    /// direct line it rang).
    pub user_id: i32,
    /// The rep's name.
    pub user_name: String,
    /// `Made a call`, `Received a call` or `Missed a call`.
    pub action: &'static str,
    /// When the call started, as `CloudTalk` reports it (RFC 3339 for a
    /// `RingCentral` or Granite Phone call).
    pub created_at: String,
    /// The `CloudTalk` contact's name, for `RingCentral` the customer with
    /// that number (else the caller-ID name), or the CRM customer of a
    /// Granite Phone call.
    pub customer_name: Option<String>,
    /// The customer with that number (`RingCentral`), or the CRM customer of
    /// a Granite Phone call.
    pub customer_id: Option<i64>,
    /// The other party's number.
    pub contact: Option<String>,
    /// Direction and outcome, like `Incoming · 2m 5s`.
    pub summary: String,
    /// The call's notes, one per line (a Granite Phone call's AI note).
    pub body: Option<String>,
    /// Direction, outcome, `Recorded`, then `CloudTalk` tags (a Granite Phone
    /// call: the label of our number).
    pub tags: Vec<String>,
    /// Whether there is a recording.
    pub recorded: bool,
    /// `CloudTalk`'s recording link, a `RingCentral` media token (`rec_<id>`),
    /// or `tx_<id>` for a recorded Granite Phone call
    /// (`GET /v1/calls/telnyx/{media_id}/recording`).
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
    try_agent_calls(state, company_id, agent_id, date_from)
        .await
        .unwrap_or_default()
}

/// Every call of one agent since `date_from`; `None` when `CloudTalk` fails.
pub async fn try_agent_calls(
    state: &AppState,
    company_id: i32,
    agent_id: &str,
    date_from: &str,
) -> Option<Vec<Value>> {
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
            return None;
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
    Some(items)
}

/// Names each call after the CRM customer with the other party's number
/// (`customer_id` too), leaving the rest as they are.
pub async fn name_customers(
    pool: &sqlx::MySqlPool,
    company_id: i32,
    calls: &mut [CallActivity],
) -> sqlx::Result<()> {
    if calls.is_empty() {
        return Ok(());
    }
    let customers = customers_by_phone(pool, company_id).await?;
    for call in calls {
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
    Ok(())
}

/// The rep's extension on a `RingCentral` call-log row.
pub fn ringcentral_extension(record: &Value) -> String {
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
    if !ringcentral_connected(outbound, &text(&record["result"])) {
        record["duration"] = json!(0);
    }
    let mut item = map_record(&record, None);
    item["Contact"]["name"] = json!(text(&record[if outbound { "to" } else { "from" }]["name"]));
    item
}

/// Whether a `RingCentral` call with this `result` was talked on: connected,
/// or a voicemail the rep left.
fn ringcentral_connected(outbound: bool, result: &str) -> bool {
    let result = result.to_ascii_lowercase();
    matches!(result.as_str(), "call connected" | "accepted") || (outbound && result == "voicemail")
}

/// What the call-history job adds to a saved `RingCentral` call
/// (`provider_calls`): its `result`, the rep's extension and the other
/// party's name, so the feed reads it like a live call-log row.
pub fn ringcentral_saved_extra(record: &Value) -> Value {
    let outbound = text(&record["direction"]).eq_ignore_ascii_case("outbound");
    json!({
        "result": text(&record["result"]),
        "extension": ringcentral_extension(record),
        "other_name": text(&record[if outbound { "to" } else { "from" }]["name"]),
    })
}

/// A saved `RingCentral` call (`provider_calls`) as a row of the feed.
///
/// A call saved before the job kept its `Ringcentral` part keeps its whole duration,
/// and an outgoing one no name (its `Contact` could be the rep).
pub fn saved_ringcentral_activity(item: &Value, user_id: i32, user_name: &str) -> CallActivity {
    let mut item = item.clone();
    let extra = item["Ringcentral"].clone();
    let outbound = item["Cdr"]["type"].as_str() != Some("incoming");
    if extra.is_object() {
        if !ringcentral_connected(outbound, &text(&extra["result"])) {
            item["Cdr"]["talking_time"] = json!("0");
            item["Cdr"]["billsec"] = json!("0");
        }
        item["Contact"]["name"] = json!(text(&extra["other_name"]));
    } else if outbound {
        item["Contact"]["name"] = json!("");
    }
    let mut call = activity(&item, user_id, user_name);
    call.id = 0;
    call.source = "ringcentral_call";
    call
}

/// The `RingCentral` call log since `date_from` (RFC 3339) of the whole
/// company, or of one extension; empty when `RingCentral` fails.
pub async fn company_ringcentral_calls(
    state: &AppState,
    company_id: i32,
    extension: Option<&str>,
    date_from: &str,
) -> Vec<Value> {
    try_company_ringcentral_calls(state, company_id, extension, date_from)
        .await
        .unwrap_or_default()
}

/// The `RingCentral` call log since `date_from` (RFC 3339) of the whole
/// company, or of one extension; `None` when `RingCentral` fails (it allows
/// few call-log reads a minute).
pub async fn try_company_ringcentral_calls(
    state: &AppState,
    company_id: i32,
    extension: Option<&str>,
    date_from: &str,
) -> Option<Vec<Value>> {
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
            return None;
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
    Some(records)
}

/// A Granite Phone call as a row of the feed, for rep `user_id`.
pub fn telnyx_activity(call: &TelnyxCall, user_id: i32, user_name: &str) -> CallActivity {
    let talk_seconds = i64::from(call.duration_secs.unwrap_or(0).max(0));
    let incoming = call.direction == CallDirection::Inbound;
    let voicemail = call.status == CallStatus::Voicemail;
    let direction = if incoming { "Incoming" } else { "Outgoing" };
    let outcome = if talk_seconds > 0 {
        duration(talk_seconds)
    } else {
        match call.status {
            CallStatus::Voicemail => "Voicemail",
            CallStatus::AiMessage => "AI message",
            CallStatus::Declined => "Declined",
            CallStatus::Busy => "Busy",
            CallStatus::Failed => "Failed",
            _ if incoming => "Missed",
            _ => "No answer",
        }
        .into()
    };
    let action = if !incoming {
        "Made a call"
    } else if voicemail || talk_seconds == 0 {
        "Missed a call"
    } else {
        "Received a call"
    };
    let mut tags = vec![direction.to_string(), outcome.clone()];
    if call.recorded {
        tags.push("Recorded".into());
    }
    tags.extend(call.line_label.clone());
    CallActivity {
        id: call.id,
        call_id: call.id.to_string(),
        source: "telnyx_call",
        user_id,
        user_name: user_name.to_string(),
        action,
        created_at: telnyx::iso(call.started_at),
        customer_name: call
            .customer_name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string),
        customer_id: call.customer_id.map(i64::from),
        contact: Some(call.other_number.clone()).filter(|number| !number.is_empty()),
        summary: format!("{direction} · {outcome}"),
        body: call
            .ai_note
            .clone()
            .filter(|note| call.may_hear && !note.trim().is_empty()),
        tags,
        recorded: call.recorded && call.may_hear,
        recording_link: (call.recorded && call.may_hear).then(|| telnyx::token(call.id)),
        is_voicemail: voicemail,
        talk_seconds,
    }
}

/// When a feed row's call started: `CloudTalk`'s `YYYY-MM-DD HH:MM:SS` (UTC)
/// or RFC 3339.
fn started(row: &CallActivity) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(&row.created_at)
        .map(|at| at.with_timezone(&Utc))
        .ok()
        .or_else(|| {
            NaiveDateTime::parse_from_str(&row.created_at, "%Y-%m-%d %H:%M:%S")
                .ok()
                .map(|at| at.and_utc())
        })
}

/// List reps' calls
///
/// The `CloudTalk` and `RingCentral` calls of the given users of the caller's
/// company over the last 90 days, and their Granite Phone calls (placed or
/// answered by them, and unanswered calls of their direct lines; `source`
/// `telnyx_call`), newest first. Users without a `CloudTalk` agent or
/// `RingCentral` extension, and companies without either, have no
/// `CloudTalk` or `RingCentral` calls.
#[crate::openapi::path(
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
    let names: HashMap<i32, String> = reps
        .iter()
        .map(|(id, name, _, _)| (*id, name.clone().unwrap_or_default()))
        .collect();
    let rep_ids: Vec<i32> = names.keys().copied().collect();
    let phone_calls = if rep_ids.is_empty() {
        Vec::new()
    } else {
        telnyx::list(
            &state.pool,
            user.company_id,
            crate::phone::calls::Hearer::of(&user),
            &telnyx::Filter {
                users: &rep_ids,
                from: Some(since),
                limit: TELNYX_CALLS_PER_REP * i64::try_from(rep_ids.len()).unwrap_or(1),
                ..telnyx::Filter::default()
            },
        )
        .await?
    };
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
            // CloudTalk down or disconnected: the calls the call-history job saved.
            let items = match try_agent_calls(&state, company_id, &agent, &date_from).await {
                Some(items) => items,
                None => saved::list_by_agents(
                    &state.pool,
                    company_id,
                    "cloudtalk",
                    std::slice::from_ref(&agent),
                    since,
                    SAVED_CALLS_LIMIT,
                )
                .await
                .unwrap_or_default(),
            };
            items
                .iter()
                .map(|item| activity(item, id, &name))
                .collect::<Vec<_>>()
        }));
    }
    let mut calls: Vec<CallActivity> = phone_calls
        .iter()
        .filter_map(|call| {
            let owner = call.owner_id?;
            Some(telnyx_activity(call, owner, names.get(&owner)?))
        })
        .collect();
    if !extensions.is_empty() {
        let date_from = since.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let live = try_company_ringcentral_calls(&state, user.company_id, None, &date_from).await;
        let mut ringcentral_calls: Vec<CallActivity> = if let Some(records) = live {
            records
                .iter()
                .filter_map(|record| {
                    let (id, name) = extensions.get(&ringcentral_extension(record))?;
                    Some(ringcentral_activity(record, *id, name))
                })
                .collect()
        } else {
            // RingCentral allows few call-log reads a minute (429): the calls
            // the call-history job saved, a few minutes behind.
            let agents: Vec<String> = extensions.keys().cloned().collect();
            saved::list_by_agents(
                &state.pool,
                user.company_id,
                "ringcentral",
                &agents,
                since,
                SAVED_CALLS_LIMIT,
            )
            .await?
            .iter()
            .filter_map(|item| {
                let extension = [&item["Ringcentral"]["extension"], &item["Cdr"]["user_id"]]
                    .into_iter()
                    .map(text)
                    .find(|id| !id.is_empty())?;
                let (id, name) = extensions.get(&extension)?;
                Some(saved_ringcentral_activity(item, *id, name))
            })
            .collect()
        };
        name_customers(&state.pool, user.company_id, &mut ringcentral_calls).await?;
        calls.extend(ringcentral_calls);
    }
    for task in tasks {
        calls.extend(task.await.unwrap_or_default());
    }
    calls.sort_by(|a, b| {
        started(b)
            .cmp(&started(a))
            .then_with(|| b.created_at.cmp(&a.created_at))
    });
    Ok(Json(calls))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_saved_ringcentral_calls() {
        // As the call-history job saves it: ringing is not talk time, the
        // other party is the contact, the rep's extension is kept.
        let missed = json!({
            "id": "a1", "direction": "Inbound", "result": "Missed", "duration": 20,
            "startTime": "2026-05-02T10:00:00.000Z",
            "from": { "phoneNumber": "+13175550100", "name": "ALICE LEAD" },
            "to": { "extensionId": "601", "name": "Rita Rep" },
        });
        let mut item = map_record(&missed, None);
        item["Ringcentral"] = ringcentral_saved_extra(&missed);
        assert_eq!(item["Ringcentral"]["extension"], "601");
        let call = saved_ringcentral_activity(&item, 100, "Rita Rep");
        assert_eq!(call.source, "ringcentral_call");
        assert_eq!(call.action, "Missed a call");
        assert_eq!(call.talk_seconds, 0);
        assert_eq!(call.customer_name.as_deref(), Some("ALICE LEAD"));

        let outgoing = json!({
            "id": "a2", "direction": "Outbound", "result": "Call connected", "duration": 76,
            "startTime": "2026-05-03T10:00:00.000Z", "extension": { "id": 601 },
            "from": { "extensionId": "601", "name": "Rita Rep" },
            "to": { "phoneNumber": "+13175550100", "name": "Alice" },
        });
        let mut item = map_record(&outgoing, None);
        item["Ringcentral"] = ringcentral_saved_extra(&outgoing);
        let call = saved_ringcentral_activity(&item, 100, "Rita Rep");
        assert_eq!(call.action, "Made a call");
        assert_eq!(call.summary, "Outgoing · 1m 16s");
        assert_eq!(call.customer_name.as_deref(), Some("Alice"));

        // Saved before the job kept its `Ringcentral` part: never the rep's name.
        let old = map_record(&outgoing, None);
        let call = saved_ringcentral_activity(&old, 100, "Rita Rep");
        assert_eq!(call.customer_name, None);
        assert_eq!(call.talk_seconds, 76);
    }

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
