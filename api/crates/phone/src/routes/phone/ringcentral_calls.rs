//! `GET /v1/phone/calls/ringcentral`: the caller's `RingCentral` calls for
//! the app's Calls tab, beside its Granite Phone calls.
//!
//! Like texts, a call is the rep's by its user or its number, either one:
//! their `RingCentral` extension now or ever, or a number of theirs now or
//! ever (`user_sms_identities`). Read live from `RingCentral`'s company call
//! log, at most once a minute per company on this instance.

use crate::auth::device::DeviceUser;
use crate::error::{ApiResult, ErrorBody};
use crate::openapi::ToSchema;
use crate::phone::calls::{CallCustomer, CallDirection, CallStatus};
use crate::routes::sms::provider::SmsProvider;
use crate::routes::sms::scope::own_ids;
use crate::routes::users::activity::{customers_by_phone, last10};
use crate::routes::users::call_activity::{company_ringcentral_calls, ringcentral_extension};
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

/// Days of calls listed.
const DAYS: i64 = 30;
/// How long one company's call log is reused.
const TTL: Duration = Duration::from_secs(60);

type Cache = HashMap<i32, (Instant, Arc<Vec<Value>>)>;
static CALL_LOGS: LazyLock<Mutex<Cache>> = LazyLock::new(Mutex::default);

/// One `RingCentral` call.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RingCentralCall {
    /// `RingCentral`'s call-log id.
    pub id: String,
    pub direction: CallDirection,
    /// `answered`, `missed`, `voicemail`, `busy` or `no_answer`.
    pub status: CallStatus,
    /// RFC 3339.
    pub started_at: String,
    /// Talk time in seconds; 0 when nobody talked.
    pub duration_secs: i64,
    /// The other party's number, as `RingCentral` gives it (E.164).
    pub other_number: String,
    /// Our customer with that number; null when none.
    pub customer: Option<CallCustomer>,
}

/// The caller's `RingCentral` calls.
#[derive(Debug, Serialize, ToSchema)]
pub struct RingCentralCalls {
    /// Newest first, the last 30 days.
    pub calls: Vec<RingCentralCall>,
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or("")
}

/// The company's call log of the last [`DAYS`], cached [`TTL`].
async fn call_log(state: &AppState, company_id: i32) -> Arc<Vec<Value>> {
    let cached = CALL_LOGS.lock().ok().and_then(|cache| {
        cache
            .get(&company_id)
            .filter(|(read_at, _)| read_at.elapsed() < TTL)
            .map(|(_, records)| Arc::clone(records))
    });
    if let Some(records) = cached {
        return records;
    }
    let from = (chrono::Utc::now() - chrono::Duration::days(DAYS))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let records = Arc::new(company_ringcentral_calls(state, company_id, None, &from).await);
    if let Ok(mut cache) = CALL_LOGS.lock() {
        cache.retain(|_, (read_at, _)| read_at.elapsed() < TTL);
        cache.insert(company_id, (Instant::now(), Arc::clone(&records)));
    }
    records
}

/// Direction, status and talk time of one call-log row.
fn outcome(record: &Value) -> (CallDirection, CallStatus, i64) {
    let outbound = text(record, "direction").eq_ignore_ascii_case("outbound");
    let result = text(record, "result").to_ascii_lowercase();
    let connected = matches!(result.as_str(), "call connected" | "accepted")
        || (outbound && result == "voicemail");
    let talk = if connected {
        record["duration"].as_i64().unwrap_or(0)
    } else {
        0
    };
    let status = match (outbound, connected, result.as_str()) {
        (_, true, _) => CallStatus::Answered,
        (false, false, "voicemail") => CallStatus::Voicemail,
        (false, false, _) => CallStatus::Missed,
        (true, false, "busy") => CallStatus::Busy,
        (true, false, _) => CallStatus::NoAnswer,
    };
    let direction = if outbound {
        CallDirection::Outbound
    } else {
        CallDirection::Inbound
    };
    (direction, status, talk)
}

/// List my `RingCentral` calls
///
/// The caller's `RingCentral` calls of the last 30 days, newest first: those
/// of an extension of theirs, now or ever, and those to or from a number of
/// theirs, now or ever (the rule texts follow). Each is named after our
/// customer with that number. Empty when the company has no `RingCentral`
/// or it can't be read. Device token only.
#[crate::openapi::path(
    get,
    path = "/v1/phone/calls/ringcentral",
    operation_id = "list_ringcentral_calls",
    tag = "phone",
    responses(
        (status = 200, description = "The caller's RingCentral calls", body = RingCentralCalls),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn list_ringcentral_calls(
    State(state): State<AppState>,
    device: DeviceUser,
) -> ApiResult<Json<RingCentralCalls>> {
    let user = &device.user;
    let (extensions, lines) = own_ids(&state.pool, SmsProvider::Ringcentral, user).await?;
    if extensions.is_empty() && lines.is_empty() {
        return Ok(Json(RingCentralCalls { calls: Vec::new() }));
    }
    let records = call_log(&state, user.company_id).await;
    let mine: Vec<&Value> = records
        .iter()
        .filter(|record| {
            let outbound = text(record, "direction").eq_ignore_ascii_case("outbound");
            let ours = &record[if outbound { "from" } else { "to" }];
            extensions.contains(&ringcentral_extension(record))
                || last10(text(ours, "phoneNumber")).is_some_and(|line| lines.contains(&line))
        })
        .collect();
    if mine.is_empty() {
        return Ok(Json(RingCentralCalls { calls: Vec::new() }));
    }
    let customers = customers_by_phone(&state.pool, user.company_id).await?;
    let mut calls: Vec<RingCentralCall> = mine
        .into_iter()
        .map(|record| {
            let (direction, status, duration_secs) = outcome(record);
            let other = &record[if direction == CallDirection::Outbound {
                "to"
            } else {
                "from"
            }];
            let other_number = text(other, "phoneNumber").to_string();
            let customer = last10(&other_number)
                .and_then(|key| customers.get(&key))
                .and_then(|customer| {
                    Some(CallCustomer {
                        id: i32::try_from(customer.id).ok()?,
                        name: customer.name.clone().unwrap_or_default(),
                        company: None,
                    })
                });
            RingCentralCall {
                id: text(record, "id").to_string(),
                direction,
                status,
                started_at: text(record, "startTime").to_string(),
                duration_secs,
                other_number,
                customer,
            }
        })
        .collect();
    calls.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    Ok(Json(RingCentralCalls { calls }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn maps_outcomes() {
        let call = |direction: &str, result: &str| {
            outcome(&json!({ "direction": direction, "result": result, "duration": 42 }))
        };
        assert_eq!(
            call("Inbound", "Accepted"),
            (CallDirection::Inbound, CallStatus::Answered, 42)
        );
        assert_eq!(
            call("Inbound", "Missed"),
            (CallDirection::Inbound, CallStatus::Missed, 0)
        );
        assert_eq!(
            call("Inbound", "Voicemail"),
            (CallDirection::Inbound, CallStatus::Voicemail, 0)
        );
        assert_eq!(
            call("Outbound", "Voicemail"),
            (CallDirection::Outbound, CallStatus::Answered, 42)
        );
        assert_eq!(
            call("Outbound", "Busy"),
            (CallDirection::Outbound, CallStatus::Busy, 0)
        );
        assert_eq!(
            call("Outbound", "No Answer"),
            (CallDirection::Outbound, CallStatus::NoAnswer, 0)
        );
    }
}
