//! `GET /v1/users/call-activity`: the `CloudTalk` and Granite Phone calls of
//! chosen reps over the last 90 days, for the admin users-activity page.
//! Remix: `api.users.activity.calls.tsx`.

use crate::auth::AdminUser;
use crate::error::{ApiResult, ErrorBody};
use crate::phone::calls::{CallDirection, CallStatus};
use crate::routes::calls::telnyx::{self, TelnyxCall};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Query, State};
use chrono::{DateTime, NaiveDateTime, Utc};
use common::telephony::cloudtalk;
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use utoipa::{IntoParams, ToSchema};

const PAGE_SIZE: usize = 200;
const MAX_PAGES_PER_AGENT: usize = 5;
const DAYS_BACK: i64 = 90;
/// Most Granite Phone calls listed per rep (`CloudTalk` lists 1,000 an agent).
const TELNYX_CALLS_PER_REP: i64 = 1_000;

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CallActivityQuery {
    /// Comma-separated user ids.
    pub user_ids: String,
}

/// One call, as a row of the users-activity feed.
#[derive(Debug, Serialize, ToSchema)]
pub struct CallActivity {
    /// The `CloudTalk` call id, or our call id for a Granite Phone call.
    pub id: i64,
    /// `cloudtalk_call`, or `telnyx_call` for a Granite Phone call.
    pub source: &'static str,
    /// The rep (for a Granite Phone call nobody answered: the owner of the
    /// direct line it rang).
    pub user_id: i32,
    /// The rep's name.
    pub user_name: String,
    /// `Made a call`, `Received a call` or `Missed a call`.
    pub action: &'static str,
    /// When the call started, as `CloudTalk` reports it (RFC 3339 UTC for a
    /// Granite Phone call).
    pub created_at: String,
    /// The `CloudTalk` contact's name, or the CRM customer of a Granite
    /// Phone call.
    pub customer_name: Option<String>,
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
    /// `CloudTalk`'s recording link, or `tx_<id>` for a recorded Granite
    /// Phone call (`GET /v1/calls/telnyx/{media_id}/recording`).
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
        source: "cloudtalk_call",
        user_id,
        user_name: user_name.to_string(),
        action,
        created_at: text(&cdr["started_at"]),
        customer_name: non_empty(&item["Contact"]["name"]),
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
/// The `CloudTalk` calls of the given users of the caller's company over the
/// last 90 days, and their Granite Phone calls (placed or answered by them,
/// and unanswered calls of their direct lines; `source` `telnyx_call`),
/// newest first. Users without a `CloudTalk` agent, and companies without
/// `CloudTalk`, have no `CloudTalk` calls.
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
        "SELECT id, name, cloudtalk_agent_id FROM users WHERE is_deleted = 0 AND company_id = ",
    );
    builder.push_bind(user.company_id).push(" AND id IN (");
    let mut list = builder.separated(", ");
    for id in &ids {
        list.push_bind(id);
    }
    builder.push(")");
    let reps: Vec<(i32, Option<String>, Option<String>)> =
        builder.build_query_as().fetch_all(&state.pool).await?;
    let since = chrono::Utc::now() - chrono::Duration::days(DAYS_BACK);
    let date_from = since.format("%Y-%m-%d").to_string();
    let names: HashMap<i32, String> = reps
        .iter()
        .map(|(id, name, _)| (*id, name.clone().unwrap_or_default()))
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
    let mut tasks = Vec::new();
    for (id, name, agent) in reps {
        let Some(agent) = agent
            .map(|agent| agent.trim().to_string())
            .filter(|agent| !agent.is_empty())
        else {
            continue;
        };
        let (state, date_from) = (state.clone(), date_from.clone());
        let company_id = user.company_id;
        tasks.push(tokio::spawn(async move {
            let name = name.unwrap_or_default();
            agent_calls(&state, company_id, &agent, &date_from)
                .await
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
