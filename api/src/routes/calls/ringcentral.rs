//! `RingCentral` call history in the `CloudTalk` shape the call panels read.
//!
//! Port of `fetchCallsForPhones`, `mapCallLogRecord` and the voicemail
//! matching of `app/utils/ringcentral.server.ts` and
//! `ringcentralVoicemail.server.ts`. Media tokens (`rec_…`, `vm_…`) are the
//! ones the Remix `userCallMedia` route plays.

use chrono::DateTime;
use common::telephony::ringcentral::{ApiError, Body, request};
use reqwest::{Client, Method};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::Write;

/// Stands in for `~` (the token owner's extension) inside a media token.
const OWNER_EXTENSION: &str = "me";
/// Clock skew allowance for a message written just before the call-log row.
const MATCH_BEFORE_MS: i64 = 60_000;
/// `RingCentral` writes the message after the caller hangs up, sometimes late.
const MATCH_AFTER_MS: i64 = 600_000;

fn digits(phone: &str) -> String {
    phone.chars().filter(char::is_ascii_digit).collect()
}

fn last10(phone: &str) -> String {
    let all = digits(phone);
    all[all.len().saturating_sub(10)..].to_string()
}

fn text<'a>(value: &'a Value, path: &[&str]) -> &'a str {
    path.iter()
        .fold(value, |node, key| &node[*key])
        .as_str()
        .unwrap_or("")
}

/// `RingCentral` ids come as numbers or strings.
fn id_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        _ => String::new(),
    }
}

fn call_type(record: &Value) -> &'static str {
    match text(record, &["direction"]).to_ascii_lowercase().as_str() {
        "inbound" => "incoming",
        "outbound" => "outgoing",
        _ => "internal",
    }
}

fn external_phone(record: &Value) -> &str {
    if call_type(record) == "incoming" {
        text(record, &["from", "phoneNumber"])
    } else {
        text(record, &["to", "phoneNumber"])
    }
}

fn is_voicemail(record: &Value) -> bool {
    text(record, &["result"])
        .to_ascii_lowercase()
        .contains("voicemail")
}

fn empty_agent() -> Value {
    json!({
        "id": 0, "firstname": "", "lastname": "", "fullname": "", "email": "", "language": "",
        "role": "", "status": "", "default_outbound_number": "", "associated_numbers": [], "groups": [],
    })
}

/// One call-log row as a `CloudTalk` call.
pub fn map_record(record: &Value, voicemail_token: Option<&str>) -> Value {
    let kind = call_type(record);
    let external = external_phone(record);
    let internal = if kind == "incoming" {
        text(record, &["to", "phoneNumber"])
    } else {
        text(record, &["from", "phoneNumber"])
    };
    let duration = record["duration"].as_i64().unwrap_or(0).to_string();
    let started = text(record, &["startTime"]);
    let recording = id_text(&record["recording"]["id"]);
    let media = if recording.is_empty() {
        voicemail_token.unwrap_or("").to_string()
    } else {
        format!("rec_{recording}")
    };
    let extension = match id_text(&record["extension"]["id"]) {
        id if !id.is_empty() => id,
        _ => text(record, &["from", "extensionId"]).to_string(),
    };
    let name = [
        text(record, &["from", "name"]),
        text(record, &["to", "name"]),
    ]
    .into_iter()
    .find(|name| !name.is_empty())
    .unwrap_or("");
    let mut agent = empty_agent();
    agent["id"] = json!(
        id_text(&record["extension"]["id"])
            .parse::<i64>()
            .unwrap_or(0)
    );
    json!({
        "Cdr": {
            "id": id_text(&record["id"]), "billsec": duration, "type": kind,
            "public_external": external, "public_internal": internal,
            "recorded": !media.is_empty(), "is_voicemail": is_voicemail(record),
            "fax_email": "", "is_redirected": "0", "redirected_from": "", "transferred_from": "",
            "is_local": false, "user_id": extension, "talking_time": duration,
            "started_at": started, "answered_at": started, "ended_at": "",
            "waiting_time": "0", "wrapup_time": "0", "recording_link": media,
        },
        "Contact": {
            "id": 0, "name": name, "title": "", "company": "", "industry": "", "address": "",
            "city": "", "zip": "", "state": "", "type": "",
            "contact_numbers": if external.is_empty() { json!([]) } else { json!([external]) },
            "contact_emails": [], "tags": [], "external_urls": [], "custom_fields": [],
            "favorite_agent": empty_agent(),
        },
        "CallNumber": {
            "id": 0, "internal_name": "", "caller_id_e164": internal, "country_code": 0, "area_code": 0,
        },
        "BillingCall": { "price": "0" },
        "Agent": agent,
        "Notes": [], "Tags": [], "Ratings": [],
    })
}

/// An inbound voicemail call to pair with a message.
#[derive(Debug, Clone)]
pub struct VoicemailCall {
    pub call_id: String,
    pub from_phone: String,
    pub start_time: String,
    pub duration_seconds: i64,
    pub extension_id: Option<String>,
    pub callee_phone: String,
}

/// A voicemail message in an extension's message store.
#[derive(Debug, Clone)]
pub struct Voicemail {
    pub message_id: String,
    pub extension_id: String,
    pub from_phone: String,
    pub creation_time: String,
    pub audio_attachment_id: Option<String>,
}

fn voicemail_calls(records: &[Value]) -> Vec<VoicemailCall> {
    records
        .iter()
        .filter(|record| is_voicemail(record) && call_type(record) == "incoming")
        .filter_map(|record| {
            let call_id = id_text(&record["id"]);
            (!call_id.is_empty()).then(|| VoicemailCall {
                call_id,
                from_phone: text(record, &["from", "phoneNumber"]).to_string(),
                start_time: text(record, &["startTime"]).to_string(),
                duration_seconds: record["duration"].as_i64().unwrap_or(0),
                extension_id: [
                    id_text(&record["to"]["extensionId"]),
                    id_text(&record["extension"]["id"]),
                ]
                .into_iter()
                .find(|id| !id.is_empty()),
                callee_phone: text(record, &["to", "phoneNumber"]).to_string(),
            })
        })
        .collect()
}

fn epoch_ms(time: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(time)
        .ok()
        .map(|at| at.timestamp_millis())
}

/// Pairs calls with messages by caller and time, each message once; returns
/// the media token per call id.
pub fn match_voicemails(
    calls: &[VoicemailCall],
    voicemails: &[Voicemail],
) -> HashMap<String, String> {
    let mut ordered: Vec<&VoicemailCall> = calls.iter().collect();
    ordered.sort_by_key(|call| epoch_ms(&call.start_time).unwrap_or(i64::MAX));
    let mut used = HashSet::new();
    let mut tokens = HashMap::new();
    for call in ordered {
        let Some(started) = epoch_ms(&call.start_time) else {
            continue;
        };
        let ended = started + call.duration_seconds * 1000;
        let caller = last10(&call.from_phone);
        let best = voicemails
            .iter()
            .filter(|voicemail| voicemail.audio_attachment_id.is_some())
            .filter(|voicemail| !used.contains(&voicemail.message_id))
            .filter(|voicemail| caller.is_empty() || last10(&voicemail.from_phone) == caller)
            .filter_map(|voicemail| {
                let created = epoch_ms(&voicemail.creation_time)?;
                (created >= started - MATCH_BEFORE_MS && created <= ended + MATCH_AFTER_MS)
                    .then_some((voicemail, (created - ended).abs()))
            })
            .min_by_key(|(_, distance)| *distance);
        if let Some((voicemail, _)) = best {
            used.insert(voicemail.message_id.clone());
            tokens.insert(
                call.call_id.clone(),
                format!(
                    "vm_{}_{}_{}",
                    voicemail.extension_id,
                    voicemail.message_id,
                    voicemail.audio_attachment_id.as_deref().unwrap_or("")
                ),
            );
        }
    }
    tokens
}

pub fn attachment_of(record: &Value, kind: &str) -> Option<String> {
    record["attachments"]
        .as_array()?
        .iter()
        .find(|attachment| attachment["type"] == kind)
        .map(|attachment| id_text(&attachment["id"]))
        .filter(|id| !id.is_empty())
}

/// What the call log needs from the caller.
pub struct Fetch<'a> {
    pub pool: &'a MySqlPool,
    pub client: &'a Client,
    pub company_id: i32,
}

impl Fetch<'_> {
    async fn get(&self, path: &str) -> Result<Value, ApiError> {
        request(
            self.pool,
            self.client,
            self.company_id,
            Method::GET,
            path,
            &Body::Empty,
            None,
        )
        .await
    }

    /// The extension of the user linked to `phone`, for a voicemail row
    /// `RingCentral` left without one.
    async fn linked_extension(&self, phone: &str) -> Option<String> {
        let needle = last10(phone);
        if needle.len() < 10 {
            return None;
        }
        let rows: Vec<(Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT ringcentral_extension_id, ringcentral_phone_number FROM users
              WHERE company_id = ? AND is_deleted = 0 AND TRIM(COALESCE(ringcentral_extension_id, '')) <> ''",
        )
        .bind(self.company_id)
        .fetch_all(self.pool)
        .await
        .ok()?;
        rows.into_iter()
            .find(|(_, number)| last10(number.as_deref().unwrap_or("")) == needle)
            .and_then(|(extension, _)| extension.map(|id| id.trim().to_string()))
    }

    async fn voicemails(&self, extension_id: &str, date_from: Option<&str>) -> Vec<Voicemail> {
        let segment = if extension_id == OWNER_EXTENSION {
            "~"
        } else {
            extension_id
        };
        let mut path = format!(
            "account/~/extension/{segment}/message-store?messageType=VoiceMail&perPage=250"
        );
        if let Some(from) = date_from {
            path.push_str("&dateFrom=");
            path.push_str(&urlencode(from));
        }
        // An extension we cannot read voicemail for just stays unplayable.
        let Ok(body) = self.get(&path).await else {
            return Vec::new();
        };
        body["records"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|record| {
                let message_id = id_text(&record["id"]);
                (!message_id.is_empty()).then(|| Voicemail {
                    message_id,
                    extension_id: extension_id.to_string(),
                    from_phone: text(record, &["from", "phoneNumber"]).to_string(),
                    creation_time: text(record, &["creationTime"]).to_string(),
                    audio_attachment_id: attachment_of(record, "AudioRecording"),
                })
            })
            .collect()
    }

    /// Calls with any of `phones` (E.164), newest first as `RingCentral`
    /// returns them, with voicemail made playable.
    pub async fn calls_for_phones(
        &self,
        phones: &[String],
        date_from: Option<&str>,
        date_to: Option<&str>,
    ) -> Result<Vec<Value>, ApiError> {
        if phones.is_empty() {
            return Ok(Vec::new());
        }
        let mut path = "account/~/call-log?perPage=250&view=Detailed".to_string();
        for (key, value) in [("dateFrom", date_from), ("dateTo", date_to)] {
            if let Some(value) = value {
                let _ = write!(path, "&{key}={}", urlencode(value));
            }
        }
        let body = self.get(&path).await?;
        let wanted: HashSet<String> = phones
            .iter()
            .map(|phone| digits(phone))
            .filter(|d| !d.is_empty())
            .collect();
        let mut seen = HashSet::new();
        let records: Vec<Value> = body["records"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|record| {
                let id = id_text(&record["id"]);
                !id.is_empty()
                    && (wanted.is_empty() || wanted.contains(&digits(external_phone(record))))
                    && seen.insert(id)
            })
            .cloned()
            .collect();

        let mut calls = voicemail_calls(&records);
        let mut by_phone: HashMap<String, Option<String>> = HashMap::new();
        for call in &mut calls {
            if call.extension_id.is_some() {
                continue;
            }
            if !by_phone.contains_key(&call.callee_phone) {
                let linked = self.linked_extension(&call.callee_phone).await;
                by_phone.insert(call.callee_phone.clone(), linked);
            }
            call.extension_id = Some(
                by_phone
                    .get(&call.callee_phone)
                    .cloned()
                    .flatten()
                    .unwrap_or_else(|| OWNER_EXTENSION.to_string()),
            );
        }
        let message_from = date_from.map(|from| {
            if from.len() == 10 {
                format!("{from}T00:00:00.000Z")
            } else {
                from.to_string()
            }
        });
        let mut voicemails = Vec::new();
        let extensions: BTreeSet<String> = calls
            .iter()
            .filter_map(|call| call.extension_id.clone())
            .collect();
        for extension in extensions {
            voicemails.extend(self.voicemails(&extension, message_from.as_deref()).await);
        }
        let tokens = match_voicemails(&calls, &voicemails);
        Ok(records
            .iter()
            .map(|record| {
                map_record(
                    record,
                    tokens.get(&id_text(&record["id"])).map(String::as_str),
                )
            })
            .collect())
    }
}

fn urlencode(value: &str) -> String {
    percent_encoding::utf8_percent_encode(value, percent_encoding::NON_ALPHANUMERIC).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_call_log_row_reads_like_a_cloudtalk_call() {
        let record = json!({
            "id": "c1", "direction": "Inbound", "duration": 42, "startTime": "2026-10-01T15:00:00Z",
            "result": "Accepted", "from": { "phoneNumber": "+13175550100", "name": "Alice" },
            "to": { "phoneNumber": "+13175550999" }, "recording": { "id": 77 }, "extension": { "id": "501" },
        });
        let call = map_record(&record, None);
        assert_eq!(call["Cdr"]["type"], "incoming");
        assert_eq!(call["Cdr"]["public_external"], "+13175550100");
        assert_eq!(call["Cdr"]["public_internal"], "+13175550999");
        assert_eq!(call["Cdr"]["billsec"], "42");
        assert_eq!(call["Cdr"]["recording_link"], "rec_77");
        assert_eq!(call["Cdr"]["recorded"], true);
        assert_eq!(call["Cdr"]["user_id"], "501");
        assert_eq!(call["Agent"]["id"], 501);
        assert_eq!(call["Contact"]["name"], "Alice");
        assert_eq!(call["Contact"]["contact_numbers"], json!(["+13175550100"]));
    }

    #[test]
    fn voicemail_pairs_with_the_closest_message_once() {
        let call = |id: &str, start: &str| VoicemailCall {
            call_id: id.into(),
            from_phone: "+13175550100".into(),
            start_time: start.into(),
            duration_seconds: 30,
            extension_id: Some("501".into()),
            callee_phone: String::new(),
        };
        let voicemail = |id: &str, at: &str| Voicemail {
            message_id: id.into(),
            extension_id: "501".into(),
            from_phone: "(317) 555-0100".into(),
            creation_time: at.into(),
            audio_attachment_id: Some(format!("a{id}")),
        };
        let tokens = match_voicemails(
            &[
                call("c1", "2026-10-01T15:00:00Z"),
                call("c2", "2026-10-01T15:05:00Z"),
            ],
            &[
                voicemail("m1", "2026-10-01T15:00:40Z"),
                voicemail("m2", "2026-10-01T15:05:45Z"),
                voicemail("m3", "2026-10-01T18:00:00Z"),
            ],
        );
        assert_eq!(tokens.get("c1").map(String::as_str), Some("vm_501_m1_am1"));
        assert_eq!(tokens.get("c2").map(String::as_str), Some("vm_501_m2_am2"));
        assert_eq!(tokens.len(), 2);
    }
}
