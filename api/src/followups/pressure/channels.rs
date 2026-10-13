//! Customer-requested dates, quote evidence from the conversation, and the
//! state of each channel (part of `smsFlowAiContactPressure.ts`).

use super::super::crm_hold::relative_wait_days_from_note;
use super::super::dates::{Instant, utc_date, year};
use super::super::facts::quotes::{
    text_looks_like_quote_created, text_looks_like_quote_discussed,
    text_looks_like_quote_existence, text_looks_like_quote_received, text_looks_like_quote_request,
    text_looks_like_quote_sent,
};
use super::super::facts::{EvidenceConfidence, EvidenceSource};
use super::super::js::fancy_test;
use super::super::phone::any_valid_nanp_phone;
use super::super::timeline::{Channel, Direction, NormalizedSenderRole, TimelineMessage};
use super::timeline::{
    ClassifiedMessage, MS_PER_DAY, classify_timeline, is_genuine_customer_inbound,
    is_semantic_customer_inbound, last_genuine_customer_reply_at, last_message_at,
    parse_message_at,
};
use super::types::{
    ChannelAttemptState, ContactPressureLevel, CustomerIntent, EmailChannelStatus, FollowupAngle,
    PhoneChannelStatus, SmsChannelStatus,
};
use crate::{fre, re};
use chrono::Duration;
use regex::Regex;

const MONTH_INDEX: [(&str, i32); 24] = [
    ("january", 0),
    ("february", 1),
    ("march", 2),
    ("april", 3),
    ("may", 4),
    ("june", 5),
    ("july", 6),
    ("august", 7),
    ("september", 8),
    ("october", 9),
    ("november", 10),
    ("december", 11),
    ("jan", 0),
    ("feb", 1),
    ("mar", 2),
    ("apr", 3),
    ("jun", 5),
    ("jul", 6),
    ("aug", 7),
    ("sept", 8),
    ("sep", 8),
    ("oct", 9),
    ("nov", 10),
    ("dec", 11),
];

fn month_index_from_name(name: &str) -> Option<i32> {
    let key = name.to_lowercase();
    MONTH_INDEX
        .iter()
        .find(|(entry, _)| *entry == key)
        .map(|(_, month)| *month)
}

/// "Text me March 3rd": the next such date after `now`.
pub fn parse_explicit_followup_date(text: &str, now: Instant) -> Option<Instant> {
    let caps = re!(
        r"\b(january|february|march|april|may|june|july|august|september|october|november|december|jan|feb|mar|apr|jun|jul|aug|sept|sep|oct|nov|dec)\s+(\d{1,2})(?:st|nd|rd|th)?(?:,?\s*(20\d{2}))?\b",
        "i"
    )
    .captures(text)?;
    let month = month_index_from_name(caps.get(1)?.as_str())?;
    let day: i32 = caps.get(2)?.as_str().parse().ok()?;
    if !(1..=31).contains(&day) {
        return None;
    }
    let stated = caps.get(3).and_then(|m| m.as_str().parse::<i32>().ok());
    let stated_year = stated.unwrap_or_else(|| year(now));
    let mut date = utc_date(stated_year, month, day);
    if stated.is_none() && date <= now {
        date = utc_date(stated_year + 1, month, day);
    }
    (date > now).then_some(date)
}

/// The newest wait the customer asked for ("text me March 3", "in 2 weeks").
pub(crate) fn customer_requested_followup_at(
    timeline: &[ClassifiedMessage],
    now: Instant,
) -> Option<Instant> {
    for message in timeline
        .iter()
        .rev()
        .filter(|m| is_semantic_customer_inbound(m))
    {
        let text = message.text();
        if let Some(explicit) = parse_explicit_followup_date(text, now) {
            return Some(explicit);
        }
        if !looks_like_wait_request(text) {
            continue;
        }
        let Some(days) = relative_wait_days_from_note(text).filter(|d| *d > 0.0) else {
            continue;
        };
        let Some(at) = parse_message_at(&message.at) else {
            continue;
        };
        #[allow(clippy::cast_possible_truncation)]
        let until = at + Duration::milliseconds((days.min(1e8) * MS_PER_DAY) as i64);
        if until > now {
            return Some(until);
        }
    }
    None
}

pub fn looks_like_wait_request(text: &str) -> bool {
    re!(r"\bbe patient\b", "i").is_match(text)
        || re!(r"\bno rush\b", "i").is_match(text)
        || re!(r"\btry(ing)? to get (the )?money\b", "i").is_match(text)
        || re!(r"\bfollow up later\b", "i").is_match(text)
        || re!(r"\bready in\b", "i").is_match(text)
        || re!(r"\bin a (week|month)\b", "i").is_match(text)
        || re!(r"\bin \d+\s+(day|days|week|weeks|month|months)\b", "i").is_match(text)
        || re!(r"\d+\s*[-–]\s*\d+\s+days", "i").is_match(text)
        || re!(r"days would be best", "i").is_match(text)
        || re!(r"\bnot right now\b", "i").is_match(text)
        || re!(r"\bcheck back\b", "i").is_match(text)
}

/// Urgency from the customer's own lines and deal notes.
pub fn infer_customer_intent(profile: &str) -> CustomerIntent {
    let reliable = profile
        .split('\n')
        .filter(|line| {
            line.starts_with("Customer message:")
                || line.starts_with("Lead details:")
                || line.starts_with("Deal note ")
                || line.starts_with("When they want to start:")
        })
        .collect::<Vec<_>>()
        .join("\n")
        .to_lowercase();
    if reliable.is_empty() {
        return CustomerIntent::Normal;
    }
    let r = reliable.as_str();
    if re!(r"\bnext year\b").is_match(r)
        || re!(r"\bproject paused\b").is_match(r)
        || re!(r"\bcontact me in\b").is_match(r)
        || re!(r"\bwaiting for (cabinets|financing|money)\b").is_match(r)
    {
        return if r.contains("paused") {
            CustomerIntent::Paused
        } else {
            CustomerIntent::Deferred
        };
    }
    if re!(r"\bpersistent\b").is_match(r)
        || re!(r"\bfollow up\b").is_match(r)
        || re!(r"\bcall again\b").is_match(r)
        || re!(r"\bcheck again\b").is_match(r)
    {
        return CustomerIntent::Normal;
    }
    if re!(r"\basap\b").is_match(r)
        || re!(r"\burgent\b").is_match(r)
        || re!(r"\bimmediately\b").is_match(r)
        || re!(r"\bthis month\b").is_match(r)
        || re!(r"\bcabinets already installed\b").is_match(r)
        || re!(r"\bready to template\b").is_match(r)
        || re!(r"\bwants estimate quickly\b").is_match(r)
    {
        return CustomerIntent::HighUrgency;
    }
    CustomerIntent::Normal
}

/// Quote state read from the conversation and the CRM profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
#[allow(clippy::struct_excessive_bools)]
pub struct QuoteHistory {
    pub quote_requested: bool,
    pub quote_evidence: bool,
    pub quote_created: bool,
    pub quote_sent: bool,
    pub quote_received_or_discussed: bool,
    pub source: EvidenceSource,
    pub confidence: EvidenceConfidence,
}

#[allow(clippy::too_many_lines)]
pub fn quote_evidence_from_history(timeline: &[TimelineMessage], profile: &str) -> QuoteHistory {
    let profile_requested = text_looks_like_quote_request(profile);
    let mut quote_requested = profile_requested;
    let mut quote_created = text_looks_like_quote_created(profile);
    let mut quote_sent = text_looks_like_quote_sent(profile);
    let mut received = false;
    let mut source = EvidenceSource::None;
    let set_if_none = |source: &mut EvidenceSource, value: EvidenceSource| {
        if *source == EvidenceSource::None {
            *source = value;
        }
    };
    for message in timeline {
        let text = message.text();
        let semantic = is_semantic_customer_inbound(message);
        if semantic && text_looks_like_quote_request(text) {
            quote_requested = true;
        }
        let salesperson_authored = message.direction == Direction::Outbound
            || message.normalized_sender_role == Some(NormalizedSenderRole::SalesRep)
            || message.is_mirrored_duplicate == Some(true);
        if salesperson_authored && text_looks_like_quote_sent(text) {
            quote_sent = true;
            source = EvidenceSource::Conversation;
        }
        if semantic && quote_sent && re!(r"thanks|we('ll| will) be in touch", "i").is_match(text) {
            received = true;
            set_if_none(&mut source, EvidenceSource::CustomerMessage);
        }
        if semantic && text_looks_like_quote_received(text) {
            received = true;
            quote_sent = true;
            source = EvidenceSource::CustomerMessage;
        }
        if (text_looks_like_quote_discussed(text) && !text_looks_like_quote_received(text))
            || (semantic
                && re!(
                    r"highest of all my bids|your (numbers|quote).{0,40}(highest|high|expensive)|can you change this price|i like the quote",
                    "i"
                )
                .is_match(text))
        {
            received = true;
            set_if_none(&mut source, EvidenceSource::Conversation);
        }
    }
    if re!(r"^Activity:.*\bquote\b", "im").is_match(profile)
        && !profile_requested
        && (text_looks_like_quote_existence(profile)
            || re!(r"check the quote|expiration quote", "i").is_match(profile))
    {
        quote_created = true;
        set_if_none(&mut source, EvidenceSource::CrmActivity);
    }
    if re!(r"\bhave a quote\b", "i").is_match(profile) && !profile_requested {
        quote_created = true;
        set_if_none(&mut source, EvidenceSource::CrmNote);
    }
    if re!(r"\bDeal:.*Got a Quote\b", "i").is_match(profile) {
        quote_created = true;
        set_if_none(&mut source, EvidenceSource::DealRecord);
    }
    if (re!(r"quote (was )?(created|prepared)|cabinet quote|countertop quote|dekton quote|got .{0,20}quote", "i").is_match(profile)
        && !profile_requested)
        || (re!(r"existing quote", "i").is_match(profile) && re!(r"measurements|price breakdown|pricing", "i").is_match(profile))
    {
        quote_created = true;
        set_if_none(&mut source, EvidenceSource::SalespersonNote);
    }
    if received || quote_sent {
        quote_created = true;
        set_if_none(&mut source, EvidenceSource::Conversation);
    }
    if (text_looks_like_quote_received(profile)
        || fancy_test(
            fre!(
                r"\bgot (?:the |your )(?:\w+\s+)?quote\b|\bgot (?!a quote\b)\w{3,}\s+quote\b",
                "i"
            ),
            profile,
        ))
        && !re!(r"\bDeal:.*· Got a Quote\b", "i").is_match(profile)
    {
        received = true;
        quote_sent = true;
        quote_created = true;
        set_if_none(&mut source, EvidenceSource::SalespersonNote);
    }
    let quote_evidence = quote_created || quote_sent || received;
    QuoteHistory {
        quote_requested,
        quote_evidence,
        quote_created,
        quote_sent,
        quote_received_or_discussed: received,
        source: match (quote_evidence, source) {
            (false, _) => EvidenceSource::None,
            (true, EvidenceSource::None) => EvidenceSource::SalespersonNote,
            (true, source) => source,
        },
        confidence: if quote_evidence {
            EvidenceConfidence::High
        } else {
            EvidenceConfidence::Low
        },
    }
}

pub fn has_quote_evidence(timeline: &[TimelineMessage], profile: &str) -> bool {
    quote_evidence_from_history(timeline, profile).quote_evidence
}

fn crm_mentions_phone_attempt(profile: &str) -> bool {
    re!(r"\bcalled\b", "i").is_match(profile)
        || re!(r"\bleft (a )?(voice\s*mail|voice message)\b", "i").is_match(profile)
        || re!(r"\bdidn'?t an[s]?wer\b", "i").is_match(profile)
        || re!(r"\bno answer\b", "i").is_match(profile)
}

fn phone_doesnt_work(profile: &str) -> bool {
    re!(r"\bphone doesn'?t work\b|\bphone doesnt work\b", "i").is_match(profile)
}

pub fn infer_phone_channel_state(profile: &str) -> ChannelAttemptState {
    if sms_channel_status_from_history(profile, &[], &[], &[]) == SmsChannelStatus::WrongNumber {
        return ChannelAttemptState::Unsuccessful;
    }
    if crm_mentions_phone_attempt(profile) {
        return ChannelAttemptState::AttemptedNoResponse;
    }
    if re!(r"\bphone is off\b", "i").is_match(profile)
        || re!(r"\bphone attempts? unsuccessful\b", "i").is_match(profile)
        || phone_doesnt_work(profile)
    {
        return ChannelAttemptState::Unsuccessful;
    }
    ChannelAttemptState::Untested
}

fn wrong_phone_re() -> &'static Regex {
    re!(
        r"wrong\s+(?:phone\s+)?number|gave a wrong number|phone invalid|invalid (phone|number)|number disconnected|disconnected number",
        "i"
    )
}

fn wrong_email_re() -> &'static Regex {
    re!(
        r"wrong\s+(?:number\s+and\s+)?email|gave a wrong (?:number\s+and\s+)?email|invalid email|bad email|email (?:is )?(?:wrong|invalid)",
        "i"
    )
}

/// `parseProfileLineWhen`: a profile line's `(YYYY-MM-DD HH:MM` or `created ...` time.
pub(crate) fn parse_profile_line_when(line: &str) -> Option<Instant> {
    re!(r"\((\d{4}-\d{2}-\d{2}[ T]\d{2}:\d{2})")
        .captures(line)
        .or_else(|| re!(r"created (\d{4}-\d{2}-\d{2}[ T]\d{2}:\d{2})").captures(line))
        .and_then(|caps| parse_message_at(&caps[1]))
}

/// The newest dated line matching `pattern`; the epoch when only undated ones match.
fn latest_flagged_at(profile: &str, pattern: &Regex) -> Option<Instant> {
    let latest = profile
        .split('\n')
        .filter(|line| pattern.is_match(line))
        .filter_map(parse_profile_line_when)
        .max();
    if latest.is_none() && pattern.is_match(profile) {
        return super::super::dates::from_millis(0);
    }
    latest
}

fn latest_genuine_inbound_at(
    timeline: &[ClassifiedMessage],
    keep: impl Fn(Option<Channel>) -> bool,
) -> Option<Instant> {
    timeline
        .iter()
        .filter(|m| is_genuine_customer_inbound(m) && keep(m.channel))
        .filter_map(|m| parse_message_at(&m.at))
        .max()
}

fn channel_state(timeline: &[ClassifiedMessage], channel: Channel) -> ChannelAttemptState {
    let mut outbound = 0;
    let mut inbound = 0;
    for message in timeline.iter().filter(|m| m.channel == Some(channel)) {
        if message.direction == Direction::Outbound {
            outbound += 1;
        }
        if is_semantic_customer_inbound(message) {
            inbound += 1;
        }
    }
    if inbound > 0 {
        ChannelAttemptState::Replied
    } else if outbound > 0 {
        ChannelAttemptState::AttemptedNoResponse
    } else {
        ChannelAttemptState::Untested
    }
}

pub fn sms_channel_status_from_history(
    profile: &str,
    timeline: &[TimelineMessage],
    emails: &[String],
    phones: &[String],
) -> SmsChannelStatus {
    let classified = classify_timeline(timeline, emails, phones);
    if !phones.is_empty() && !any_valid_nanp_phone(phones) {
        return SmsChannelStatus::InvalidFormat;
    }
    if re!(r"\bopt[- ]?out\b", "i").is_match(profile)
        || re!(r"\bunsubscribe\b", "i").is_match(profile)
    {
        return SmsChannelStatus::OptedOut;
    }
    if let Some(wrong_at) = latest_flagged_at(profile, wrong_phone_re()) {
        let later_sms =
            latest_genuine_inbound_at(&classified, |c| c.is_none_or(|c| c == Channel::Sms));
        return if later_sms.is_some_and(|at| at > wrong_at) {
            SmsChannelStatus::Valid
        } else {
            SmsChannelStatus::WrongNumber
        };
    }
    match channel_state(&classified, Channel::Sms) {
        ChannelAttemptState::Replied => SmsChannelStatus::Valid,
        ChannelAttemptState::AttemptedNoResponse => SmsChannelStatus::AttemptedNoResponse,
        _ if re!(
            r"\btexted\b|\bsent (an? )?sms\b|\btexted (him|her|them|the customer)\b",
            "i"
        )
        .is_match(profile) =>
        {
            SmsChannelStatus::AttemptedUnknownResult
        }
        _ => SmsChannelStatus::Untested,
    }
}

pub fn email_channel_status_from_history(
    profile: &str,
    timeline: &[TimelineMessage],
    emails: &[String],
    phones: &[String],
) -> EmailChannelStatus {
    let classified = classify_timeline(timeline, emails, phones);
    if re!(r"\bbounce[d]?\b", "i").is_match(profile)
        || re!(r"\binvalid email\b", "i").is_match(profile)
    {
        return EmailChannelStatus::Bounced;
    }
    if let Some(wrong_at) = latest_flagged_at(profile, wrong_email_re()) {
        let later_email = latest_genuine_inbound_at(&classified, |c| c == Some(Channel::Email));
        if later_email.is_none_or(|at| at <= wrong_at) {
            return EmailChannelStatus::Invalid;
        }
    }
    if re!(r"\bunsubscribe\b", "i").is_match(profile)
        || re!(r"\bopt[- ]?out\b", "i").is_match(profile)
    {
        return EmailChannelStatus::OptedOut;
    }
    match channel_state(&classified, Channel::Email) {
        ChannelAttemptState::Replied => EmailChannelStatus::Valid,
        ChannelAttemptState::AttemptedNoResponse => EmailChannelStatus::AttemptedNoResponse,
        _ if re!(r"\bemailed\b|\bsent (an? )?email\b", "i").is_match(profile) => {
            EmailChannelStatus::AttemptedUnknownResult
        }
        _ => EmailChannelStatus::Untested,
    }
}

pub fn phone_channel_status_from_history(
    profile: &str,
    timeline: &[TimelineMessage],
    emails: &[String],
    phones: &[String],
) -> PhoneChannelStatus {
    let sms = sms_channel_status_from_history(profile, timeline, emails, phones);
    match sms {
        SmsChannelStatus::InvalidFormat => return PhoneChannelStatus::InvalidFormat,
        SmsChannelStatus::WrongNumber => return PhoneChannelStatus::WrongNumber,
        SmsChannelStatus::OptedOut => return PhoneChannelStatus::DoNotCall,
        _ => {}
    }
    if phone_doesnt_work(profile) {
        return PhoneChannelStatus::Unreachable;
    }
    if crm_mentions_phone_attempt(profile) {
        return PhoneChannelStatus::AttemptedNoResponse;
    }
    if sms == SmsChannelStatus::Valid {
        return PhoneChannelStatus::Valid;
    }
    if infer_phone_channel_state(profile) == ChannelAttemptState::Unsuccessful {
        return PhoneChannelStatus::Unreachable;
    }
    PhoneChannelStatus::Untested
}

pub const fn sms_channel_is_usable(status: SmsChannelStatus) -> bool {
    matches!(
        status,
        SmsChannelStatus::Valid
            | SmsChannelStatus::Untested
            | SmsChannelStatus::AttemptedNoResponse
            | SmsChannelStatus::AttemptedUnknownResult
            | SmsChannelStatus::Unknown
    )
}

pub const fn phone_channel_is_usable(status: PhoneChannelStatus) -> bool {
    matches!(
        status,
        PhoneChannelStatus::Valid
            | PhoneChannelStatus::Untested
            | PhoneChannelStatus::AttemptedNoResponse
            | PhoneChannelStatus::Unknown
    )
}

pub const fn email_channel_is_usable(status: EmailChannelStatus) -> bool {
    matches!(
        status,
        EmailChannelStatus::Valid
            | EmailChannelStatus::Untested
            | EmailChannelStatus::AttemptedNoResponse
            | EmailChannelStatus::AttemptedUnknownResult
            | EmailChannelStatus::Unknown
    )
}

pub(crate) fn sms_channel_state(timeline: &[ClassifiedMessage]) -> ChannelAttemptState {
    channel_state(timeline, Channel::Sms)
}

pub(crate) fn email_channel_state(timeline: &[ClassifiedMessage]) -> ChannelAttemptState {
    channel_state(timeline, Channel::Email)
}

pub(crate) fn pressure_level(unanswered: f64, state: ChannelAttemptState) -> ContactPressureLevel {
    if state == ChannelAttemptState::Untested {
        ContactPressureLevel::None
    } else if unanswered >= 3.0 || state == ChannelAttemptState::Unsuccessful {
        ContactPressureLevel::Elevated
    } else if unanswered >= 2.0 {
        ContactPressureLevel::Moderate
    } else if unanswered >= 1.0 || state == ChannelAttemptState::AttemptedNoResponse {
        ContactPressureLevel::Low
    } else {
        ContactPressureLevel::None
    }
}

const fn pressure_rank(level: ContactPressureLevel) -> u8 {
    match level {
        ContactPressureLevel::Elevated => 3,
        ContactPressureLevel::Moderate => 2,
        ContactPressureLevel::Low => 1,
        ContactPressureLevel::None => 0,
    }
}

pub(crate) const fn stronger_pressure(
    left: ContactPressureLevel,
    right: ContactPressureLevel,
) -> ContactPressureLevel {
    if pressure_rank(left) >= pressure_rank(right) {
        left
    } else {
        right
    }
}

pub(crate) fn global_pressure_level(
    total: f64,
    sms_state: ChannelAttemptState,
    outbound_last30_days: f64,
) -> ContactPressureLevel {
    let untested = sms_state == ChannelAttemptState::Untested;
    let unanswered = if total <= 0.0 {
        ContactPressureLevel::None
    } else if untested && total <= 3.0 {
        if total >= 3.0 {
            ContactPressureLevel::Moderate
        } else {
            ContactPressureLevel::Low
        }
    } else if total >= 3.0 && !untested {
        ContactPressureLevel::Elevated
    } else if total >= 3.0 {
        ContactPressureLevel::Moderate
    } else {
        ContactPressureLevel::Low
    };
    let volume = if outbound_last30_days >= 20.0 {
        ContactPressureLevel::Elevated
    } else if outbound_last30_days >= 10.0 {
        ContactPressureLevel::Moderate
    } else {
        ContactPressureLevel::None
    };
    stronger_pressure(unanswered, volume)
}

/// Angles our earlier messages already used.
pub fn used_followup_angles_from_timeline(timeline: &[TimelineMessage]) -> Vec<FollowupAngle> {
    let hints: [(FollowupAngle, &Regex); 9] = [
        (
            FollowupAngle::Promotion,
            re!(r"\b\d+%\s*off\b|\bsale\b|\bpromo", "i"),
        ),
        (
            FollowupAngle::EstimateOffer,
            re!(r"\bin-?home estimate\b|\bschedule.*estimate\b", "i"),
        ),
        (
            FollowupAngle::MaterialPreference,
            re!(
                r"\bcolor or style\b|\bmaterials?\b|\bquartz\b|\bgranite\b",
                "i"
            ),
        ),
        (
            FollowupAngle::InitialInterest,
            re!(
                r"\bstill interested\b|\breached out\b|\bthank you for your request\b",
                "i"
            ),
        ),
        (
            FollowupAngle::ProjectStatus,
            re!(
                r"\bany updates\b|\bstill interested in moving forward\b|\bproject still\b",
                "i"
            ),
        ),
        (
            FollowupAngle::OriginalTimeline,
            re!(r"\basap\b|\btiming changed\b|\bstill the plan\b", "i"),
        ),
        (
            FollowupAngle::DesignHelp,
            re!(r"\b3d design\b|\bbrowse our stone\b", "i"),
        ),
        (
            FollowupAngle::QuoteStatus,
            re!(r"\bcheck the quote\b|\byour quote\b", "i"),
        ),
        (
            FollowupAngle::CloseLoop,
            re!(
                r"\bgiven up\b|\bgo another direction\b|\bclose (the )?(loop|file)\b",
                "i"
            ),
        ),
    ];
    let mut used = Vec::new();
    for message in timeline
        .iter()
        .filter(|m| m.direction == Direction::Outbound)
    {
        for (angle, pattern) in &hints {
            if pattern.is_match(message.text()) && !used.contains(angle) {
                used.push(*angle);
            }
        }
    }
    used
}

pub fn customer_replied_after_last_outbound(
    timeline: &[TimelineMessage],
    emails: &[String],
    phones: &[String],
) -> bool {
    let classified = classify_timeline(timeline, emails, phones);
    let Some(last_inbound) = last_genuine_customer_reply_at(&classified) else {
        return false;
    };
    last_message_at(classified.iter().map(|m| &m.message), Direction::Outbound)
        .is_none_or(|outbound| last_inbound >= outbound)
}
