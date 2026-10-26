//! Who really wrote each message, and whether a person owns the next step.
//!
//! A message can be the customer's, the rep's own text mirrored back, or a
//! system notice. Ports of `smsFlowAiConversationEvents.ts` and
//! `smsFlowAiRepOwnedNextStep.ts`.

use super::classify::{
    InboundMessage, InboundMessageType, ReplyClassificationConfidence, classify_inbound_message,
    is_customer_reply_type, text_looks_like_auto_reply,
};
use super::facts::LatestCustomerState;
use super::intent::{
    MeaningfulCustomerIntent, MeaningfulCustomerIntentType, is_reaction_or_acknowledgment,
};
use super::js::{js_regex, trim};
use super::timeline::{
    Channel, Direction, NormalizedSenderRole, TimelineMessage, parse_event_at, time_with_z,
};
use crate::{re, str_enum};
use serde::Serialize;

const MIRROR_WINDOW_MS: i64 = 2 * 60 * 1000;

/// A timeline message with its sender worked out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[allow(clippy::struct_excessive_bools)]
pub struct ConversationEvent {
    pub id: String,
    pub at: String,
    pub at_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<Channel>,
    pub direction: Direction,
    pub text: String,
    pub from_email: Option<String>,
    pub from_phone: Option<String>,
    pub inbound_type: Option<InboundMessageType>,
    pub matched_customer_identity: bool,
    pub sender_role_conflict: bool,
    pub normalized_sender_role: NormalizedSenderRole,
    pub role_confidence: ReplyClassificationConfidence,
    pub is_mirrored_duplicate: bool,
    pub duplicate_of_message_id: Option<String>,
}

/// A tapback ("Liked “...”"), a heart, or a bare "thanks".
pub fn text_looks_like_acknowledgment_reaction(text: &str) -> bool {
    let trimmed = trim(text);
    if trimmed.is_empty() {
        return false;
    }
    re!(r#"^(liked|loved)\s+[“"']"#, "i").is_match(trimmed)
        || re!(r"^(👍|❤️|❤|♥)$", "u").is_match(trimmed)
        || is_reaction_or_acknowledgment(trimmed)
}

/// Lower case with runs of whitespace squashed.
pub fn normalize_conversation_text(text: &str) -> String {
    trim(&re!(r"\s+").replace_all(&text.to_lowercase(), " ")).to_string()
}

pub fn texts_look_mirrored(left: &str, right: &str) -> bool {
    let a = normalize_conversation_text(left);
    let b = normalize_conversation_text(right);
    !a.is_empty() && !b.is_empty() && a == b
}

/// Text only the salesperson writes ("this is Dima from Granite Depot").
pub fn text_looks_like_salesperson_authored(text: &str) -> bool {
    re!(
        r"this is \w[\w' -]{0,40} (with|from|at) granite depot|this is (dema|dima)\b|just sent you (the |your )?(updated )?quote|just sent all the quotes|would you like me (to )?add|have you given up on|have you had a chance to (compare|check|review|pick)|looks like laminate|our edges|which edge do you prefer|these options will be under|i just noticed that i wrote you a message|granite-manager\.com\/employee|resent it\. i got your email wrong|hey \w+, this is |hi[,]?\s+\w+[,!]+\s+(this is |i('m| am) following up|have you |i just |are you still int)|good (morning|afternoon|evening)[,]?\s+\w+|are you still int[e]?rested|i('m| am) following up (to see|on|with)|thanks for reaching out to granite depot",
        "i"
    )
    .is_match(text)
}

/// The sender's role from the message direction and inbound type.
pub fn normalize_sender_role(
    direction: Direction,
    inbound_type: Option<InboundMessageType>,
    matched_customer_identity: bool,
) -> (NormalizedSenderRole, ReplyClassificationConfidence) {
    use InboundMessageType as T;
    use NormalizedSenderRole as R;
    use ReplyClassificationConfidence as C;
    if direction == Direction::Outbound {
        return (R::SalesRep, C::High);
    }
    match inbound_type {
        Some(T::SystemMessage | T::AutomatedMessage) => (R::System, C::High),
        Some(T::ThirdPartyMessage | T::MisThreadedMessage) => (R::ThirdParty, C::High),
        Some(T::InternalMessage) => (R::SalesRep, C::Medium),
        kind if is_customer_reply_type(kind) => (
            R::Customer,
            if matched_customer_identity {
                C::High
            } else {
                C::Medium
            },
        ),
        _ => (R::Unknown, C::Low),
    }
}

/// Classifies every message, marks inbound copies of the rep's own texts
/// (mirrored within two minutes) and fixes roles from the content.
pub fn annotate_conversation_events(
    timeline: &[TimelineMessage],
    emails: &[String],
    phones: &[String],
) -> Vec<ConversationEvent> {
    let mut events: Vec<ConversationEvent> = timeline
        .iter()
        .enumerate()
        .map(|(index, message)| {
            let classification = classify_inbound_message(&InboundMessage {
                sms: message.channel == Some(Channel::Sms),
                from_email: message.from_email.as_deref(),
                from_phone: message.from_phone.as_deref(),
                text: message.text.as_deref(),
                customer_emails: emails,
                customer_phones: phones,
            });
            let inbound_type = message
                .inbound()
                .then(|| message.inbound_type.unwrap_or(classification.kind));
            let matched = inbound_type.is_some() && classification.matched_customer_identity;
            let (role, confidence) =
                normalize_sender_role(message.direction, inbound_type, matched);
            ConversationEvent {
                id: format!("{}:{}:{index}", message.at, message.direction),
                at: message.at.clone(),
                at_ms: parse_event_at(&message.at),
                channel: message.channel,
                direction: message.direction,
                text: message.text().to_string(),
                from_email: message.from_email.clone(),
                from_phone: message.from_phone.clone(),
                inbound_type,
                matched_customer_identity: matched,
                sender_role_conflict: false,
                normalized_sender_role: role,
                role_confidence: confidence,
                is_mirrored_duplicate: false,
                duplicate_of_message_id: None,
            }
        })
        .collect();
    for index in 0..events.len() {
        if events[index].direction != Direction::Inbound || trim(&events[index].text).is_empty() {
            continue;
        }
        for earlier in (0..index).rev() {
            let prior = &events[earlier];
            if prior.direction != Direction::Outbound {
                continue;
            }
            if let (Some(event_at), Some(prior_at)) = (events[index].at_ms, prior.at_ms) {
                if event_at - prior_at > MIRROR_WINDOW_MS {
                    break;
                }
                if event_at < prior_at {
                    continue;
                }
            }
            if !texts_look_mirrored(&events[index].text, &prior.text) {
                continue;
            }
            let prior_id = prior.id.clone();
            let event = &mut events[index];
            event.is_mirrored_duplicate = true;
            event.duplicate_of_message_id = Some(prior_id);
            event.normalized_sender_role = NormalizedSenderRole::SalesRep;
            event.role_confidence = ReplyClassificationConfidence::High;
            break;
        }
    }
    for event in &mut events {
        if event.direction != Direction::Inbound || event.is_mirrored_duplicate {
            continue;
        }
        if text_looks_like_acknowledgment_reaction(&event.text) {
            event.normalized_sender_role = NormalizedSenderRole::Customer;
            event.role_confidence = ReplyClassificationConfidence::High;
            continue;
        }
        if text_looks_like_salesperson_authored(&event.text) {
            let labeled_customer = event.normalized_sender_role == NormalizedSenderRole::Customer
                || event.inbound_type == Some(InboundMessageType::CustomerReply);
            event.sender_role_conflict = labeled_customer;
            event.normalized_sender_role = NormalizedSenderRole::SalesRep;
            event.role_confidence = if labeled_customer {
                ReplyClassificationConfidence::Medium
            } else {
                ReplyClassificationConfidence::High
            };
            event.matched_customer_identity = false;
            continue;
        }
        if event.channel == Some(Channel::Sms)
            && (event.inbound_type == Some(InboundMessageType::UnknownInbound)
                || event.normalized_sender_role == NormalizedSenderRole::Unknown)
            && !trim(&event.text).is_empty()
        {
            event.normalized_sender_role = NormalizedSenderRole::Customer;
            event.role_confidence = ReplyClassificationConfidence::Medium;
        }
    }
    events
}

/// A message the customer really wrote (not a mirror, an auto-reply or a
/// low-confidence guess).
pub fn is_semantic_customer_event(event: &ConversationEvent) -> bool {
    !event.is_mirrored_duplicate
        && !text_looks_like_auto_reply(&event.text)
        && event.inbound_type != Some(InboundMessageType::AutomatedMessage)
        && event.normalized_sender_role == NormalizedSenderRole::Customer
        && event.role_confidence != ReplyClassificationConfidence::Low
}

str_enum! {
    /// The next step belongs to a person, not to an automated sequence.
    pub enum RepOwnedNextStep {
        ProjectMovingForward = "PROJECT_MOVING_FORWARD", UnansweredProjectMessage = "UNANSWERED_PROJECT_MESSAGE",
        AppointmentBooked = "APPOINTMENT_BOOKED",
    }
}

const MS_PER_DAY: f64 = 24.0 * 60.0 * 60.0 * 1000.0;
/// Messages sent in the same minute count as one send (a link and the booking text).
const SAME_SEND_MS: i64 = 60_000;
/// A booked appointment owns the next step for this long, then automation may resume.
pub const APPOINTMENT_BOOKED_WINDOW_DAYS: f64 = 21.0;
/// An unanswered project message this old no longer blocks automation.
pub const UNANSWERED_PROJECT_MESSAGE_WINDOW_DAYS: f64 = 30.0;

fn mentions_customer(text: &str, customer_name: Option<&str>) -> bool {
    customer_name
        .unwrap_or("")
        .split_whitespace()
        .map(|part| re!(r"[^a-z'-]", "gi").replace_all(part, "").into_owned())
        .filter(|part| super::js::js_len(part) >= 3)
        .any(|name| js_regex(&format!(r"\b{name}\b"), "i").is_match(text))
}

/// What [`rep_owned_next_step`] reads.
pub struct RepOwnedInputs<'a> {
    pub timeline: &'a [TimelineMessage],
    pub latest_customer_state: Option<LatestCustomerState>,
    pub latest_intent: Option<&'a MeaningfulCustomerIntent>,
    /// Sender classification of the latest inbound message.
    pub latest_inbound_type: Option<InboundMessageType>,
    /// The customer's name from the profile; ties a message from another sender to this project.
    pub customer_name: Option<&'a str>,
    pub now_ms: i64,
}

#[allow(clippy::cast_precision_loss)]
pub fn rep_owned_next_step(params: &RepOwnedInputs<'_>) -> Option<RepOwnedNextStep> {
    if params.latest_customer_state == Some(LatestCustomerState::TemplateApproved) {
        return Some(RepOwnedNextStep::ProjectMovingForward);
    }
    if let Some(intent) = params.latest_intent
        && intent.kind == MeaningfulCustomerIntentType::MovingForward
        && re!(r"\b(signed|contract|deposit|paid|payment|check)\b", "i").is_match(&intent.evidence)
    {
        return Some(RepOwnedNextStep::ProjectMovingForward);
    }
    let mut ordered: Vec<(&TimelineMessage, i64)> = params
        .timeline
        .iter()
        .filter_map(|message| time_with_z(&message.at).map(|at| (message, at)))
        .collect();
    ordered.sort_by_key(|(_, at)| *at);
    let (last, last_at) = *ordered.last()?;
    let age_days = (params.now_ms - last_at) as f64 / MS_PER_DAY;
    // Someone on the project (often a spouse writing from another address,
    // naming the customer) asked us something and nobody answered.
    if last.inbound()
        && last.inbound_type.or(params.latest_inbound_type)
            == Some(InboundMessageType::ThirdPartyMessage)
        && mentions_customer(last.text(), params.customer_name)
        && re!(
            r"\?|\bquestions?\b|\bcan you\b|\bcould you\b|\bplease\b",
            "i"
        )
        .is_match(last.text())
        && age_days <= UNANSWERED_PROJECT_MESSAGE_WINDOW_DAYS
    {
        return Some(RepOwnedNextStep::UnansweredProjectMessage);
    }
    // Our last word booked an appointment and the customer has not written
    // since: the rep attends it or records the outcome first.
    let last_inbound_at = ordered
        .iter()
        .filter(|(m, _)| m.inbound())
        .map(|(_, at)| *at)
        .max();
    let last_outbound_at = ordered
        .iter()
        .filter(|(m, _)| !m.inbound())
        .map(|(_, at)| *at)
        .max();
    let booking = ordered.iter().find(|(message, at)| {
        !message.inbound()
            && last_inbound_at.is_none_or(|inbound| *at > inbound)
            && last_outbound_at.is_some_and(|outbound| outbound - at <= SAME_SEND_MS)
            && re!(
                r"\b(appointment|estimate|measure(?:ment)?|template|consultation|visit)\b[^.?!\n]{0,40}\b(?:is |has been )?(scheduled|booked|confirmed)\b|\bsee you (?:on|at|tomorrow)\b",
                "i"
            )
            .is_match(message.text())
    });
    match booking {
        Some((_, at))
            if (params.now_ms - at) as f64 / MS_PER_DAY <= APPOINTMENT_BOOKED_WINDOW_DAYS =>
        {
            Some(RepOwnedNextStep::AppointmentBooked)
        }
        _ => None,
    }
}
