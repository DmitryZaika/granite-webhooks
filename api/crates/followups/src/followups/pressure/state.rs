//! The customer's follow-up state, hold reasons, relationship and starting
//! phase, and timing checks for scheduled copy (part of
//! `smsFlowAiContactPressure.ts`).

use super::super::classify::{SalespersonOutboundType, classify_salesperson_outbound};
use super::super::dates::{Instant, iso, month_index, year};
use super::super::facts::{
    DealProductScope, EvidenceConfidence, FollowupFacts, QuoteType, TimelinePosition,
};
use super::super::intent::{MeaningfulCustomerIntentType, intent_prefers_low_pressure_phase};
use super::super::js::{fancy_test, js_regex, trim};
use super::super::timeline::{Channel, Direction};
use super::channels::parse_profile_line_when;
use super::timeline::{
    ClassifiedMessage, is_genuine_customer_inbound, is_semantic_customer_inbound, parse_message_at,
};
use super::types::{
    ChannelAttemptState, CommunicationHistoryStatus, ContactPressureLevel, CustomerFollowupState,
    DealState, EngagementState, FollowupContactPressure, FollowupPhase, HoldReasonEvidence,
    HoldReasonSource, HoldReasonType, HoldSourceType, InformationDensity, OperationalContactState,
    RelationshipAge, RelationshipState, TargetDealSummary, TerminalStatus,
};
use crate::{fre, re};
use chrono::Duration;

/// Inputs of [`classify_customer_followup_state`] beyond the pressure.
#[derive(Debug, Clone, Copy, Default)]
pub struct StateExtras {
    pub last_customer_message_wait: bool,
    pub quote_unanswered: bool,
    pub chase_days: Option<f64>,
    pub quote_type: Option<QuoteType>,
}

pub fn classify_customer_followup_state(
    pressure: &FollowupContactPressure,
    extras: &StateExtras,
    now: Instant,
) -> CustomerFollowupState {
    use CustomerFollowupState as S;
    let history = pressure.has_relationship_history;
    if pressure.crm_hold_until.is_some_and(|until| until > now) {
        return S::Hold;
    }
    if pressure
        .customer_requested_followup_at
        .is_some_and(|at| at > now)
        || extras.last_customer_message_wait
    {
        return S::Waiting;
    }
    if pressure.days_since_last_outbound.is_none()
        && pressure.unanswered_total_since_last_reply <= 0.0
    {
        return if history { S::Dormant } else { S::FreshLead };
    }
    if pressure.last_customer_message_at.is_some()
        && pressure.days_since_last_customer_reply.unwrap_or(999.0) <= 14.0
    {
        return S::RecentlyEngaged;
    }
    if pressure.quote_evidence && extras.quote_unanswered {
        return if matches!(
            extras.quote_type,
            Some(QuoteType::Countertop | QuoteType::FullRemodel)
        ) {
            S::QuoteGhost
        } else {
            S::StalledProject
        };
    }
    let silence_days = pressure
        .days_since_last_customer_reply
        .or(pressure.days_since_last_outbound)
        .unwrap_or(0.0);
    if silence_days >= 60.0 && pressure.sms_channel_state != ChannelAttemptState::Untested {
        return S::Dormant;
    }
    if pressure.last_customer_message_at.is_none() && pressure.days_since_last_outbound.is_some() {
        let both_tried = pressure.sms_channel_state != ChannelAttemptState::Untested
            && pressure.email_channel_state != ChannelAttemptState::Untested;
        if both_tried && pressure.unanswered_total_since_last_reply >= 3.0 {
            return S::RepeatedNonResponder;
        }
        if extras.chase_days.unwrap_or(0.0) >= 21.0
            || pressure.phone_channel_state == ChannelAttemptState::Unsuccessful
        {
            return if history {
                S::StalledProject
            } else {
                S::UnreachedLead
            };
        }
        return if history {
            S::StalledProject
        } else {
            S::NewLeadNoResponse
        };
    }
    if pressure.unanswered_total_since_last_reply >= 3.0
        && pressure.sms_channel_state != ChannelAttemptState::Untested
    {
        return S::RepeatedNonResponder;
    }
    if pressure.days_since_last_outbound.is_none() {
        return if history { S::Dormant } else { S::FreshLead };
    }
    if history {
        S::StalledProject
    } else {
        S::NewLeadNoResponse
    }
}

fn filled(value: Option<&String>) -> bool {
    value.is_some_and(|v| !v.is_empty())
}

pub fn infer_information_density(facts: &FollowupFacts) -> InformationDensity {
    let mut score = 0;
    let known_scope = facts.known_scope.as_deref().unwrap_or("");
    if !known_scope.is_empty() {
        score += 1;
    }
    if facts
        .project_details
        .iter()
        .any(|item| re!(r"\d+\s*sqft|\d+\s*(?:'|ft)", "i").is_match(item))
        || re!(r"\d+\s*(?:sq\.?\s*ft|sqft)", "i").is_match(known_scope)
    {
        score += 1;
    }
    if facts.sink_required || facts.sink_source.is_some() || facts.needs_sink {
        score += 1;
    }
    if facts.backsplash_known {
        score += 1;
    }
    if facts.existing_countertop_removal {
        score += 1;
    }
    if !facts.material_preferences.is_empty()
        || !facts.material_type_preferences.is_empty()
        || facts.uncertain_material_direction
    {
        score += 1;
    }
    if facts.quote_evidence || facts.quote_created {
        score += 1;
    }
    if facts.estimated_ready_at.is_some() || filled(facts.original_timeline_evidence.as_ref()) {
        score += 1;
    }
    if filled(facts.explicit_customer_request.as_ref()) {
        score += 1;
    }
    if facts.decision_obstacle != super::super::facts::DecisionObstacle::None
        || filled(facts.decision_obstacle_evidence.as_ref())
    {
        score += 1;
    }
    if score >= 5 {
        InformationDensity::High
    } else if score >= 2 {
        InformationDensity::Medium
    } else {
        InformationDensity::Low
    }
}

fn is_generic_first_touch_call_activity(line: &str) -> bool {
    if !line.starts_with("Activity:") {
        return false;
    }
    let stripped = re!(r"^Activity:\s*", "i").replace(line, "");
    let name = trim(stripped.split(" ·").next().unwrap_or(""));
    re!(r"^(call (him|her|them|back|the customer))\s*[.!?]?$", "i").is_match(name)
}

/// Deal notes, an old lead, real CRM activity or a logged call.
pub fn has_crm_relationship_history(profile: &str) -> bool {
    if re!(r"^Deal note ", "m").is_match(profile)
        || re!(r"\bold lead\b", "i").is_match(profile)
        || re!(r"\bOn Hold\b").is_match(profile)
    {
        return true;
    }
    if profile
        .split('\n')
        .filter(|line| line.starts_with("Activity:"))
        .any(|line| !is_generic_first_touch_call_activity(line))
    {
        return true;
    }
    re!(r"\bcalled\b", "i").is_match(profile)
}

/// A hold reason read from one text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoldReasonText {
    pub kind: HoldReasonType,
    pub confidence: EvidenceConfidence,
    pub evidence: String,
}

#[allow(clippy::too_many_lines)]
pub fn classify_hold_reason_from_text(text: &str) -> Option<HoldReasonText> {
    use EvidenceConfidence::{High, Medium};
    use HoldReasonType as H;
    let t = text.to_lowercase();
    let t = t.as_str();
    let found = |kind, confidence| {
        Some(HoldReasonText {
            kind,
            confidence,
            evidence: trim(text).to_string(),
        })
    };
    if re!(r"beginning of (the |next )?year|start of (the |next )?year|asked to call .{0,40}year").is_match(t)
        || re!(
            r"late (january|february|march|april|may|june|july|august|september|october|november|december)|ready in (january|february|march|april|may|june|july|august|september|october|november|december)|started in late|she said she'?ll be ready",
            "i"
        )
        .is_match(t)
    {
        return found(H::ProjectTimeline, High);
    }
    if re!(r"ready to start").is_match(t)
        && !fancy_test(
            fre!(
                r"waiting for cabinets|cabinets not installed|contractor delayed|construction delayed|\bnew build\b(?!er)|house (is )?being built"
            ),
            t,
        )
    {
        return found(H::ProjectTimeline, Medium);
    }
    if re!(r"higher priority").is_match(t)
        || re!(r"something of higher priority").is_match(t)
        || re!(r"other (project|priority)").is_match(t)
        || re!(r"working on something else").is_match(t)
    {
        return found(H::HigherPriorityDelay, High);
    }
    if re!(r"contact me (in|on|after)").is_match(t)
        || re!(r"reach out (in|on|after)").is_match(t)
        || re!(r"call me (in|on|after)").is_match(t)
        || re!(r"follow up (in|after)\b").is_match(t)
        || re!(r"follow up on (monday|tuesday|wednesday|thursday|friday|saturday|sunday|\d)")
            .is_match(t)
        || re!(r"wait until").is_match(t)
    {
        return found(H::CustomerRequestedDelay, High);
    }
    if re!(r"need to save").is_match(t)
        || re!(r"waiting for financ").is_match(t)
        || re!(r"budget is too (high|much)").is_match(t)
        || re!(r"spent .* on").is_match(t)
        || re!(r"spent money").is_match(t)
        || re!(r"can'?t afford").is_match(t)
    {
        return found(H::FinancialDelay, High);
    }
    if re!(r"family|health|personal|sick|hospital").is_match(t) {
        return found(H::PersonalIssue, Medium);
    }
    if fancy_test(fre!(r"\bnew build\b(?!er)"), t) || re!(r"house (is )?being built").is_match(t) {
        return found(H::ConstructionDependency, High);
    }
    if re!(r"ready in a year").is_match(t)
        || re!(r"will be ready in a year").is_match(t)
        || re!(r"ready in about a year").is_match(t)
        || re!(r"in a year approximat").is_match(t)
    {
        return found(H::ProjectTimeline, High);
    }
    if (re!(r"\bfall\b").is_match(t)
        || (re!(r"\bspring\b").is_match(t)
            && !re!(r"\bwhite spring\b|\bspring (?:green|valley|meadow)\b", "i").is_match(t)
            && !re!(r"\d+\s*k\s+white spring", "i").is_match(t))
        || re!(r"\bwinter\b").is_match(t)
        || re!(r"\bsummer\b").is_match(t))
        && (re!(r"\b(?:this|next|in the|during the|early|late)\s+(?:spring|fall|winter|summer)\b")
            .is_match(t)
            || re!(r"\b(?:spring|fall|winter|summer)\s+(?:of|season)\b").is_match(t))
    {
        return found(H::SeasonalDelay, Medium);
    }
    if re!(r"waiting for cabinets").is_match(t)
        || re!(r"cabinets not installed").is_match(t)
        || re!(r"cabinets? (are |is )?(unlevel|not level|bot leveled|unleveled)").is_match(t)
        || re!(r"waiting for (the )?floors").is_match(t)
        || re!(r"flooring (is )?on order").is_match(t)
        || re!(r"until (the )?floors are in place").is_match(t)
        || re!(r"no decisions will be made until").is_match(t)
        || re!(r"new build not ready").is_match(t)
        || re!(r"contractor delayed").is_match(t)
        || re!(r"construction delayed").is_match(t)
    {
        return found(H::ConstructionDependency, High);
    }
    if re!(r"\bdecid").is_match(t) {
        return found(H::DecisionDelay, Medium);
    }
    None
}

fn hold_from(
    text: HoldReasonText,
    source: HoldReasonSource,
    at: Option<Instant>,
) -> HoldReasonEvidence {
    HoldReasonEvidence {
        hold_reason_type: text.kind,
        hold_reason_source: Some(source),
        hold_reason_evidence: Some(text.evidence),
        hold_reason_at: at,
        hold_reason_confidence: text.confidence,
    }
}

/// Why the customer is on hold: their own messages first, then the lead
/// form, CRM activities, deal notes and the whole profile.
pub fn classify_hold_reason(
    profile: &str,
    timeline: &[ClassifiedMessage],
    has_hold: bool,
) -> Option<HoldReasonEvidence> {
    for message in timeline
        .iter()
        .rev()
        .filter(|m| is_genuine_customer_inbound(m))
    {
        if let Some(text) = classify_hold_reason_from_text(message.text()) {
            let source = if message.channel == Some(Channel::Email) {
                HoldReasonSource::CustomerEmail
            } else {
                HoldReasonSource::CustomerSms
            };
            return Some(hold_from(text, source, parse_message_at(&message.at)));
        }
    }
    let lines: Vec<&str> = profile.split('\n').collect();
    for line in lines
        .iter()
        .filter(|l| l.starts_with("Customer message:") || l.starts_with("Lead details:"))
    {
        if let Some(text) = classify_hold_reason_from_text(line) {
            return Some(hold_from(
                text,
                HoldReasonSource::LeadForm,
                parse_profile_line_when(line),
            ));
        }
    }
    for line in lines.iter().filter(|l| l.starts_with("Activity:")) {
        if re!(r"follow up\b", "i").is_match(line)
            && !re!(
                r"she said|he said|they said|customer (said|asked)|asked to ",
                "i"
            )
            .is_match(line)
        {
            return Some(HoldReasonEvidence {
                hold_reason_type: HoldReasonType::InternalFollowupSchedule,
                hold_reason_source: Some(HoldReasonSource::CrmActivity),
                hold_reason_evidence: Some(trim(line).to_string()),
                hold_reason_at: parse_profile_line_when(line),
                hold_reason_confidence: EvidenceConfidence::Medium,
            });
        }
        if let Some(text) = classify_hold_reason_from_text(line) {
            return Some(hold_from(
                text,
                HoldReasonSource::CrmActivity,
                parse_profile_line_when(line),
            ));
        }
    }
    for line in lines.iter().filter(|l| l.starts_with("Deal note ")) {
        if let Some(text) = classify_hold_reason_from_text(line) {
            return Some(hold_from(
                text,
                HoldReasonSource::SalespersonNote,
                parse_profile_line_when(line),
            ));
        }
    }
    if let Some(text) =
        classify_hold_reason_from_text(profile).filter(|t| t.confidence == EvidenceConfidence::High)
    {
        return Some(HoldReasonEvidence {
            hold_reason_type: text.kind,
            hold_reason_source: Some(HoldReasonSource::Deal),
            hold_reason_evidence: Some(text.evidence),
            hold_reason_at: None,
            hold_reason_confidence: EvidenceConfidence::Low,
        });
    }
    has_hold.then_some(HoldReasonEvidence {
        hold_reason_type: HoldReasonType::Unknown,
        hold_reason_source: None,
        hold_reason_evidence: None,
        hold_reason_at: None,
        hold_reason_confidence: EvidenceConfidence::Low,
    })
}

pub fn infer_hold_source_type(
    hold_reason_type: Option<HoldReasonType>,
    hold_reason_source: Option<HoldReasonSource>,
    profile: &str,
) -> Option<HoldSourceType> {
    if matches!(
        hold_reason_source,
        Some(HoldReasonSource::CustomerSms | HoldReasonSource::CustomerEmail)
    ) {
        return Some(HoldSourceType::CustomerRequested);
    }
    if matches!(
        hold_reason_type,
        Some(HoldReasonType::ConstructionDependency | HoldReasonType::ProjectTimeline)
    ) {
        return Some(HoldSourceType::ProjectDependency);
    }
    if re!(r"\bput it on hold\b", "i").is_match(profile) || re!(r"\bOn Hold\b").is_match(profile) {
        return Some(HoldSourceType::CrmManualHold);
    }
    if hold_reason_type == Some(HoldReasonType::InternalFollowupSchedule)
        || hold_reason_source == Some(HoldReasonSource::CrmActivity)
    {
        return Some(HoldSourceType::InternalFollowupTask);
    }
    hold_reason_type.map(|_| HoldSourceType::Unknown)
}

pub fn infer_hold_reason_type(profile: &str) -> HoldReasonType {
    classify_hold_reason_from_text(profile).map_or(HoldReasonType::Unknown, |t| t.kind)
}

/// Where a send date falls against the customer's ready date or window.
pub fn timeline_position_at(
    send_at: Instant,
    ready_at: Option<Instant>,
    window_start: Option<Instant>,
    window_end: Option<Instant>,
) -> Option<TimelinePosition> {
    let start = window_start.or(ready_at)?;
    let end = window_end.or(ready_at)?;
    Some(if send_at > end {
        TimelinePosition::AfterExpectedDate
    } else if send_at >= start - Duration::days(14) {
        TimelinePosition::NearExpectedDate
    } else {
        TimelinePosition::BeforeExpectedDate
    })
}

pub fn timeline_copy_is_valid(
    body: &str,
    send_at: Instant,
    ready_at: Option<Instant>,
    window_start: Option<Instant>,
    window_end: Option<Instant>,
) -> bool {
    let Some(position) = timeline_position_at(send_at, ready_at, window_start, window_end) else {
        return true;
    };
    let after = position == TimelinePosition::AfterExpectedDate;
    if after
        && re!(
            r"getting closer|still on track|coming up|as .+ approaches|ahead of",
            "i"
        )
        .is_match(body)
    {
        return false;
    }
    if after && re!(r"still on track for", "i").is_match(body) {
        return false;
    }
    if after
        && re!(r"\b(january|february|march|april|may|june|july|august|september|october|november|december)\b", "i").is_match(body)
        && re!(r"on track|getting closer|coming up", "i").is_match(body)
    {
        return false;
    }
    if re!(r"\bnext year\b", "i").is_match(body)
        && let Some(ready) = ready_at
        && year(send_at) >= year(ready)
    {
        return false;
    }
    true
}

const SEASONS: [(&str, [i32; 3]); 4] = [
    ("spring", [2, 3, 4]),
    ("summer", [5, 6, 7]),
    ("fall", [8, 9, 10]),
    ("winter", [11, 0, 1]),
];

fn season_of_month(month: i32) -> Option<&'static str> {
    SEASONS
        .iter()
        .find(|(_, months)| months.contains(&month))
        .map(|(season, _)| *season)
}

/// Relative time phrases that are wrong on the copy's own send date (copy
/// written today can go out months later).
pub fn relative_timing_copy_errors(
    body: &str,
    send_at: Instant,
    ready_at: Option<Instant>,
    window_start: Option<Instant>,
    window_end: Option<Instant>,
) -> Vec<String> {
    let Some(referenced) = ready_at.or(window_start).or(window_end) else {
        return Vec::new();
    };
    let mut errors = Vec::new();
    let (send_year, send_month) = (year(send_at), month_index(send_at));
    let (target_year, target_month) = (year(referenced), month_index(referenced));
    let send_day = iso(send_at)[..10].to_string();
    let mut flag = |phrase: &str, meaning: String| {
        errors.push(format!(
            "\"{phrase}\" is wrong on the {send_day} send date (it would mean {meaning}); use the month or \"your original timing\" instead"
        ));
    };
    if re!(r"\bnext year\b", "i").is_match(body) && target_year != send_year + 1 {
        flag(
            "next year",
            format!("{}, but the project window is {target_year}", send_year + 1),
        );
    }
    if re!(r"\bthis year\b", "i").is_match(body) && target_year != send_year {
        flag(
            "this year",
            format!("{send_year}, but the project window is {target_year}"),
        );
    }
    if re!(r"\blater this year\b", "i").is_match(body)
        && (target_year != send_year || target_month <= send_month)
    {
        flag(
            "later this year",
            format!("a month after {send_year}-{:02}", send_month + 1),
        );
    }
    if re!(r"\bnext month\b", "i").is_match(body) {
        let next_month = (send_month + 1) % 12;
        let next_month_year = if send_month == 11 {
            send_year + 1
        } else {
            send_year
        };
        if target_month != next_month || target_year != next_month_year {
            flag(
                "next month",
                format!("{next_month_year}-{:02}", next_month + 1),
            );
        }
    }
    for (season, _) in SEASONS {
        let target_season = season_of_month(target_month);
        if js_regex(&format!(r"\bnext {season}\b"), "i").is_match(body) {
            let expected_year = if season_of_month(send_month) == Some(season) {
                send_year + 1
            } else {
                send_year
            };
            if target_season != Some(season) || target_year != expected_year {
                flag(
                    &format!("next {season}"),
                    format!("{season} {expected_year}"),
                );
            }
        }
        if js_regex(&format!(r"\bthis {season}\b"), "i").is_match(body)
            && (target_season != Some(season) || target_year != send_year)
        {
            flag(&format!("this {season}"), format!("{season} {send_year}"));
        }
    }
    errors
}

const fn scope_from_mentions(countertops: bool, cabinets: bool) -> Option<DealProductScope> {
    match (countertops, cabinets) {
        (true, true) => Some(DealProductScope::CabinetsAndCountertops),
        (true, false) => Some(DealProductScope::Countertops),
        (false, true) => Some(DealProductScope::Cabinets),
        (false, false) => None,
    }
}

/// "Yes" right after "are you still interested in the countertop project?"
/// confirms that scope (only the immediately preceding question counts).
pub fn scope_from_conversational_antecedent(
    timeline: &[ClassifiedMessage],
) -> Option<(DealProductScope, String)> {
    for index in (1..timeline.len()).rev() {
        let reply = &timeline[index];
        if !is_semantic_customer_inbound(reply) {
            continue;
        }
        let text = trim(reply.text());
        if !re!(r"^(yes|yeah|yep|yup|sure|correct|absolutely|of course|definitely|still am|i am|we are)\b", "i").is_match(text) {
            continue;
        }
        let question = &timeline[index - 1];
        if question.direction != Direction::Outbound {
            continue;
        }
        let asked = question.text();
        if !re!(
            r"still (interested|planning|looking|active|on)|are you (still|interested)|is (the|your) .{0,40}(project|remodel) still|interested in",
            "i"
        )
        .is_match(asked)
        {
            continue;
        }
        if let Some(scope) = scope_from_mentions(
            re!(r"countertop|counter top", "i").is_match(asked),
            re!(r"cabinet", "i").is_match(asked),
        ) {
            return Some((scope, format!("{} → {text}", trim(asked))));
        }
    }
    None
}

pub fn scope_from_quote_discussion_context(
    timeline: &[ClassifiedMessage],
) -> Option<(DealProductScope, String)> {
    for index in (0..timeline.len()).rev() {
        let reply = &timeline[index];
        if !is_semantic_customer_inbound(reply)
            || !re!(r"checking on the quotes|review (?:the )?quote|looked at (?:the )?quote|checking on quotes", "i").is_match(reply.text())
        {
            continue;
        }
        for earlier in timeline[..index]
            .iter()
            .rev()
            .filter(|m| m.direction == Direction::Outbound)
        {
            let asked = earlier.text();
            let countertops =
                re!(r"purchasing countertops|countertop quote|countertops for your project|\bcountertops?\b", "i").is_match(asked);
            if let Some(scope) =
                scope_from_mentions(countertops, re!(r"\bcabinets?\b", "i").is_match(asked))
            {
                return Some((scope, format!("{} → {}", trim(asked), trim(reply.text()))));
            }
        }
    }
    None
}

pub fn infer_communication_history_status(
    has_outbound: bool,
    has_inbound: bool,
    has_relationship_history: bool,
    customer_state: CustomerFollowupState,
) -> CommunicationHistoryStatus {
    use CommunicationHistoryStatus as C;
    use CustomerFollowupState as S;
    if has_inbound && customer_state == S::RecentlyEngaged {
        return C::ActiveConversation;
    }
    if customer_state == S::Dormant {
        return C::Dormant;
    }
    if has_inbound || (has_outbound && has_relationship_history) {
        return C::PreviouslyContacted;
    }
    if has_outbound {
        return if matches!(
            customer_state,
            S::RepeatedNonResponder | S::QuoteGhost | S::StalledProject
        ) {
            C::PreviouslyContacted
        } else {
            C::TrulyNew
        };
    }
    if has_relationship_history {
        C::ExistingCustomerNoDigitalHistory
    } else {
        C::TrulyNew
    }
}

pub fn infer_relationship_state(
    profile: &str,
    status: CommunicationHistoryStatus,
) -> RelationshipState {
    if status == CommunicationHistoryStatus::TrulyNew {
        return RelationshipState::None;
    }
    if re!(r"\bold lead\b", "i").is_match(profile)
        || re!(r"\bOn Hold\b").is_match(profile)
        || matches!(
            status,
            CommunicationHistoryStatus::ExistingCustomerNoDigitalHistory
                | CommunicationHistoryStatus::Dormant
        )
    {
        return RelationshipState::ExistingOldLead;
    }
    RelationshipState::ExistingActive
}

pub const fn is_gentle_post_hold(hold_reason_type: Option<HoldReasonType>) -> bool {
    matches!(
        hold_reason_type,
        Some(
            HoldReasonType::FinancialDelay
                | HoldReasonType::PersonalIssue
                | HoldReasonType::SeasonalDelay
                | HoldReasonType::CustomerRequestedDelay
                | HoldReasonType::HigherPriorityDelay
        )
    )
}

pub fn last_outbound_text(timeline: &[ClassifiedMessage]) -> String {
    timeline
        .iter()
        .rev()
        .find(|m| m.direction == Direction::Outbound)
        .map(|m| m.text().to_string())
        .unwrap_or_default()
}

pub fn last_conversational_rep_reply_at(timeline: &[ClassifiedMessage]) -> Option<Instant> {
    timeline
        .iter()
        .rev()
        .filter(|m| m.direction == Direction::Outbound)
        .filter(|m| {
            !matches!(
                classify_salesperson_outbound(m.text()),
                SalespersonOutboundType::Promotion
                    | SalespersonOutboundType::System
                    | SalespersonOutboundType::Transactional
            )
        })
        .find_map(|m| parse_message_at(&m.at))
}

pub fn latest_outbound_type_from_timeline(
    timeline: &[ClassifiedMessage],
) -> Option<SalespersonOutboundType> {
    timeline
        .iter()
        .rev()
        .find(|m| m.direction == Direction::Outbound)
        .map(|m| classify_salesperson_outbound(m.text()))
}

pub const fn infer_relationship_age(state: RelationshipState) -> RelationshipAge {
    match state {
        RelationshipState::None => RelationshipAge::New,
        RelationshipState::ExistingOldLead => RelationshipAge::Old,
        RelationshipState::ExistingActive => RelationshipAge::Existing,
    }
}

pub fn infer_engagement_state(
    customer_state: CustomerFollowupState,
    relationship_state: RelationshipState,
    days_since_last_outbound: Option<f64>,
    last_customer_message_at: Option<Instant>,
) -> EngagementState {
    use CustomerFollowupState as S;
    use EngagementState as E;
    match customer_state {
        S::Hold => return E::Hold,
        S::RecentlyEngaged => return E::Responsive,
        _ => {}
    }
    if relationship_state == RelationshipState::ExistingOldLead
        && days_since_last_outbound.is_none()
    {
        return E::DormantNoDigitalHistory;
    }
    match customer_state {
        S::Dormant => return E::Dormant,
        S::Waiting => return E::Stalled,
        _ => {}
    }
    if last_customer_message_at.is_none()
        && let Some(days) = days_since_last_outbound
    {
        return if days <= 14.0 {
            E::RecentlyUnresponsive
        } else {
            E::Stalled
        };
    }
    if days_since_last_outbound.is_none() {
        E::Unreached
    } else {
        E::Stalled
    }
}

/// The phase a new sequence starts in.
pub fn select_followup_starting_phase(pressure: &FollowupContactPressure) -> FollowupPhase {
    use FollowupPhase as P;
    use MeaningfulCustomerIntentType as I;
    let intent = pressure
        .latest_meaningful_customer_intent
        .as_ref()
        .map(|i| i.kind);
    let extras = &pressure.additional_customer_intents;
    let low_pressure = intent_prefers_low_pressure_phase(intent)
        || extras
            .iter()
            .any(|item| intent_prefers_low_pressure_phase(Some(item.kind)));
    let unanswered = pressure.unanswered_total_since_last_reply;
    let days = pressure.days_since_last_outbound;
    let elevated = pressure.global_contact_pressure == ContactPressureLevel::Elevated;
    if pressure.customer_state == CustomerFollowupState::Hold
        || pressure.deal_stage_hold
        || pressure.communication_history_status
            == CommunicationHistoryStatus::ExistingCustomerNoDigitalHistory
    {
        return P::Reactivation;
    }
    if intent == Some(I::FutureProject)
        || intent == Some(I::NotNow)
        || extras.iter().any(|item| item.kind == I::NotNow)
    {
        return P::LongTermNurture;
    }
    if low_pressure {
        return if unanswered >= 3.0 || elevated || days.is_some_and(|d| d >= 30.0) {
            P::Decay
        } else {
            P::Reactivation
        };
    }
    if pressure.communication_history_status == CommunicationHistoryStatus::TrulyNew {
        let first_wave = unanswered <= 1.0 || (days.unwrap_or(0.0) < 2.0 && unanswered <= 2.0);
        return if first_wave {
            P::FirstFollowup
        } else {
            P::EarlyFollowup
        };
    }
    if unanswered >= 3.0 || elevated {
        return P::Decay;
    }
    if days.is_none() && pressure.relationship_state == RelationshipState::ExistingOldLead {
        return P::Reactivation;
    }
    if days.is_some_and(|d| d >= 45.0)
        || (pressure.customer_state == CustomerFollowupState::Dormant && days.is_some())
        || (pressure.engagement_state == EngagementState::Dormant && days.is_some())
    {
        return P::LongTermNurture;
    }
    P::ActivePursuit
}

pub fn infer_crm_deal_stage(
    target_deal: Option<&TargetDealSummary>,
    profile: &str,
) -> Option<String> {
    if let Some(label) = target_deal
        .map(|deal| deal.status_label.clone())
        .filter(|l| !l.is_empty())
    {
        return Some(label);
    }
    if let Some(caps) = re!(r"^Deal: Target #\d+ · ([^·\n]+)", "m").captures(profile) {
        return Some(trim(&caps[1]).to_string());
    }
    re!(r"^Deal: ([^·\n]+)", "m")
        .captures(profile)
        .map(|caps| trim(&caps[1]).to_string())
}

pub const fn infer_operational_contact_state(
    has_outbound: bool,
    has_inbound: bool,
    has_relationship_history: bool,
) -> OperationalContactState {
    if has_inbound {
        OperationalContactState::Responsive
    } else if has_outbound {
        OperationalContactState::ContactedNoResponse
    } else if has_relationship_history {
        OperationalContactState::NoDigitalHistory
    } else {
        OperationalContactState::Uncontacted
    }
}

pub fn infer_deal_state(
    profile: &str,
    quote_evidence: bool,
    terminal_status: Option<TerminalStatus>,
    has_outbound: bool,
) -> DealState {
    match terminal_status {
        Some(TerminalStatus::Won) => return DealState::Won,
        Some(TerminalStatus::Lost) => return DealState::Lost,
        None => {}
    }
    if re!(r"\bDeal:.*On Hold", "i").is_match(profile) {
        return DealState::OnHold;
    }
    if quote_evidence || re!(r"\bGot a [Qq]uote\b").is_match(profile) {
        return DealState::Quoted;
    }
    if has_outbound || re!(r"\bDeal:.*Contacted", "i").is_match(profile) {
        return DealState::Contacted;
    }
    DealState::NotContacted
}

pub fn open_call_activity_requires_manual_action(profile: &str) -> bool {
    let open_calls: Vec<&str> = profile
        .split('\n')
        .filter(|line| {
            line.starts_with("Activity:")
                && re!(r"\bcall (him|her|them|back|the customer)\b", "i").is_match(line)
                && !re!(r"\bcompleted\b").is_match(line)
        })
        .collect();
    if open_calls.is_empty() {
        return false;
    }
    let call_text = open_calls.join("\n");
    re!(r"\bcheck (?:what|whta).{0,48}(?:decided|choose|countertop|counterops|island|material|quote)", "i").is_match(&call_text)
        || re!(r"\bdecided on|choose the|wanted to choose|what (?:she|he|they) decided", "i").is_match(&call_text)
        || re!(r"\babout the (?:island|countertops?|quote|material)\b", "i").is_match(&call_text)
}
