//! Who sent a message, and what kind of message it is.
//!
//! An inbound message is a customer reply, an auto-reply or a system notice;
//! an outbound one is a sales follow-up, a promotion or a transactional note.
//! Ports of `smsFlowAiInboundClassification.ts` and
//! `smsFlowAiOutboundClassification.ts`.

use super::js::trim;
use crate::{re, str_enum};
use regex::Regex;

/// Stored in `sms_flow_steps.custom_text` so an AI step fits the template
/// XOR custom-text constraint (`smsFlowAiStep.ts`).
pub const SMS_FLOW_AI_SENTINEL: &str = "__GR_AI_FOLLOWUP__";

pub fn is_ai_followup_step(custom_text: Option<&str>) -> bool {
    custom_text == Some(SMS_FLOW_AI_SENTINEL)
}

str_enum! { pub enum SalespersonOutboundType {
    ConversationalReply = "CONVERSATIONAL_REPLY", SalesFollowup = "SALES_FOLLOWUP", Promotion = "PROMOTION",
    Transactional = "TRANSACTIONAL", System = "SYSTEM",
} }

pub fn classify_salesperson_outbound(text: &str) -> SalespersonOutboundType {
    use SalespersonOutboundType as T;
    let trimmed = trim(text);
    if trimmed.is_empty() {
        return T::SalesFollowup;
    }
    if re!(
        r"adobe\s*sign|docusign|no-?reply|this is an automated|has been sent out for signature",
        "i"
    )
    .is_match(trimmed)
    {
        return T::System;
    }
    if re!(
        r"\b\d+%\s*off\b|\bsale\b|\bpromo|\bsave \d+%|summer sale|fourth of july|4th of july|wishlist|stop by our showroom",
        "i"
    )
    .is_match(trimmed)
    {
        return T::Promotion;
    }
    if re!(
        r"\binvoice\b|\breceipt\b|\bappointment (is )?confirm|\bpayment (received|link)\b|\bscheduled for\b",
        "i"
    )
    .is_match(trimmed)
    {
        return T::Transactional;
    }
    if re!(
        r"^(yes|yep|yeah|ok|okay|got it|thank you|thanks|sounds good|understood|will do|no problem)\b",
        "i"
    )
    .is_match(trimmed)
        || re!(r"thank you for the update|hope your .{0,40}turns out", "i").is_match(trimmed)
        || re!(r"^this is \w+", "i").is_match(trimmed)
        || re!(r"\bthis is .{0,60}granite depot", "i").is_match(trimmed)
    {
        return T::ConversationalReply;
    }
    T::SalesFollowup
}

pub fn is_conversational_salesperson_reply(text: &str) -> bool {
    classify_salesperson_outbound(text) == SalespersonOutboundType::ConversationalReply
}

str_enum! { pub enum InboundMessageType {
    CustomerReply = "CUSTOMER_REPLY", SystemMessage = "SYSTEM_MESSAGE", AutomatedMessage = "AUTOMATED_MESSAGE",
    ThirdPartyMessage = "THIRD_PARTY_MESSAGE", InternalMessage = "INTERNAL_MESSAGE",
    MisThreadedMessage = "MIS_THREADED_MESSAGE", UnknownInbound = "UNKNOWN_INBOUND",
} }

str_enum! { pub enum ReplyClassificationConfidence { High = "high", Medium = "medium", Low = "low" } }

/// Who sent an inbound message, and how sure we are.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InboundClassification {
    #[serde(rename = "type")]
    pub kind: InboundMessageType,
    pub matched_customer_identity: bool,
    pub sender_label: String,
    pub confidence: ReplyClassificationConfidence,
    pub reason: &'static str,
}

fn system_sender_re() -> &'static Regex {
    re!(
        r"adobe\s*sign|adobesign@|docusign|no-?reply|noreply|notifications?@|mailer-daemon|delivery-status|postmaster@|calendar-notification|donotreply",
        "i"
    )
}

fn system_content_re() -> &'static Regex {
    re!(
        r"adobe acrobat sign|adobesign@adobesign|has been sent out for signature|signed and filed|docusign|mailer-daemon|delivery status notification|this is an automated|auto-submitted:|precedence:\s*(bulk|junk|auto)|x-auto-response-suppress|list-id:",
        "i"
    )
}

fn other_customer_contract_re() -> &'static Regex {
    re!(
        r"between granite depot.*and |has been sent out for signature to ",
        "i"
    )
}

fn normalize_email(value: &str) -> String {
    let inner = re!(r"<([^>]+)>")
        .captures(value)
        .and_then(|caps| caps.get(1))
        .map_or(value, |m| m.as_str());
    trim(inner).to_lowercase()
}

fn normalize_phone(value: &str) -> String {
    let digits: String = value.chars().filter(char::is_ascii_digit).collect();
    digits[digits.len().saturating_sub(10)..].to_string()
}

pub fn looks_like_system_inbound(from_email: Option<&str>, text: Option<&str>) -> bool {
    system_sender_re().is_match(from_email.unwrap_or(""))
        || system_content_re().is_match(text.unwrap_or(""))
}

/// "I'm driving", "Sent from my iPhone" and other auto-replies.
pub fn text_looks_like_auto_reply(text: &str) -> bool {
    let normalized = text
        .replace('\r', "")
        .split('\n')
        .map(trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let normalized = trim(&normalized);
    !normalized.is_empty()
        && re!(
            r"^i'?m driving\b|sent from my car\b|sent from my iphone\b|sent from my galaxy\b|automatic reply\b|auto-?reply\b|this is an automated text",
            "i"
        )
        .is_match(normalized)
}

/// An inbound message to classify against the customer's known identities.
#[derive(Debug, Clone, Copy)]
pub struct InboundMessage<'a> {
    pub sms: bool,
    pub from_email: Option<&'a str>,
    pub from_phone: Option<&'a str>,
    pub text: Option<&'a str>,
    pub customer_emails: &'a [String],
    pub customer_phones: &'a [String],
}

#[allow(clippy::too_many_lines)]
pub fn classify_inbound_message(message: &InboundMessage<'_>) -> InboundClassification {
    use InboundMessageType as T;
    use ReplyClassificationConfidence as C;
    let from_email = message.from_email.map(normalize_email).unwrap_or_default();
    let from_phone = message.from_phone.map(normalize_phone).unwrap_or_default();
    let text = message.text.unwrap_or("");
    let has_customer_emails = message
        .customer_emails
        .iter()
        .any(|e| !normalize_email(e).is_empty());
    let email_match = !from_email.is_empty()
        && message
            .customer_emails
            .iter()
            .any(|e| normalize_email(e) == from_email);
    let phone_match = !from_phone.is_empty()
        && message
            .customer_phones
            .iter()
            .any(|p| normalize_phone(p) == from_phone);
    let matched = email_match || phone_match;
    let sender_label = if !from_email.is_empty() {
        from_email.clone()
    } else if !from_phone.is_empty() {
        from_phone.clone()
    } else {
        "unknown".to_string()
    };
    let result = |kind, matched, sender_label: String, confidence, reason| InboundClassification {
        kind,
        matched_customer_identity: matched,
        sender_label,
        confidence,
        reason,
    };
    if text_looks_like_auto_reply(text) {
        return result(
            T::AutomatedMessage,
            matched,
            sender_label,
            C::High,
            "Content is an auto-reply or driving message, not meaningful project engagement",
        );
    }
    if looks_like_system_inbound(Some(message.from_email.unwrap_or(&from_email)), Some(text)) {
        return result(
            T::SystemMessage,
            matched,
            if from_email.is_empty() {
                "system".into()
            } else {
                from_email
            },
            C::High,
            "Sender or content is a recognized system/automated message",
        );
    }
    if matched {
        return result(
            T::CustomerReply,
            true,
            sender_label,
            C::High,
            "Sender matches a known customer email or phone",
        );
    }
    if !from_email.is_empty() && has_customer_emails {
        let mis_threaded = other_customer_contract_re().is_match(text);
        return result(
            if mis_threaded {
                T::MisThreadedMessage
            } else {
                T::ThirdPartyMessage
            },
            false,
            from_email,
            C::High,
            if mis_threaded {
                "Sender does not match customer and the content refers to another contract or party"
            } else {
                "Sender does not match a known customer email"
            },
        );
    }
    if message.sms && from_phone.is_empty() {
        return result(
            T::CustomerReply,
            true,
            "customer-phone".into(),
            C::Medium,
            "Inbound SMS on the customer phone thread with no conflicting sender",
        );
    }
    if !message.sms && from_email.is_empty() {
        if other_customer_contract_re().is_match(text) {
            return result(
                T::MisThreadedMessage,
                false,
                "unknown".into(),
                C::High,
                "Email content is a contract/signature notice for another party",
            );
        }
        return result(
            T::UnknownInbound,
            false,
            "unknown".into(),
            C::Low,
            "Sender identity could not be resolved; not treated as a customer reply",
        );
    }
    result(
        T::UnknownInbound,
        false,
        sender_label,
        C::Low,
        "Sender does not match customer; not treated as a customer reply",
    )
}

pub fn is_customer_reply_type(kind: Option<InboundMessageType>) -> bool {
    kind == Some(InboundMessageType::CustomerReply)
}
