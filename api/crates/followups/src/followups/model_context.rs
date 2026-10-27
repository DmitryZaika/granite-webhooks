//! What the model reads for a `NEW_CONTEXT` cadence: a compact, typed view
//! of the customer, project, conversation and facts, and its prompt text
//! (port of `aiFollowupModelContext.ts`).

use super::cadence::objective::copy_objective_instruction;
use super::cadence::plan::planner_copy_objective;
use super::cadence::types::{CopySemanticObjective, PlannedAiCadenceStep};
use super::dates::{Instant, iso};
use super::facts::{
    DealProductScope, EvidenceConfidence, EvidenceSource, MaterialDirectionStatus,
    UnfinishedNextStepKind,
};
use super::instruction::admin_instruction_gap_minutes;
use super::js::{js_len, js_number, js_slice, trim};
use super::pressure::{CommunicationHistoryStatus, FollowupContactPressure, HoldReasonSource};
use super::prompt::{
    FollowupTemplateExample, FollowupTimelineMessage, TemplateScope, stones_link_already_sent,
};
use super::send_window::FollowupChannel;
use super::style::{FollowupStyleProfile, first_name_from_customer_profile};
use super::timeline::Direction;
use crate::{re, str_enum};
use serde::Serialize;

str_enum! { pub enum FactConfidence {
    Confirmed = "confirmed", CustomerStated = "customer_stated", RepOrCrm = "rep_or_crm", Inferred = "inferred", Unknown = "unknown",
} }

str_enum! { pub enum LeadKind { FreshUnanswered = "fresh_unanswered", ActiveThread = "active_thread", Reactivation = "reactivation" } }

str_enum! { pub enum Speaker { Customer = "customer", Rep = "rep" } }

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConversationTurn {
    pub at: String,
    pub channel: FollowupChannel,
    pub speaker: Speaker,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RelevantFact {
    pub key: String,
    pub value: String,
    pub confidence: FactConfidence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteSnapshot {
    pub requested: bool,
    pub sent: bool,
    pub received_or_discussed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomerContext {
    pub first_name: Option<String>,
    /// First name of the customer's sales rep (the "you" of every message).
    pub salesperson_first_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectContext {
    pub scope: DealProductScope,
    pub scope_confidence: FactConfidence,
    pub known_scope_note: Option<String>,
    pub quote: Option<QuoteSnapshot>,
    pub other_open_projects: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HoldContext {
    pub until: String,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationContext {
    pub lead_kind: LeadKind,
    pub current_state: String,
    pub latest_customer_intent: Option<String>,
    pub unresolved_request: Option<String>,
    pub days_since_last_outbound: Option<f64>,
    pub days_since_last_customer_reply: Option<f64>,
    pub unanswered_outbound_count: f64,
    /// Whether we ever texted this customer.
    pub sms_sent_before: bool,
    pub hold: Option<HoldContext>,
    pub latest_customer_message: Option<ConversationTurn>,
    pub latest_rep_message: Option<ConversationTurn>,
    pub recent_relevant: Vec<ConversationTurn>,
}

str_enum! { pub enum CrmNoteSource { CustomerForm = "customer form", RepNote = "rep note" } }

/// What the customer wrote on the lead form, or a rep's note.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CrmNote {
    pub source: CrmNoteSource,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceSample {
    pub channel: FollowupChannel,
    pub body_preview: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SharedModelContext {
    pub customer: CustomerContext,
    pub project: ProjectContext,
    pub conversation: ConversationContext,
    pub crm_notes: Vec<CrmNote>,
    pub style: Option<FollowupStyleProfile>,
    pub voice_samples: Vec<VoiceSample>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub admin_instruction: Option<String>,
    /// The company's current offer, while it stands.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_offer: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskContext {
    pub step_index: usize,
    pub channel: FollowupChannel,
    /// Always `MODEL_CHOOSES`: the model picks each message's approach.
    pub objective: String,
    pub approach: String,
    pub task_description: String,
    pub expected_send_on: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StepModelContext {
    pub task: TaskContext,
    pub relevant_facts: Vec<RelevantFact>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CadenceModelContext {
    pub shared: SharedModelContext,
    /// Facts any message may use, rendered once.
    pub known_facts: Vec<RelevantFact>,
    pub steps: Vec<StepModelContext>,
}

/// What the context is built from (the gathered follow-up context).
#[derive(Debug, Clone, Copy)]
pub struct ModelContextSource<'a> {
    pub profile: &'a str,
    pub timeline: &'a [FollowupTimelineMessage],
    pub templates: &'a [FollowupTemplateExample],
    pub admin_instruction: Option<&'a str>,
    pub contact_pressure: &'a FollowupContactPressure,
    pub style_profile: Option<&'a FollowupStyleProfile>,
    /// The company's current offer, while it stands ("… (until <day>)").
    pub current_offer: Option<&'a str>,
}

/// The task text for an objective; with an unknown product scope it never
/// names a product category.
pub fn refactored_task_description(
    objective: CopySemanticObjective,
    scope: DealProductScope,
) -> &'static str {
    if scope == DealProductScope::Unknown {
        match objective {
            CopySemanticObjective::Scope => {
                return "Discover the customer's project area/scope without assuming a product category. Ask what project or area they are working on.";
            }
            CopySemanticObjective::MaterialType => {
                return "Ask whether they have a material direction in mind. Do not name a product category or material family unless it appears in relevant facts.";
            }
            CopySemanticObjective::Color | CopySemanticObjective::Design => {
                return "Ask about the look they want without naming a product category or material family unless it appears in relevant facts.";
            }
            _ => {}
        }
    }
    copy_objective_instruction(objective)
}

const RECENT_MESSAGE_LIMIT: usize = 6;
const MESSAGE_CHAR_LIMIT: usize = 280;
const VOICE_SAMPLE_LIMIT: usize = 2;
const VOICE_SAMPLE_CHAR_LIMIT: usize = 160;

const fn is_customer_evidence(source: EvidenceSource) -> bool {
    matches!(
        source,
        EvidenceSource::CustomerMessage
            | EvidenceSource::CustomerSms
            | EvidenceSource::CustomerReplyToContext
            | EvidenceSource::CustomerEmail
    )
}

const fn is_crm_evidence(source: EvidenceSource) -> bool {
    matches!(
        source,
        EvidenceSource::SalespersonNote
            | EvidenceSource::SalespersonEmail
            | EvidenceSource::DealRecord
            | EvidenceSource::CrmActivity
            | EvidenceSource::CrmNote
    )
}

str_enum! { pub enum ProductFamily {
    Countertops = "countertops", Cabinets = "cabinets", Granite = "granite", Quartz = "quartz",
    Quartzite = "quartzite", Marble = "marble", Stone = "stone",
} }

/// In the CRM's order (quartzite before quartz).
fn family_patterns() -> [(ProductFamily, &'static regex::Regex); 7] {
    [
        (ProductFamily::Countertops, re!(r"\bcountertops?\b", "i")),
        (ProductFamily::Cabinets, re!(r"\bcabinets?\b", "i")),
        (ProductFamily::Quartzite, re!(r"\bquartzite\b", "i")),
        (ProductFamily::Quartz, re!(r"\bquartz\b", "i")),
        (ProductFamily::Granite, re!(r"\bgranite\b", "i")),
        (ProductFamily::Marble, re!(r"\bmarble\b", "i")),
        (ProductFamily::Stone, re!(r"\bstones?\b", "i")),
    ]
}

const VENDOR_TOKEN: &str = "__VENDOR_GRANITE_DEPOT__";

fn without_vendor_name(text: &str) -> String {
    re!(r"\bgranite depot\b", "gi")
        .replace_all(text, VENDOR_TOKEN)
        .into_owned()
}

fn restore_vendor_name(text: &str) -> String {
    text.replace(VENDOR_TOKEN, "Granite Depot")
}

fn customer_originated_product_family_text(
    pressure: &FollowupContactPressure,
    timeline: &[FollowupTimelineMessage],
) -> String {
    let text = |value: &Option<String>| value.clone().unwrap_or_default();
    let mut parts: Vec<String> = vec![
        text(&pressure.last_customer_originated_information),
        text(&pressure.last_conversation_reply),
        text(&pressure.last_customer_message_summary),
        text(&pressure.explicit_customer_request),
    ];
    parts.extend(pressure.customer_project_components.iter().cloned());
    if is_customer_evidence(pressure.timeline_evidence_source) {
        parts.push(text(&pressure.original_timeline_evidence));
    }
    if is_customer_evidence(pressure.construction_delay_source) {
        parts.push(text(&pressure.construction_dependency_evidence));
    }
    if is_customer_evidence(pressure.project_scope_source) {
        parts.push(text(&pressure.known_scope));
    }
    if matches!(
        pressure.hold_reason_source,
        Some(
            HoldReasonSource::CustomerSms
                | HoldReasonSource::CustomerEmail
                | HoldReasonSource::LeadForm
        )
    ) {
        parts.push(text(&pressure.hold_reason_evidence));
    }
    if is_customer_evidence(pressure.material_selection_evidence_source) {
        parts.push(text(&pressure.preferred_material));
        parts.push(text(&pressure.favorite_material));
        parts.extend(pressure.material_preferences.iter().cloned());
        parts.extend(pressure.material_type_preferences.iter().cloned());
    }
    for message in timeline {
        if message.direction == Direction::Inbound && !trim(&message.text).is_empty() {
            parts.push(message.text.clone());
        }
    }
    parts.join("\n")
}

/// The product families the customer raised themselves.
pub fn customer_approved_product_families(
    pressure: &FollowupContactPressure,
    timeline: &[FollowupTimelineMessage],
) -> Vec<ProductFamily> {
    let evidence =
        without_vendor_name(&customer_originated_product_family_text(pressure, timeline));
    family_patterns()
        .into_iter()
        .filter(|(_, re)| re.is_match(&evidence))
        .map(|(family, _)| family)
        .collect()
}

/// Product families a text names that the customer never raised.
pub fn unsupported_product_families_in_text(
    text: &str,
    allowed: &[ProductFamily],
) -> Vec<ProductFamily> {
    let probe = without_vendor_name(text);
    family_patterns()
        .into_iter()
        .filter(|(family, re)| !allowed.contains(family) && re.is_match(&probe))
        .map(|(family, _)| family)
        .collect()
}

fn has_unsupported_family(text: &str, allowed: &[ProductFamily]) -> bool {
    !unsupported_product_families_in_text(text, allowed).is_empty()
}

fn collapse_whitespace(text: &str) -> String {
    re!(r"\s+", "g").replace_all(text, " ").into_owned()
}

fn redact_unsupported_product_families(text: &str, allowed: &[ProductFamily]) -> String {
    let mut out = without_vendor_name(text);
    for (family, re) in family_patterns() {
        if allowed.contains(&family) {
            continue;
        }
        let replacement = match family {
            ProductFamily::Countertops | ProductFamily::Cabinets => "project",
            _ => "material",
        };
        out = re.replace_all(&out, replacement).into_owned();
    }
    trim(&collapse_whitespace(&restore_vendor_name(&out))).to_string()
}

/// How sure we are of a fact, from where it came from.
pub const fn confidence_from_evidence(
    source: EvidenceSource,
    evidence_confidence: Option<EvidenceConfidence>,
) -> FactConfidence {
    if matches!(source, EvidenceSource::None) {
        return FactConfidence::Unknown;
    }
    if is_customer_evidence(source) {
        return FactConfidence::CustomerStated;
    }
    if is_crm_evidence(source) {
        return FactConfidence::RepOrCrm;
    }
    if matches!(source, EvidenceSource::Conversation) {
        return if matches!(evidence_confidence, Some(EvidenceConfidence::Low)) {
            FactConfidence::Inferred
        } else {
            FactConfidence::CustomerStated
        };
    }
    FactConfidence::Inferred
}

fn iso_date(at: Instant) -> String {
    iso(at)[..10].to_string()
}

fn trim_end(text: &str) -> &str {
    text.trim_end_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
}

fn truncate_text(text: &str, max: usize) -> String {
    let normalized = collapse_whitespace(trim(text));
    if js_len(&normalized) <= max {
        return normalized;
    }
    format!("{}…", trim_end(&js_slice(&normalized, 0, max - 1)))
}

fn turn_from_message(message: &FollowupTimelineMessage) -> ConversationTurn {
    ConversationTurn {
        at: message.at.clone(),
        channel: message.channel,
        speaker: if message.direction == Direction::Inbound {
            Speaker::Customer
        } else {
            Speaker::Rep
        },
        text: truncate_text(&message.text, MESSAGE_CHAR_LIMIT),
    }
}

fn latest_turn(
    timeline: &[FollowupTimelineMessage],
    direction: Direction,
) -> Option<ConversationTurn> {
    timeline
        .iter()
        .rev()
        .find(|m| m.direction == direction && !trim(&m.text).is_empty())
        .map(turn_from_message)
}

fn recent_relevant_messages(timeline: &[FollowupTimelineMessage]) -> Vec<ConversationTurn> {
    let mut kept: Vec<&FollowupTimelineMessage> = timeline
        .iter()
        .rev()
        .filter(|m| !trim(&m.text).is_empty())
        .take(RECENT_MESSAGE_LIMIT)
        .collect();
    kept.reverse();
    kept.into_iter().map(turn_from_message).collect()
}

fn classify_lead_kind(pressure: &FollowupContactPressure) -> LeadKind {
    if pressure.communication_history_status == CommunicationHistoryStatus::TrulyNew
        || (!pressure.has_relationship_history && pressure.days_since_last_outbound.is_none())
    {
        return LeadKind::FreshUnanswered;
    }
    if matches!(
        pressure.communication_history_status,
        CommunicationHistoryStatus::Dormant
            | CommunicationHistoryStatus::ExistingCustomerNoDigitalHistory
    ) || pressure
        .days_since_last_outbound
        .is_some_and(|days| days >= 14.0)
        || pressure.hold_reason_type.is_some()
        || pressure.crm_hold_until.is_some()
    {
        return LeadKind::Reactivation;
    }
    LeadKind::ActiveThread
}

fn unresolved_customer_request(pressure: &FollowupContactPressure) -> Option<String> {
    if let Some(explicit) = pressure
        .explicit_customer_request
        .as_deref()
        .map(trim)
        .filter(|s| !s.is_empty())
    {
        return Some(explicit.to_string());
    }
    if pressure.unfinished_possibly_satisfied
        || pressure.unfinished_next_step_kind == Some(UnfinishedNextStepKind::SalespersonTask)
    {
        return None;
    }
    let unfinished = pressure
        .unfinished_next_step
        .as_deref()
        .map(trim)
        .filter(|s| !s.is_empty())?;
    matches!(
        pressure.unfinished_next_step_kind,
        Some(
            UnfinishedNextStepKind::CustomerRequest
                | UnfinishedNextStepKind::InHomeEstimate
                | UnfinishedNextStepKind::ScheduleMeasurement
                | UnfinishedNextStepKind::CustomerMeasurements
                | UnfinishedNextStepKind::ShowroomAppointment
        )
    )
    .then(|| unfinished.to_string())
}

fn active_hold(pressure: &FollowupContactPressure) -> Option<HoldContext> {
    let until = pressure
        .hard_contact_hold_until
        .or(pressure.crm_hold_until)?;
    Some(HoldContext {
        until: iso(until)[..10].to_string(),
        reason: pressure
            .hold_reason_evidence
            .clone()
            .or_else(|| pressure.customer_resume_condition.clone()),
    })
}

fn quote_snapshot(pressure: &FollowupContactPressure) -> Option<QuoteSnapshot> {
    (pressure.quote_requested
        || pressure.quote_sent
        || pressure.quote_received_or_discussed
        || pressure.quote_created)
        .then_some(QuoteSnapshot {
            requested: pressure.quote_requested,
            sent: pressure.quote_sent,
            received_or_discussed: pressure.quote_received_or_discussed,
        })
}

fn style_has_learned_prefs(style: Option<&FollowupStyleProfile>) -> bool {
    style.is_some_and(|s| {
        s.sms_length.is_some()
            || s.email_length.is_some()
            || s.question_count.is_some()
            || s.exclamation.is_some()
            || s.customer_name.is_some()
            || s.greeting.is_some()
            || s.link.is_some()
            || s.notes.as_ref().is_some_and(|notes| !notes.is_empty())
    })
}

/// "You (the salesperson sending this …): Tania Smith" → "Tania".
fn salesperson_first_name_from_profile(profile: &str) -> Option<String> {
    re!(r"^You \([^)]*\):\s*(\S+)", "m")
        .captures(profile)
        .and_then(|c| c.get(1))
        .map(|m| trim(m.as_str()).to_string())
        .filter(|s| !s.is_empty())
}

fn voice_samples_for_channel(
    templates: &[FollowupTemplateExample],
    channel: FollowupChannel,
    style: Option<&FollowupStyleProfile>,
) -> Vec<VoiceSample> {
    if style_has_learned_prefs(style) {
        return Vec::new();
    }
    // Promotional templates go stale; they are not voice samples.
    let offer = re!(
        r"\d+\s?%|\bdiscount|\bsale\b|\bspecials?\b|\bpromo|\boffer(ing)?\b",
        "i"
    );
    templates
        .iter()
        .filter(|t| {
            t.channel == Some(channel)
                && t.scope == Some(TemplateScope::Personal)
                && !offer.is_match(&t.body)
        })
        .map(|t| truncate_text(&t.body, VOICE_SAMPLE_CHAR_LIMIT))
        .filter(|preview| !preview.is_empty())
        .take(VOICE_SAMPLE_LIMIT)
        .map(|body_preview| VoiceSample {
            channel,
            body_preview,
        })
        .collect()
}

fn push_fact(
    facts: &mut Vec<RelevantFact>,
    key: &str,
    value: Option<&str>,
    confidence: FactConfidence,
) {
    let Some(value) = value.map(trim).filter(|v| !v.is_empty()) else {
        return;
    };
    if facts.iter().any(|f| f.key == key && f.value == value) {
        return;
    }
    facts.push(RelevantFact {
        key: key.into(),
        value: value.into(),
        confidence,
    });
}

fn push_flag(facts: &mut Vec<RelevantFact>, key: &str, value: bool, confidence: FactConfidence) {
    push_fact(
        facts,
        key,
        Some(if value { "yes" } else { "no" }),
        confidence,
    );
}

fn material_confidence(pressure: &FollowupContactPressure) -> FactConfidence {
    if pressure.material_direction_status == MaterialDirectionStatus::Confirmed
        && pressure.current_confirmed_material.is_some()
    {
        return FactConfidence::Confirmed;
    }
    if pressure.material_selection_evidence_source == EvidenceSource::Conversation {
        return FactConfidence::Inferred;
    }
    confidence_from_evidence(pressure.material_selection_evidence_source, None)
}

/// Unknown scope: drop facts naming a product family the customer never raised.
fn scope_supported_facts(
    facts: Vec<RelevantFact>,
    allowed: Option<&[ProductFamily]>,
) -> Vec<RelevantFact> {
    let Some(allowed) = allowed else {
        return facts;
    };
    let supported: Vec<RelevantFact> = facts
        .into_iter()
        .filter(|f| !has_unsupported_family(&f.value, allowed))
        .collect();
    if allowed.contains(&ProductFamily::Countertops) || allowed.contains(&ProductFamily::Stone) {
        return supported;
    }
    supported
        .into_iter()
        .filter(|f| f.key != "measurementsStatus")
        .collect()
}

fn sanitize_turn(
    turn: Option<ConversationTurn>,
    allowed: &[ProductFamily],
) -> Option<ConversationTurn> {
    let turn = turn?;
    if !has_unsupported_family(&turn.text, allowed) {
        return Some(turn);
    }
    let redacted = redact_unsupported_product_families(&turn.text, allowed);
    (!redacted.is_empty()).then_some(ConversationTurn {
        text: redacted,
        ..turn
    })
}

fn sanitize_text(text: Option<String>, allowed: &[ProductFamily]) -> Option<String> {
    let text = text?;
    if !has_unsupported_family(&text, allowed) {
        return Some(text);
    }
    Some(redact_unsupported_product_families(&text, allowed)).filter(|r| !r.is_empty())
}

fn sanitize_unknown_scope_shared(shared: &mut SharedModelContext, allowed: &[ProductFamily]) {
    let conversation = &mut shared.conversation;
    conversation.unresolved_request =
        sanitize_text(conversation.unresolved_request.take(), allowed);
    if let Some(hold) = conversation.hold.as_mut() {
        hold.reason = sanitize_text(hold.reason.take(), allowed);
    }
    conversation.latest_customer_message =
        sanitize_turn(conversation.latest_customer_message.take(), allowed);
    conversation.latest_rep_message =
        sanitize_turn(conversation.latest_rep_message.take(), allowed);
    conversation.recent_relevant = std::mem::take(&mut conversation.recent_relevant)
        .into_iter()
        .filter_map(|t| sanitize_turn(Some(t), allowed))
        .collect();
    shared
        .voice_samples
        .retain(|s| !has_unsupported_family(&s.body_preview, allowed));
    shared.crm_notes = std::mem::take(&mut shared.crm_notes)
        .into_iter()
        .filter_map(|note| {
            sanitize_text(Some(note.text.clone()), allowed).map(|text| CrmNote { text, ..note })
        })
        .collect();
}

fn add_quote_facts(facts: &mut Vec<RelevantFact>, p: &FollowupContactPressure) {
    let c = confidence_from_evidence(p.quote_evidence_source, Some(p.quote_evidence_confidence));
    push_flag(facts, "quoteRequested", p.quote_requested, c);
    push_flag(facts, "quoteSent", p.quote_sent, c);
    push_flag(
        facts,
        "quoteReceivedOrDiscussed",
        p.quote_received_or_discussed,
        c,
    );
    if p.quote_created && !p.quote_sent {
        push_flag(facts, "quoteCreated", true, c);
    }
    push_fact(
        facts,
        "quoteType",
        p.quote_type.map(super::facts::types::QuoteType::as_str),
        c,
    );
    push_fact(
        facts,
        "quoteExpirationDate",
        p.quote_expiration_date.map(iso_date).as_deref(),
        c,
    );
    push_fact(
        facts,
        "quoteExpirationEvidence",
        p.quote_expiration_evidence.as_deref(),
        c,
    );
}

fn add_material_facts(facts: &mut Vec<RelevantFact>, p: &FollowupContactPressure) {
    if p.deal_product_scope == DealProductScope::Unknown {
        return;
    }
    let stated = material_confidence(p);
    let confirmed = p.current_confirmed_material.as_deref();
    if confirmed.is_some() {
        push_fact(
            facts,
            "confirmedMaterial",
            confirmed,
            FactConfidence::Confirmed,
        );
    }
    let favorite = p.favorite_material.as_deref();
    if favorite.is_some() && favorite != confirmed {
        push_fact(facts, "favoriteMaterial", favorite, stated);
    }
    let preferred = p.preferred_material.as_deref();
    if preferred.is_some() && preferred != confirmed && preferred != favorite {
        let c = if p.material_direction_status == MaterialDirectionStatus::Confirmed {
            FactConfidence::Confirmed
        } else {
            stated
        };
        push_fact(facts, "preferredMaterial", preferred, c);
    }
    let known = p.known_material.as_deref();
    if known.is_some() && known != confirmed && known != favorite {
        push_fact(facts, "knownMaterial", known, stated);
    }
    push_fact(
        facts,
        "quotedMaterialContext",
        p.quoted_material_context.as_deref(),
        stated,
    );
    for name in &p.material_type_preferences {
        push_fact(facts, "materialTypePreference", Some(name), stated);
    }
    if p.deal_product_scope != DealProductScope::Cabinets {
        for name in &p.material_preferences {
            if Some(name.as_str()) == confirmed {
                continue;
            }
            push_fact(facts, "materialPreference", Some(name), stated);
        }
    }
    if p.uncertain_material_direction {
        push_flag(
            facts,
            "uncertainMaterialDirection",
            true,
            FactConfidence::Inferred,
        );
    }
    if p.did_not_find_suitable_option {
        push_flag(facts, "didNotFindSuitableOption", true, stated);
    }
}

fn add_color_design_facts(facts: &mut Vec<RelevantFact>, p: &FollowupContactPressure) {
    if p.deal_product_scope == DealProductScope::Unknown {
        return;
    }
    let stated = material_confidence(p);
    push_fact(
        facts,
        "visualStyle",
        p.visual_style.map(super::facts::types::VisualStyle::as_str),
        stated,
    );
    for color in &p.countertop_color_preferences {
        push_fact(facts, "colorPreference", Some(color), stated);
    }
    for edge in &p.edge_preferences {
        push_fact(facts, "edgePreference", Some(edge), stated);
    }
}

fn add_budget_facts(facts: &mut Vec<RelevantFact>, p: &FollowupContactPressure) {
    use super::facts::{DecisionObstacle, PriceContext};
    let c = confidence_from_evidence(p.budget_evidence_source, Some(p.budget_evidence_confidence));
    if p.budget_constraint {
        push_flag(facts, "budgetConstraint", true, c);
    }
    if let Some(target) = p.budget_target_approx {
        push_fact(facts, "budgetTargetApprox", Some(&js_number(target)), c);
    }
    push_fact(
        facts,
        "budgetObstacleEvidence",
        p.budget_obstacle_evidence.as_deref(),
        c,
    );
    if p.comparing_quotes {
        push_flag(facts, "comparingQuotes", true, c);
    }
    if p.price_context != PriceContext::None {
        push_fact(facts, "priceContext", Some(p.price_context.as_str()), c);
    }
    if !matches!(
        p.decision_obstacle,
        DecisionObstacle::None | DecisionObstacle::Timing
    ) {
        push_fact(
            facts,
            "decisionObstacle",
            Some(p.decision_obstacle.as_str()),
            c,
        );
    }
}

fn add_timing_facts(facts: &mut Vec<RelevantFact>, p: &FollowupContactPressure) {
    let c = confidence_from_evidence(
        p.timeline_evidence_source,
        Some(p.timeline_evidence_confidence),
    );
    push_fact(
        facts,
        "originalTimelineEvidence",
        p.original_timeline_evidence.as_deref(),
        c,
    );
    push_fact(
        facts,
        "constructionDependency",
        p.construction_dependency_evidence.as_deref(),
        confidence_from_evidence(
            p.construction_delay_source,
            Some(p.construction_delay_confidence),
        ),
    );
    push_fact(
        facts,
        "estimatedReadyOn",
        p.estimated_ready_at.map(iso_date).as_deref(),
        c,
    );
    push_fact(
        facts,
        "estimatedReadyPrecision",
        p.estimated_ready_at_precision
            .map(super::facts::types::EstimatedReadyPrecision::as_str),
        c,
    );
    push_fact(
        facts,
        "timelinePosition",
        p.timeline_position
            .map(super::facts::types::TimelinePosition::as_str),
        c,
    );
    push_fact(
        facts,
        "customerResumeCondition",
        p.customer_resume_condition.as_deref(),
        c,
    );
    for fact in &p.active_unresolved_facts {
        push_fact(
            facts,
            "unresolvedProjectFact",
            Some(fact),
            FactConfidence::CustomerStated,
        );
    }
}

fn add_scope_facts(facts: &mut Vec<RelevantFact>, p: &FollowupContactPressure) {
    let c = confidence_from_evidence(p.project_scope_source, Some(p.project_scope_confidence));
    push_fact(facts, "knownScope", p.known_scope.as_deref(), c);
    for component in &p.customer_project_components {
        push_fact(
            facts,
            "customerProjectComponent",
            Some(component),
            FactConfidence::CustomerStated,
        );
    }
    for component in &p.unsupported_requested_components {
        push_fact(
            facts,
            "unsupportedComponent",
            Some(component),
            FactConfidence::CustomerStated,
        );
    }
    if p.deal_product_scope == DealProductScope::Cabinets {
        for pref in &p.cabinet_preferences {
            push_fact(facts, "cabinetPreference", Some(pref), c);
        }
    }
    for area in &p.project_areas {
        push_fact(
            facts,
            "projectArea",
            Some(&format!(
                "{}:{}",
                area.area.as_str(),
                area.readiness.as_str()
            )),
            c,
        );
    }
}

fn add_next_step_facts(facts: &mut Vec<RelevantFact>, p: &FollowupContactPressure) {
    use super::facts::{PreferredNextStep, PreferredNextStepSource as S};
    if p.preferred_next_step != PreferredNextStep::None && p.preferred_next_step_source != S::None {
        let source = match p.preferred_next_step_source {
            S::SalespersonNote => EvidenceSource::SalespersonNote,
            S::CustomerSms => EvidenceSource::CustomerSms,
            S::CustomerEmail => EvidenceSource::CustomerEmail,
            S::None => EvidenceSource::None,
        };
        push_fact(
            facts,
            "preferredNextStep",
            Some(p.preferred_next_step.as_str()),
            confidence_from_evidence(source, Some(p.preferred_next_step_confidence)),
        );
    }
    push_fact(
        facts,
        "measurementsStatus",
        p.countertop_measurements_status
            .map(super::facts::types::MeasurementsStatus::as_str),
        FactConfidence::CustomerStated,
    );
    if p.showroom_interest {
        push_flag(
            facts,
            "showroomInterest",
            true,
            FactConfidence::CustomerStated,
        );
    }
    push_fact(
        facts,
        "requestedContactMethod",
        p.customer_requested_contact_method
            .map(super::facts::types::ContactMethod::as_str),
        FactConfidence::CustomerStated,
    );
}

fn writable_steps(steps: &[PlannedAiCadenceStep]) -> impl Iterator<Item = &PlannedAiCadenceStep> {
    steps
        .iter()
        .filter(|s| s.manual_action != Some("PHONE_CALL"))
}

const CUSTOMER_FORM_LABELS: [&str; 9] = [
    "Project type",
    "Project size",
    "When they want to start",
    "Sink",
    "Backsplash",
    "Stove",
    "Remove and dispose",
    "Lead details",
    "Customer message",
];
const OTHER_PROFILE_LABELS: [&str; 12] = [
    "Name",
    "Company",
    "Phone",
    "Phone 2",
    "Address",
    "Postal code",
    "Source",
    "City",
    "State",
    "Notes",
    "Deal",
    "Activity",
];
const CRM_REP_NOTE_LIMIT: usize = 4;
const CRM_NOTE_CHAR_LIMIT: usize = 300;

/// Profile lines are `Label: value`; a value may continue on the next lines.
fn profile_blocks(profile: &str) -> Vec<(String, String)> {
    let mut blocks: Vec<(String, String)> = Vec::new();
    for line in profile.split('\n') {
        let colon = line.find(": ").filter(|i| *i > 0);
        let label = colon.map_or("", |i| &line[..i]);
        let known = CUSTOMER_FORM_LABELS.contains(&label)
            || OTHER_PROFILE_LABELS.contains(&label)
            || re!(r"^(You \(|Deal note \(|Customer page URL)", "").is_match(label);
        if known {
            blocks.push((
                label.to_string(),
                line[colon.unwrap_or(0) + 2..].to_string(),
            ));
        } else if let Some(last) = blocks.last_mut() {
            last.1.push('\n');
            last.1.push_str(line);
        }
    }
    blocks
}

fn clean_crm_text(text: &str) -> String {
    let no_email = re!(r"[\w.+-]+@[\w-]+\.[\w.]+", "g").replace_all(text, "");
    let no_phone =
        re!(r"(\+?1[\s.-]?)?\(?\d{3}\)?[\s.-]?\d{3}[\s.-]?\d{4}\b", "g").replace_all(&no_email, "");
    let cleaned = trim(&collapse_whitespace(&no_phone)).to_string();
    if js_len(&cleaned) > CRM_NOTE_CHAR_LIMIT {
        format!("{}…", js_slice(&cleaned, 0, CRM_NOTE_CHAR_LIMIT - 1))
    } else {
        cleaned
    }
}

/// The customer's lead-form answers and the newest rep notes. With several
/// open projects only notes on the target deal's stage are kept.
pub fn crm_notes_from_profile(
    profile: &str,
    multi_project_customer: bool,
    target_deal_status: Option<&str>,
) -> Vec<CrmNote> {
    let mut notes = Vec::new();
    let mut rep_notes = 0;
    for (label, value) in profile_blocks(profile) {
        if CUSTOMER_FORM_LABELS.contains(&label.as_str()) {
            let text = clean_crm_text(&value);
            let free_text = label == "Customer message" || label == "Lead details";
            if !text.is_empty() {
                notes.push(CrmNote {
                    source: CrmNoteSource::CustomerForm,
                    text: if free_text {
                        text
                    } else {
                        format!("{label}: {text}")
                    },
                });
            }
            continue;
        }
        let is_activity = label == "Activity";
        if !is_activity && label != "Notes" && !label.starts_with("Deal note (") {
            continue;
        }
        if rep_notes >= CRM_REP_NOTE_LIMIT {
            continue;
        }
        let mut body = value.as_str();
        if is_activity {
            let mut parts = value.split(" · ");
            let name = parts.next().unwrap_or("");
            let stage = parts
                .find(|part| part.starts_with("stage "))
                .map(|part| &part["stage ".len()..]);
            if multi_project_customer && stage != target_deal_status {
                continue;
            }
            body = name;
        } else if multi_project_customer && label != "Notes" {
            continue;
        }
        let text = clean_crm_text(body);
        if js_len(&text) < 12 {
            continue;
        }
        notes.push(CrmNote {
            source: CrmNoteSource::RepNote,
            text,
        });
        rep_notes += 1;
    }
    notes
}

pub fn build_shared_model_context(source: &ModelContextSource<'_>) -> SharedModelContext {
    let p = source.contact_pressure;
    SharedModelContext {
        customer: CustomerContext {
            first_name: first_name_from_customer_profile(source.profile),
            salesperson_first_name: salesperson_first_name_from_profile(source.profile),
        },
        project: ProjectContext {
            scope: p.deal_product_scope,
            scope_confidence: confidence_from_evidence(
                p.project_scope_source,
                Some(p.project_scope_confidence),
            ),
            known_scope_note: if p.deal_product_scope == DealProductScope::Unknown {
                None
            } else {
                p.known_scope
                    .as_deref()
                    .map(trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
            },
            quote: quote_snapshot(p),
            other_open_projects: p.multi_project_customer,
        },
        conversation: ConversationContext {
            lead_kind: classify_lead_kind(p),
            current_state: p.latest_customer_state.as_str().to_string(),
            latest_customer_intent: p
                .latest_meaningful_customer_intent
                .as_ref()
                .map(|intent| format!("{} ({})", intent.kind.as_str(), intent.confidence.as_str())),
            unresolved_request: unresolved_customer_request(p),
            days_since_last_outbound: p.days_since_last_outbound,
            days_since_last_customer_reply: p.days_since_last_customer_reply,
            unanswered_outbound_count: p.unanswered_total_since_last_reply,
            sms_sent_before: source
                .timeline
                .iter()
                .any(|m| m.direction == Direction::Outbound && m.channel == FollowupChannel::Sms),
            hold: active_hold(p),
            latest_customer_message: latest_turn(source.timeline, Direction::Inbound),
            latest_rep_message: latest_turn(source.timeline, Direction::Outbound),
            recent_relevant: recent_relevant_messages(source.timeline),
        },
        crm_notes: crm_notes_from_profile(
            source.profile,
            p.multi_project_customer,
            p.target_deal_status.as_deref(),
        ),
        style: if style_has_learned_prefs(source.style_profile) {
            source.style_profile.cloned()
        } else {
            None
        },
        voice_samples: Vec::new(),
        admin_instruction: source
            .admin_instruction
            .map(trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        current_offer: source
            .current_offer
            .map(trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
    }
}

/// Objectives in the order their facts matter most for a reply.
const KNOWN_FACT_OBJECTIVES: [CopySemanticObjective; 9] = [
    CopySemanticObjective::NextStep,
    CopySemanticObjective::Quote,
    CopySemanticObjective::Timing,
    CopySemanticObjective::MaterialType,
    CopySemanticObjective::Color,
    CopySemanticObjective::Design,
    CopySemanticObjective::Scope,
    CopySemanticObjective::Budget,
    CopySemanticObjective::Nurture,
];

fn relevant_facts_for_objective(
    p: &FollowupContactPressure,
    objective: CopySemanticObjective,
) -> Vec<RelevantFact> {
    use CopySemanticObjective as O;
    let mut facts = Vec::new();
    if objective == O::Quote {
        add_quote_facts(&mut facts, p);
    }
    if matches!(objective, O::MaterialType | O::Color) {
        add_material_facts(&mut facts, p);
    }
    if matches!(objective, O::Color | O::Design) {
        add_color_design_facts(&mut facts, p);
    }
    if objective == O::Design && p.deal_product_scope != DealProductScope::Unknown {
        add_material_facts(&mut facts, p);
    }
    if objective == O::Budget {
        add_budget_facts(&mut facts, p);
    }
    if objective == O::Timing {
        add_timing_facts(&mut facts, p);
    }
    if objective == O::Scope {
        add_scope_facts(&mut facts, p);
    }
    if objective == O::NextStep {
        add_next_step_facts(&mut facts, p);
    }
    if matches!(objective, O::Nurture | O::CloseLoop) {
        push_fact(
            &mut facts,
            "holdReason",
            p.hold_reason_evidence.as_deref(),
            FactConfidence::CustomerStated,
        );
    }
    facts
}

/// The facts a planned step's objective draws on.
pub fn step_relevant_facts(
    p: &FollowupContactPressure,
    step: &PlannedAiCadenceStep,
) -> Vec<RelevantFact> {
    relevant_facts_for_objective(p, planner_copy_objective(step))
}

/// Every fact the model may use, for any message.
pub fn known_facts(p: &FollowupContactPressure) -> Vec<RelevantFact> {
    let mut facts = Vec::new();
    for objective in KNOWN_FACT_OBJECTIVES {
        for fact in relevant_facts_for_objective(p, objective) {
            push_fact(&mut facts, &fact.key, Some(&fact.value), fact.confidence);
        }
    }
    facts
}

pub fn customer_page_url_from_profile(profile: &str) -> Option<String> {
    re!(r#"https?:\/\/\S*\/customer\/\d+\/stones[^\s)"]*"#, "i")
        .find(profile)
        .map(|m| m.as_str().to_string())
}

/// The customer page link, once, when not sent yet and the scope allows it.
pub fn customer_page_url_fact(
    source: &ModelContextSource<'_>,
    already_assigned: bool,
) -> Option<RelevantFact> {
    let p = source.contact_pressure;
    if already_assigned || p.deal_product_scope == DealProductScope::Cabinets {
        return None;
    }
    if p.deal_product_scope == DealProductScope::Unknown {
        let allowed = customer_approved_product_families(p, source.timeline);
        if !allowed.contains(&ProductFamily::Countertops)
            && !allowed.contains(&ProductFamily::Stone)
        {
            return None;
        }
    }
    if stones_link_already_sent(source.timeline, source.profile) {
        return None;
    }
    let url = customer_page_url_from_profile(source.profile)?;
    Some(RelevantFact {
        key: "customerPageUrl".into(),
        value: url,
        confidence: FactConfidence::RepOrCrm,
    })
}

/// The planner's send day for a step.
fn planned_send_on(step: &PlannedAiCadenceStep, now: Instant) -> Option<String> {
    if let Some(day) = &step.expected_send_on {
        return Some(day.clone());
    }
    if !step.offset_days.is_finite() {
        return None;
    }
    #[allow(clippy::cast_possible_truncation)]
    let millis = (step.offset_days * 24.0 * 60.0 * 60.0 * 1000.0).trunc() as i64;
    Some(iso(now + chrono::Duration::milliseconds(millis))[..10].to_string())
}

pub fn build_cadence_model_context(
    source: &ModelContextSource<'_>,
    planned_steps: &[PlannedAiCadenceStep],
    now: Instant,
) -> CadenceModelContext {
    let mut shared = build_shared_model_context(source);
    let p = source.contact_pressure;
    let allowed = (p.deal_product_scope == DealProductScope::Unknown)
        .then(|| customer_approved_product_families(p, source.timeline));
    let mut assigned_url = false;
    let steps = writable_steps(planned_steps)
        .enumerate()
        .map(|(index, step)| {
            let mut relevant_facts = step_relevant_facts(p, step);
            if let Some(fact) = customer_page_url_fact(source, assigned_url) {
                relevant_facts.push(fact);
                assigned_url = true;
            }
            StepModelContext {
                task: TaskContext {
                    step_index: index + 1,
                    channel: step.channel,
                    objective: "MODEL_CHOOSES".into(),
                    approach: "YOU choose the approach and wording for this message".into(),
                    task_description: "Write the best follow-up for this channel to get a reply. No prescribed message type.".into(),
                    expected_send_on: planned_send_on(step, now),
                },
                relevant_facts: scope_supported_facts(relevant_facts, allowed.as_deref()),
            }
        })
        .collect();
    let first_writable = writable_steps(planned_steps).next();
    if let Some(first) = first_writable {
        shared.voice_samples =
            voice_samples_for_channel(source.templates, first.channel, source.style_profile);
    }
    if let Some(allowed) = &allowed {
        sanitize_unknown_scope_shared(&mut shared, allowed);
    }
    let mut facts = known_facts(p);
    if first_writable.is_some() {
        if let Some(fact) = customer_page_url_fact(source, false) {
            facts.push(fact);
        }
    }
    CadenceModelContext {
        shared,
        known_facts: scope_supported_facts(facts, allowed.as_deref()),
        steps,
    }
}

fn format_fact(fact: &RelevantFact) -> String {
    format!(
        "- {}: {} [{}]",
        fact.key,
        fact.value,
        fact.confidence.as_str()
    )
}

fn format_turn(turn: &ConversationTurn) -> String {
    let who = if turn.speaker == Speaker::Customer {
        "Customer"
    } else {
        "Rep"
    };
    format!(
        "[{}] {} {who}: {}",
        turn.at,
        turn.channel.as_str().to_uppercase(),
        turn.text
    )
}

fn format_style(style: &FollowupStyleProfile) -> Vec<String> {
    let mut lines = Vec::new();
    let mut add = |name: &str, value: Option<&str>| {
        if let Some(value) = value {
            lines.push(format!("{name}: {value}"));
        }
    };
    add(
        "smsLength",
        style
            .sms_length
            .map(super::style::FollowupStyleLength::as_str),
    );
    add(
        "emailLength",
        style
            .email_length
            .map(super::style::FollowupStyleLength::as_str),
    );
    add(
        "questionCount",
        style
            .question_count
            .map(super::style::FollowupQuestionPreference::as_str),
    );
    add(
        "exclamation",
        style
            .exclamation
            .map(super::style::ExclamationPreference::as_str),
    );
    add(
        "customerName",
        style
            .customer_name
            .map(super::style::FollowupTogglePreference::as_str),
    );
    add(
        "greeting",
        style
            .greeting
            .map(super::style::FollowupTogglePreference::as_str),
    );
    add(
        "link",
        style
            .link
            .map(super::style::FollowupTogglePreference::as_str),
    );
    lines
}

fn days(value: Option<f64>) -> String {
    value.map_or_else(|| "none".to_string(), js_number)
}

fn format_steps(s: &mut Vec<String>, context: &CadenceModelContext) {
    let shared = &context.shared;
    let timing = admin_instruction_gap_minutes(shared.admin_instruction.as_deref().unwrap_or("")).map_or_else(
        || "timing: delayMinutes is minutes after the previous message".to_string(),
        |gap| {
            #[allow(clippy::cast_precision_loss)]
            let day_gap = js_number(gap as f64 / (24.0 * 60.0));
            format!(
                "timing: the user instruction has higher priority. delayMinutes must be {gap} ({day_gap} day gap), including Sunday. Do not choose a longer wait."
            )
        },
    );
    for step in &context.steps {
        s.extend([
            String::new(),
            format!("STEP {}", step.task.step_index),
            "TASK".into(),
            format!("channel: {}", step.task.channel.as_str()),
            format!("objective: {}", step.task.objective),
            format!("approach: {}", step.task.approach),
            format!("task: {}", step.task.task_description),
        ]);
        if let Some(day) = &step.task.expected_send_on {
            s.push(format!("planned send: around {day}"));
        }
        s.push(timing.clone());
    }
}

fn format_situation(s: &mut Vec<String>, c: &ConversationContext) {
    s.extend([
        String::new(),
        "CURRENT SITUATION".into(),
        format!("leadKind: {}", c.lead_kind.as_str()),
        format!("currentState: {}", c.current_state),
    ]);
    if let Some(intent) = &c.latest_customer_intent {
        s.push(format!("latestCustomerIntent: {intent}"));
    }
    if let Some(request) = &c.unresolved_request {
        s.push(format!("unresolvedRequest: {request}"));
    }
    s.push(format!(
        "daysSinceLastOutbound: {}",
        days(c.days_since_last_outbound)
    ));
    s.push(format!(
        "daysSinceLastCustomerReply: {}",
        days(c.days_since_last_customer_reply)
    ));
    s.push(format!(
        "unansweredOutboundCount: {}",
        js_number(c.unanswered_outbound_count)
    ));
    if !c.sms_sent_before {
        s.push("smsSentBefore: no".into());
    }
    if let Some(hold) = &c.hold {
        s.push(format!("holdUntil: {}", hold.until));
        if let Some(reason) = &hold.reason {
            s.push(format!("holdReason: {reason}"));
        }
    }
    s.extend([String::new(), "RECENT CONVERSATION".into()]);
    if c.recent_relevant.is_empty() {
        s.push("(no prior SMS or email)".into());
    } else {
        s.extend(c.recent_relevant.iter().map(format_turn));
    }
}

/// The user prompt of a `NEW_CONTEXT` cadence.
pub fn format_cadence_model_context(context: &CadenceModelContext) -> String {
    let shared = &context.shared;
    let yes_no = |value: bool| if value { "yes" } else { "no" };
    let mut s: Vec<String> = vec![
        "CUSTOMER".into(),
        format!(
            "firstName: {}",
            shared.customer.first_name.as_deref().unwrap_or("unknown")
        ),
    ];
    if let Some(name) = &shared.customer.salesperson_first_name {
        s.push(format!("you (their salesperson): {name}"));
    }
    s.extend([
        String::new(),
        "PROJECT".into(),
        format!(
            "scope: {} [{}]",
            shared.project.scope.as_str(),
            shared.project.scope_confidence.as_str()
        ),
    ]);
    if let Some(note) = &shared.project.known_scope_note {
        s.push(format!("knownScope: {note}"));
    }
    if let Some(quote) = &shared.project.quote {
        s.push(format!("quoteRequested: {}", yes_no(quote.requested)));
        s.push(format!("quoteSent: {}", yes_no(quote.sent)));
        s.push(format!(
            "quoteReceivedOrDiscussed: {}",
            yes_no(quote.received_or_discussed)
        ));
    }
    if shared.project.other_open_projects {
        s.push("otherOpenProjects: yes — do not mix facts from other projects".into());
    }
    format_situation(&mut s, &shared.conversation);
    s.extend([
        String::new(),
        "WHAT WE KNOW (any message may use these)".into(),
    ]);
    if context.known_facts.is_empty() {
        s.push("(nothing beyond CUSTOMER / PROJECT / CURRENT SITUATION)".into());
    } else {
        s.extend(context.known_facts.iter().map(format_fact));
    }
    if !shared.crm_notes.is_empty() {
        s.extend([
            String::new(),
            "FROM THE CRM (use the facts; never quote the notes)".into(),
        ]);
        s.extend(
            shared
                .crm_notes
                .iter()
                .map(|note| format!("- {}: {}", note.source.as_str(), note.text)),
        );
    }
    if let Some(style) = &shared.style {
        let lines = format_style(style);
        if !lines.is_empty() {
            s.extend([String::new(), "STYLE".into()]);
            s.extend(lines);
        }
    }
    if let Some(instruction) = shared
        .admin_instruction
        .as_deref()
        .map(trim)
        .filter(|i| !i.is_empty())
    {
        s.extend([
            String::new(),
            "ADMINISTRATIVE INSTRUCTION".into(),
            "This user instruction has higher priority than the planned timing.".into(),
            instruction.to_string(),
        ]);
    }
    if let Some(offer) = &shared.current_offer {
        s.extend([String::new(), "CURRENT OFFER".into(), offer.clone()]);
    }
    if !shared.voice_samples.is_empty() {
        s.extend([String::new(), "VOICE SAMPLES".into()]);
        s.extend(
            shared
                .voice_samples
                .iter()
                .map(|v| format!("- {}: {}", v.channel.as_str(), v.body_preview)),
        );
    }
    format_steps(&mut s, context);
    s.join("\n")
}

/// How many facts the context carries (for the pipeline metrics).
pub fn count_model_facts(context: &CadenceModelContext) -> usize {
    let mut count = 4;
    if context.shared.project.quote.is_some() {
        count += 3;
    }
    count += usize::from(context.shared.conversation.unresolved_request.is_some());
    count += usize::from(context.shared.conversation.hold.is_some());
    count += usize::from(context.shared.conversation.latest_customer_intent.is_some());
    count + context.known_facts.len() + context.shared.crm_notes.len()
}
