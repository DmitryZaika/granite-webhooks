//! The message timeline the engine reads, shared by its parts.

use super::classify::{InboundMessageType, ReplyClassificationConfidence};
use crate::str_enum;
use serde::{Deserialize, Serialize};

str_enum! { pub enum Channel { Sms = "sms", Email = "email" } }

str_enum! { pub enum Direction { Inbound = "inbound", Outbound = "outbound" } }

str_enum! { pub enum NormalizedSenderRole {
    Customer = "CUSTOMER", SalesRep = "SALES_REP", System = "SYSTEM", ThirdParty = "THIRD_PARTY", Unknown = "UNKNOWN",
} }

/// One message of a customer's conversation (SMS or email, either way).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimelineMessage {
    pub at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<Channel>,
    pub direction: Direction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_phone: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inbound_type: Option<InboundMessageType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_mirrored_duplicate: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub normalized_sender_role: Option<NormalizedSenderRole>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_confidence: Option<ReplyClassificationConfidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_customer_identity: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_role_conflict: Option<bool>,
}

impl TimelineMessage {
    pub fn new(at: impl Into<String>, direction: Direction, text: impl Into<String>) -> Self {
        Self {
            at: at.into(),
            channel: None,
            direction,
            text: Some(text.into()),
            from_email: None,
            from_phone: None,
            inbound_type: None,
            is_mirrored_duplicate: None,
            normalized_sender_role: None,
            role_confidence: None,
            matched_customer_identity: None,
            sender_role_conflict: None,
        }
    }

    pub fn text(&self) -> &str {
        self.text.as_deref().unwrap_or("")
    }

    pub fn inbound(&self) -> bool {
        self.direction == Direction::Inbound
    }
}

/// A message time in ms: zone-less times are UTC (`YYYY-MM-DD HH:MM` gets
/// `:00.000Z`, longer ones `Z`).
pub fn parse_event_at(at: &str) -> Option<i64> {
    super::intent::message_time_ms(Some(at))
}

/// A message time in ms, zone-less times read as UTC by appending `Z`.
pub fn time_with_z(at: &str) -> Option<i64> {
    let trimmed = super::js::trim(at);
    let with_t = if trimmed.contains('T') {
        trimmed.to_string()
    } else {
        trimmed.replacen(' ', "T", 1)
    };
    let has_zone = crate::re!(r"Z$", "u").is_match(&with_t)
        || crate::re!(r"[+-]\d{2}:\d{2}$", "u").is_match(&with_t);
    super::dates::js_date_parse(&if has_zone {
        with_t
    } else {
        format!("{with_t}Z")
    })
}
