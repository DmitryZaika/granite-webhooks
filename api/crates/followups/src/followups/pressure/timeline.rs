//! Reading the message timeline for contact pressure: who each message is
//! really from, whether it belongs to this customer, and the counts and
//! gaps the cadence uses (part of `smsFlowAiContactPressure.ts`).

use super::super::classify::{
    InboundMessage, InboundMessageType, ReplyClassificationConfidence, classify_inbound_message,
    is_customer_reply_type, text_looks_like_auto_reply,
};
use super::super::conversation::annotate_conversation_events;
use super::super::dates::{Instant, from_millis};
use super::super::intent::{is_reaction_or_acknowledgment, message_time_ms};
use super::super::js::trim;
use super::super::store_time::store_zoned_parts;
use super::super::timeline::{Channel, Direction, NormalizedSenderRole, TimelineMessage};
use crate::{re, str_enum};
use std::collections::{BTreeMap, BTreeSet};

pub const MINUTES_PER_DAY: f64 = 24.0 * 60.0;
pub const MS_PER_DAY: f64 = MINUTES_PER_DAY * 60_000.0;

/// `parseMessageAt`: zone-less times are UTC.
pub fn parse_message_at(at: &str) -> Option<Instant> {
    message_time_ms(Some(at)).and_then(from_millis)
}

/// The store calendar day of a time, so a late-night text and a morning
/// text are different days.
pub fn store_calendar_day_key(at: Option<&str>) -> Option<String> {
    let at = at.filter(|at| !trim(at).is_empty())?;
    let parts = store_zoned_parts(parse_message_at(at)?);
    Some(format!("{}-{}-{}", parts.year, parts.month, parts.day))
}

/// Outbound messages on the same store day are one contact; an undated
/// message stays its own contact.
pub fn count_outbound_touches_by_store_day<'a>(
    ats: impl IntoIterator<Item = Option<&'a str>>,
) -> f64 {
    let mut days = BTreeSet::new();
    let mut undated = 0.0;
    for at in ats {
        match store_calendar_day_key(at) {
            Some(day) => {
                days.insert(day);
            }
            None => undated += 1.0,
        }
    }
    #[allow(clippy::cast_precision_loss)]
    let unique = days.len() as f64;
    unique + undated
}

str_enum! { pub enum TargetIdentityMembership { Yes = "YES", No = "NO", Uncertain = "UNCERTAIN" } }

/// A timeline message after classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifiedMessage {
    pub message: TimelineMessage,
    pub belongs_to_target_customer: Option<TargetIdentityMembership>,
    pub event_id: Option<String>,
    pub duplicate_of_message_id: Option<String>,
}

impl std::ops::Deref for ClassifiedMessage {
    type Target = TimelineMessage;
    fn deref(&self) -> &TimelineMessage {
        &self.message
    }
}

impl ClassifiedMessage {
    const fn belongs(&self) -> Option<TargetIdentityMembership> {
        self.belongs_to_target_customer
    }
}

pub fn extract_target_customer_name(profile: &str) -> Option<String> {
    re!(r"^Name:\s*(.+)$", "m")
        .captures(profile)
        .map(|caps| trim(&caps[1]).to_string())
}

fn name_tokens(name: &str) -> Vec<String> {
    re!(r"\s+")
        .split(&re!(r"[^a-z\s]", "g").replace_all(&name.to_lowercase(), " "))
        .filter(|token| super::super::js::js_len(token) > 1)
        .map(str::to_string)
        .collect()
}

fn text_references_other_customer(text: &str, target_name: Option<&str>) -> bool {
    let Some(target_name) = target_name.filter(|n| !n.is_empty()) else {
        return false;
    };
    if trim(text).is_empty() {
        return false;
    }
    let tokens = name_tokens(target_name);
    let first = tokens.first().cloned().unwrap_or_default();
    let last = tokens.last().cloned().unwrap_or_default();
    if first.is_empty() || last.is_empty() {
        return false;
    }
    let foreign = |party: &str| {
        let party = trim(party).to_lowercase();
        !party.is_empty() && !party.contains(&first) && !party.contains(&last)
    };
    if re!(
        r"\bbetween granite depot[^.]*and\s+([A-Za-z][A-Za-z\s.'-]{1,40})",
        "gi"
    )
    .captures_iter(text)
    .any(|caps| foreign(&caps[1]))
    {
        return true;
    }
    if re!(r"\bsent out for signature to\s+([^\n,.]+)", "gi")
        .captures_iter(text)
        .any(|caps| foreign(&caps[1]))
    {
        return true;
    }
    let full = trim(&format!("{first} {last}")).to_string();
    let target_lower = target_name.to_lowercase();
    if re!(
        r"\b(?:hi|hello|dear)\s+([A-Z][a-z]+(?:\s+[A-Z][a-z]+)?)\b",
        "g"
    )
    .captures_iter(text)
    .any(|caps| {
        let party = trim(&caps[1]).to_lowercase();
        !party.is_empty() && party != first && party != full && !target_lower.contains(&party)
    }) {
        return true;
    }
    re!(
        r"\bgoogle play\b|\btest message\b|\badobe acrobat sign\b",
        "i"
    )
    .is_match(text)
}

fn resolve_belongs_to_target_customer(
    message: &ClassifiedMessage,
    target_name: Option<&str>,
) -> TargetIdentityMembership {
    use InboundMessageType as T;
    use TargetIdentityMembership as M;
    if message.direction == Direction::Outbound {
        return M::Yes;
    }
    if message.is_mirrored_duplicate == Some(true) {
        return M::No;
    }
    let text = message.text();
    let conflict = message.sender_role_conflict == Some(true);
    let inbound_type = message.inbound_type;
    if inbound_type == Some(T::CustomerReply)
        && !conflict
        && !text_references_other_customer(text, target_name)
    {
        return M::Yes;
    }
    if matches!(
        inbound_type,
        Some(
            T::SystemMessage
                | T::MisThreadedMessage
                | T::ThirdPartyMessage
                | T::AutomatedMessage
                | T::InternalMessage
        )
    ) || conflict
        || text_references_other_customer(text, target_name)
    {
        return M::No;
    }
    let role = message.normalized_sender_role;
    if message.channel == Some(Channel::Sms) && role == Some(NormalizedSenderRole::Customer) {
        return M::Yes;
    }
    if message.matched_customer_identity == Some(true) && is_customer_reply_type(inbound_type) {
        return M::Yes;
    }
    if role == Some(NormalizedSenderRole::Customer)
        && message.matched_customer_identity != Some(false)
    {
        return M::Yes;
    }
    if message.channel == Some(Channel::Sms) && inbound_type == Some(T::CustomerReply) {
        return M::Yes;
    }
    if matches!(
        role,
        Some(NormalizedSenderRole::SalesRep | NormalizedSenderRole::System)
    ) {
        return M::No;
    }
    M::Uncertain
}

pub fn annotate_target_identity(timeline: &mut [ClassifiedMessage], target_name: Option<&str>) {
    for message in timeline {
        message.belongs_to_target_customer =
            Some(resolve_belongs_to_target_customer(message, target_name));
    }
}

pub fn pressure_eligible_timeline(timeline: &[ClassifiedMessage]) -> Vec<ClassifiedMessage> {
    timeline
        .iter()
        .filter(|m| m.belongs() == Some(TargetIdentityMembership::Yes))
        .cloned()
        .collect()
}

pub fn semantic_evidence_timeline(timeline: &[ClassifiedMessage]) -> Vec<ClassifiedMessage> {
    timeline
        .iter()
        .filter(|m| {
            m.belongs() == Some(TargetIdentityMembership::Yes)
                || (m.direction == Direction::Inbound
                    && (m.normalized_sender_role == Some(NormalizedSenderRole::SalesRep)
                        || m.is_mirrored_duplicate == Some(true)))
        })
        .cloned()
        .collect()
}

/// How much of the timeline is not this customer's.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct TimelineContamination {
    pub timeline_contamination_detected: bool,
    pub excluded_event_count: f64,
    pub identity_conflict_count: f64,
    pub system_event_count: f64,
    pub unmatched_event_count: f64,
}

pub fn summarize_timeline_contamination(timeline: &[ClassifiedMessage]) -> TimelineContamination {
    let mut summary = TimelineContamination::default();
    for message in timeline {
        if message.belongs() == Some(TargetIdentityMembership::No) {
            summary.excluded_event_count += 1.0;
        }
        if message.sender_role_conflict == Some(true) {
            summary.identity_conflict_count += 1.0;
        }
        if matches!(
            message.inbound_type,
            Some(InboundMessageType::SystemMessage | InboundMessageType::AutomatedMessage)
        ) {
            summary.system_event_count += 1.0;
        }
        if message.belongs() == Some(TargetIdentityMembership::Uncertain) {
            summary.unmatched_event_count += 1.0;
        }
    }
    summary.timeline_contamination_detected = summary.excluded_event_count >= 3.0;
    summary
}

#[allow(clippy::cast_precision_loss)]
pub fn days_between(later: Instant, earlier: Instant) -> f64 {
    (later - earlier).num_milliseconds() as f64 / MS_PER_DAY
}

fn names_strongly_disagree(crm: &[String], signature: &[String]) -> bool {
    let (Some(crm_first), Some(crm_last), Some(sig_first), Some(sig_last)) =
        (crm.first(), crm.last(), signature.first(), signature.last())
    else {
        return false;
    };
    if crm_first.is_empty() || crm_last.is_empty() || sig_first.is_empty() || sig_last.is_empty() {
        return false;
    }
    crm_last != sig_last
}

fn extract_inbound_signature_name(text: &str) -> Option<String> {
    text.split('\n').map(trim).filter(|line| !line.is_empty()).rev().find_map(|line| {
        if re!(r"confidentiality disclosure|thank you,? and i look forward", "i").is_match(line) {
            return None;
        }
        re!(r"^([A-Z][a-z]+(?:\s+[A-Z]\.?)?\s+[A-Z][a-z]+)(?:,\s*(?:Esq\.|Attorney|Counselor))?\.?$")
            .captures(line)
            .map(|caps| trim(&caps[1]).to_string())
    })
}

/// A message signed with a name that is not the CRM customer's.
pub fn detect_sender_identity_conflict(profile: &str, timeline: &[TimelineMessage]) -> bool {
    let Some(crm_name) = extract_target_customer_name(profile).filter(|n| !n.is_empty()) else {
        return false;
    };
    let crm_tokens = name_tokens(&crm_name);
    timeline.iter().filter(|m| m.inbound()).any(|message| {
        extract_inbound_signature_name(message.text())
            .is_some_and(|signature| names_strongly_disagree(&crm_tokens, &name_tokens(&signature)))
    })
}

fn classify_timeline_message(
    message: &TimelineMessage,
    emails: &[String],
    phones: &[String],
) -> TimelineMessage {
    let mut classified = message.clone();
    if message.direction != Direction::Inbound || message.inbound_type.is_some() {
        return classified;
    }
    classified.inbound_type = Some(
        classify_inbound_message(&InboundMessage {
            sms: message.channel == Some(Channel::Sms),
            from_email: message.from_email.as_deref(),
            from_phone: message.from_phone.as_deref(),
            text: message.text.as_deref(),
            customer_emails: emails,
            customer_phones: phones,
        })
        .kind,
    );
    classified
}

/// Classifies every message and fills in the sender roles.
pub fn classify_timeline(
    timeline: &[TimelineMessage],
    emails: &[String],
    phones: &[String],
) -> Vec<ClassifiedMessage> {
    let classified: Vec<TimelineMessage> = timeline
        .iter()
        .map(|m| classify_timeline_message(m, emails, phones))
        .collect();
    let events = annotate_conversation_events(&classified, emails, phones);
    classified
        .into_iter()
        .zip(events)
        .map(|(mut message, event)| {
            message.inbound_type = event.inbound_type;
            message.is_mirrored_duplicate = Some(event.is_mirrored_duplicate);
            message.normalized_sender_role = Some(event.normalized_sender_role);
            message.role_confidence = Some(event.role_confidence);
            message.matched_customer_identity = Some(event.matched_customer_identity);
            message.sender_role_conflict = Some(event.sender_role_conflict);
            ClassifiedMessage {
                message,
                belongs_to_target_customer: None,
                event_id: Some(event.id),
                duplicate_of_message_id: event.duplicate_of_message_id,
            }
        })
        .collect()
}

pub fn is_genuine_customer_inbound(message: &TimelineMessage) -> bool {
    message.is_mirrored_duplicate != Some(true)
        && message.sender_role_conflict != Some(true)
        && message
            .normalized_sender_role
            .is_none_or(|role| role == NormalizedSenderRole::Customer)
        && message.matched_customer_identity != Some(false)
        && message.inbound()
        && is_customer_reply_type(message.inbound_type)
}

/// A real customer message with content (not a mirror, an auto-reply or a tapback).
pub fn is_semantic_customer_inbound(message: &TimelineMessage) -> bool {
    if message.is_mirrored_duplicate == Some(true)
        || !message.inbound()
        || text_looks_like_auto_reply(message.text())
        || message.inbound_type == Some(InboundMessageType::AutomatedMessage)
        || message.sender_role_conflict == Some(true)
        || is_reaction_or_acknowledgment(message.text())
    {
        return false;
    }
    match message.normalized_sender_role {
        Some(
            NormalizedSenderRole::SalesRep
            | NormalizedSenderRole::System
            | NormalizedSenderRole::ThirdParty
            | NormalizedSenderRole::Unknown,
        ) => false,
        Some(NormalizedSenderRole::Customer)
            if message.role_confidence != Some(ReplyClassificationConfidence::Low) =>
        {
            true
        }
        _ => is_customer_reply_type(message.inbound_type),
    }
}

pub fn last_message_at<'a>(
    timeline: impl DoubleEndedIterator<Item = &'a TimelineMessage>,
    direction: Direction,
) -> Option<Instant> {
    timeline
        .rev()
        .filter(|m| m.direction == direction)
        .find_map(|m| parse_message_at(&m.at))
}

pub fn last_genuine_customer_reply_at(timeline: &[ClassifiedMessage]) -> Option<Instant> {
    timeline
        .iter()
        .rev()
        .filter(|m| is_genuine_customer_inbound(m))
        .find_map(|m| parse_message_at(&m.at))
}

pub fn last_semantic_customer_reply_at(timeline: &[ClassifiedMessage]) -> Option<Instant> {
    timeline
        .iter()
        .rev()
        .filter(|m| is_semantic_customer_inbound(m) && !trim(m.text()).is_empty())
        .find_map(|m| parse_message_at(&m.at))
}

pub fn last_inbound_text(timeline: &[ClassifiedMessage]) -> Option<String> {
    timeline
        .iter()
        .rev()
        .filter(|m| is_semantic_customer_inbound(m))
        .map(|m| trim(m.text()).to_string())
        .find(|text| !text.is_empty())
}

pub fn last_inbound_any(timeline: &[ClassifiedMessage]) -> Option<&ClassifiedMessage> {
    timeline.iter().rev().find(|m| m.inbound())
}

/// Our messages since the customer last wrote, counted per store day.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Unanswered {
    pub sms: f64,
    pub email: f64,
    pub total: f64,
}

pub fn unanswered_since_last_reply(timeline: &[ClassifiedMessage]) -> Unanswered {
    let mut pending: Vec<&ClassifiedMessage> = Vec::new();
    for message in timeline.iter().rev() {
        if is_semantic_customer_inbound(message) {
            break;
        }
        if message.direction == Direction::Outbound {
            pending.push(message);
        }
    }
    let count = |keep: &dyn Fn(Option<Channel>) -> bool| {
        count_outbound_touches_by_store_day(
            pending
                .iter()
                .filter(|m| keep(m.channel))
                .map(|m| Some(m.at.as_str())),
        )
    };
    Unanswered {
        sms: count(&|c| c == Some(Channel::Sms)),
        email: count(&|c| c == Some(Channel::Email)),
        total: count(&|c| c.is_some()),
    }
}

#[allow(clippy::cast_precision_loss)]
pub fn outbound_in_window<'a>(
    timeline: impl Iterator<Item = &'a TimelineMessage>,
    now: Instant,
    days: f64,
) -> f64 {
    let start = now.timestamp_millis() as f64 - days * MS_PER_DAY;
    count_outbound_touches_by_store_day(
        timeline
            .filter(|m| m.direction == Direction::Outbound)
            .filter(|m| {
                #[allow(clippy::cast_precision_loss)]
                parse_message_at(&m.at).is_some_and(|at| at.timestamp_millis() as f64 >= start)
            })
            .map(|m| Some(m.at.as_str())),
    )
}

/// Average days between our last five sending days.
pub fn average_gap_days<'a>(timeline: impl Iterator<Item = &'a TimelineMessage>) -> Option<f64> {
    let mut by_day: BTreeMap<String, Instant> = BTreeMap::new();
    for message in timeline.filter(|m| m.direction == Direction::Outbound) {
        let (Some(at), Some(day)) = (
            parse_message_at(&message.at),
            store_calendar_day_key(Some(&message.at)),
        ) else {
            continue;
        };
        let entry = by_day.entry(day).or_insert(at);
        if at > *entry {
            *entry = at;
        }
    }
    let mut times: Vec<Instant> = by_day.into_values().collect();
    times.sort();
    let recent = &times[times.len().saturating_sub(5)..];
    if recent.len() < 2 {
        return None;
    }
    let total: f64 = recent
        .windows(2)
        .map(|pair| days_between(pair[1], pair[0]))
        .sum();
    #[allow(clippy::cast_precision_loss)]
    let gaps = (recent.len() - 1) as f64;
    Some(total / gaps)
}
