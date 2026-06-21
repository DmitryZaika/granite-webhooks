use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Deserialize, Debug)]
pub(crate) struct EventBridgeEvent {
    pub account: String,
    pub detail: Value,
    #[serde(rename = "detail-type")]
    pub detail_type: String,
    pub id: String,
    pub region: String,
    pub resources: Vec<String>,
    pub source: String,
    pub time: String,
    pub version: String,
}

#[derive(Serialize)]
pub(crate) struct OutgoingMessage {
    req_id: String,
    pub msg: String,
}

impl OutgoingMessage {
    pub fn new(req_id: String, msg: String) -> Self {
        Self { req_id, msg }
    }
}
