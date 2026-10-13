//! Deterministic repairs of contact-pressure contradictions found by
//! [`validate_followup_pressure`](super::validate::validate_followup_pressure)
//! (`applyValidationRepair`, `prepareFollowupValidation`).

use super::super::contact_resume::AutomationContactStatus;
use super::super::facts::scope::extract_customer_project_scope;
use super::super::facts::{
    ContactMethod, DealProductScope, EvidenceConfidence, EvidenceSource,
    ExplicitCustomerRequestKind, LatestCustomerState, ManualActionRequired, ManualActionStatus,
    MaterialDirectionStatus,
};
use super::super::lifecycle::FollowupSequenceStatus;
use super::types::{FollowupContactPressure, InformationDensity};
use super::validate::has_direct_scope_conversation_evidence;
use crate::{re, str_enum};
use serde::Serialize;

str_enum! { pub enum ValidationStatus { Valid = "VALID", Repaired = "REPAIRED", HardBlocked = "HARD_BLOCKED" } }

/// One field the repair changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ValidationRepair {
    pub field: String,
    pub from: String,
    pub to: String,
    pub reason: String,
}

/// The pressure after repairs, and what could not be repaired.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FollowupValidationResult {
    pub pressure: FollowupContactPressure,
    pub validation_status: ValidationStatus,
    pub validation_repairs: Vec<ValidationRepair>,
    pub hard_block_errors: Vec<String>,
    pub remaining_errors: Vec<String>,
}

const REPAIRABLE: [&str; 46] = [
    "low/none scope source must not assert a specific product scope",
    "5+ concrete project details must not stay information_density LOW",
    "conversation says quote was sent but quote_sent is false",
    "cabinet-only evidence must not expand deal_product_scope to both",
    "direct cabinets-and-countertops statement must not become CABINETS-only scope",
    "multiple material preferences must not look confirmed or singular",
    "countertop removal requires component provenance",
    "material type plus color must not become MULTIPLE_OPEN_OPTIONS",
    "estimate request must set quote_requested",
    "preferred SMS/email channels must be captured",
    "measurement conflict must not keep trusted island or unknown sqft",
    "manual action pending but newer evidence satisfies the action",
    "conflicting dimensions and area must set measurement_conflict",
    "pending salesperson deliverable must set manual_action_required",
    "material_preferences contains pronoun entity",
    "quote_received_or_discussed true but quote_sent false",
    "named stone and parent material type classified as competing materials",
    "CRM got-quote note but quote_sent is false",
    "readiness statement must populate estimated_ready_at",
    "package interest evidence must set known_package_interest",
    "customer directly asks project cost/estimate but quote_requested is false",
    "open actionable salesperson CALL task must set manual_action_required",
    "known full-remodel/package project must not collapse to COUNTERTOPS-only scope",
    "target-specific CRM remodel note must populate customer_project_components",
    "partial quote review must not leave latest_customer_state UNKNOWN",
    "pricing request needing dimensions must populate active_unresolved_facts",
    "explicit customer dimensions must populate measurement fields",
    "direct customer dimensions exist but typed measurement fields are empty",
    "superseded manual action must not remain preferred_next_step PHONE_CALL",
    "pending manual action must pause automated nurture",
    "completed or superseded manual action must not remain unfinished_next_step",
    // "target deal state contains project-specific evidence from another property" is never repaired.
    "terminal customer state must populate latest_terminal_status_at",
    "estimated_ready_at_source customer_sms has no supporting customer SMS",
    "money value must not become project size",
    "existing granite removal must not become a target granite preference",
    "signed contract/payment progression must set quote_received_or_discussed",
    "company scheduling obligation must set pending manual action",
    "ordinary-language fragment in material_preferences",
    "derived island area does not reconcile with stored dimensions",
    "11ft by 5ft 4in cannot become a tiny island_sqft",
    "kitchen and vanity square footage are reversed or misattributed",
    "explicit total and component fields do not reconcile",
    "template approved but old quote task is still treated as current",
    "semantic evidence shows several projects but multi_project_customer is false",
    "truly_new known-scope lead must not start in REACTIVATION",
    "",
];

fn is_repairable_validation_error(error: &str) -> bool {
    if error.contains("target deal state contains project-specific evidence from another property")
    {
        // Checked in order in the CRM: only the entries before it can match first.
        return REPAIRABLE[..31].iter().any(|item| error.contains(item));
    }
    REPAIRABLE
        .iter()
        .filter(|item| !item.is_empty())
        .any(|item| error.contains(item))
}

fn repair(
    field: &str,
    from: impl Into<String>,
    to: impl Into<String>,
    reason: &str,
) -> ValidationRepair {
    ValidationRepair {
        field: field.into(),
        from: from.into(),
        to: to.into(),
        reason: reason.into(),
    }
}

fn pause_sequence(p: &mut FollowupContactPressure) {
    p.automation_contact_status = AutomationContactStatus::Paused;
    if p.followup_sequence_status == FollowupSequenceStatus::Active {
        p.followup_sequence_status = FollowupSequenceStatus::Paused;
    }
}

/// The repair for one validation error, if it has one.
#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
fn apply_validation_repair(
    pressure: &FollowupContactPressure,
    error: &str,
) -> Option<(FollowupContactPressure, ValidationRepair)> {
    let mut p = pressure.clone();
    let has = |needle: &str| error.contains(needle);
    if has("low/none scope source must not assert a specific product scope") {
        if has_direct_scope_conversation_evidence(pressure)
            && pressure.deal_product_scope != DealProductScope::Unknown
        {
            return None;
        }
        p.deal_product_scope = DealProductScope::Unknown;
        p.project_scope_source = EvidenceSource::None;
        p.project_scope_confidence = EvidenceConfidence::Low;
        return Some((
            p,
            repair(
                "deal_product_scope",
                pressure.deal_product_scope.as_str(),
                "UNKNOWN",
                "No sufficiently strong target-specific scope evidence",
            ),
        ));
    }
    if has("5+ concrete project details must not stay information_density LOW") {
        p.information_density = InformationDensity::Medium;
        return Some((
            p,
            repair(
                "information_density",
                "LOW",
                "MEDIUM",
                "Rich project record contradicted LOW information density",
            ),
        ));
    }
    if has("conversation says quote was sent but quote_sent is false") {
        p.quote_sent = true;
        p.quote_created = true;
        p.quote_evidence = true;
        if p.quote_evidence_source == EvidenceSource::None {
            p.quote_evidence_source = EvidenceSource::Conversation;
        }
        p.quote_evidence_confidence = EvidenceConfidence::High;
        return Some((
            p,
            repair(
                "quote_sent",
                "false",
                "true",
                "Salesperson-originated conversation confirms quote was sent",
            ),
        ));
    }
    if has("cabinet-only evidence must not expand deal_product_scope to both") {
        p.deal_product_scope = DealProductScope::Cabinets;
        return Some((
            p,
            repair(
                "deal_product_scope",
                pressure.deal_product_scope.as_str(),
                "CABINETS",
                "Cabinet-only evidence cannot justify combined scope",
            ),
        ));
    }
    if has("direct cabinets-and-countertops statement must not become CABINETS-only scope") {
        p.deal_product_scope = DealProductScope::CabinetsAndCountertops;
        return Some((
            p,
            repair(
                "deal_product_scope",
                pressure.deal_product_scope.as_str(),
                "CABINETS_AND_COUNTERTOPS",
                "Direct customer statement includes both cabinets and countertops",
            ),
        ));
    }
    if has("multiple material preferences must not look confirmed or singular") {
        p.uncertain_material_direction = true;
        p.material_direction_status = MaterialDirectionStatus::MultipleOpenOptions;
        return Some((
            p,
            repair(
                "material_direction_status",
                pressure.material_direction_status.as_str(),
                "MULTIPLE_OPEN_OPTIONS",
                "Customer is choosing between competing material options",
            ),
        ));
    }
    if has("countertop removal requires component provenance") {
        p.existing_countertop_removal = false;
        return Some((
            p,
            repair(
                "existing_countertop_removal",
                "true",
                "false",
                "Removal requires explicit component evidence, not generic dispose flag",
            ),
        ));
    }
    if has("estimate request must set quote_requested") {
        p.quote_requested = true;
        p.explicit_customer_request = pressure
            .explicit_customer_request
            .clone()
            .or_else(|| pressure.last_customer_originated_information.clone())
            .or_else(|| Some("estimate request".into()));
        p.explicit_customer_request_kind = pressure
            .explicit_customer_request_kind
            .or(Some(ExplicitCustomerRequestKind::EstimateRequest));
        return Some((
            p,
            repair(
                "quote_requested",
                "false",
                "true",
                "Customer explicitly requested an estimate",
            ),
        ));
    }
    if has("preferred SMS/email channels must be captured") {
        p.customer_requested_contact_methods = vec![ContactMethod::Sms, ContactMethod::Email];
        p.customer_requested_contact_method = Some(ContactMethod::Sms);
        return Some((
            p,
            repair(
                "customer_requested_contact_methods",
                "none",
                "SMS,EMAIL",
                "Customer named text and email as preferred contact channels",
            ),
        ));
    }
    if has("measurement conflict must not keep trusted island or unknown sqft") {
        p.island_sqft = None;
        p.unknown_sqft = None;
        return Some((
            p,
            repair(
                "island_sqft",
                "set",
                "none",
                "Conflicting measurements cannot be treated as trusted area",
            ),
        ));
    }
    if has("manual action pending but newer evidence satisfies the action") {
        let to = if pressure.quote_sent {
            ManualActionStatus::Completed
        } else {
            ManualActionStatus::Superseded
        };
        p.manual_action_status = to;
        return Some((
            p,
            repair(
                "manual_action_status",
                "PENDING",
                to.as_str(),
                "Later timeline evidence satisfies or supersedes the stale manual action",
            ),
        ));
    }
    if has("conflicting dimensions and area must set measurement_conflict") {
        p.measurement_conflict = true;
        p.island_sqft = None;
        p.unknown_sqft = None;
        return Some((
            p,
            repair(
                "measurement_conflict",
                "false",
                "true",
                "Reported dimensions and area materially disagree",
            ),
        ));
    }
    if has("pending salesperson deliverable must set manual_action_required") {
        p.manual_action_required = Some(ManualActionRequired::SalesTask);
        p.manual_action_status = ManualActionStatus::Pending;
        p.manual_action_description = pressure
            .unfinished_next_step
            .clone()
            .or_else(|| pressure.manual_action_description.clone())
            .or_else(|| Some("pending salesperson deliverable".into()));
        if p.manual_action_tasks.is_empty() {
            p.manual_action_tasks = vec!["SALES_TASK".into()];
        }
        pause_sequence(&mut p);
        return Some((
            p,
            repair(
                "manual_action_required",
                "none",
                "SALES_TASK",
                "Pending salesperson deliverable requires manual action gating",
            ),
        ));
    }
    let pronoun = re!(r"^(it|this|that|them|one)$", "i");
    if has("material_preferences contains pronoun entity") {
        p.material_preferences
            .retain(|name| !pronoun.is_match(name));
        if p.known_material
            .as_deref()
            .is_some_and(|m| pronoun.is_match(m))
        {
            p.known_material = None;
        }
        return Some((
            p,
            repair(
                "material_preferences",
                "pronoun",
                "removed",
                "Pronoun references cannot be stored as material entities",
            ),
        ));
    }
    if has("quote_received_or_discussed true but quote_sent false")
        || has("CRM got-quote note but quote_sent is false")
    {
        p.quote_sent = true;
        p.quote_created = true;
        p.quote_evidence = true;
        if p.quote_evidence_source == EvidenceSource::None {
            p.quote_evidence_source = EvidenceSource::SalespersonNote;
        }
        p.quote_evidence_confidence = EvidenceConfidence::High;
        return Some((
            p,
            repair(
                "quote_sent",
                "false",
                "true",
                "Quote receipt implies quote was delivered",
            ),
        ));
    }
    if has("named stone and parent material type classified as competing materials") {
        p.material_preferences.retain(|name| {
            !re!(r"^(granite|quartz|marble|quartzite|dekton|soapstone)$", "i").is_match(name)
        });
        let count = p.material_preferences.len();
        if count <= 1 {
            p.uncertain_material_direction = false;
            p.material_direction_status = if count == 1 {
                MaterialDirectionStatus::SingleOpenOption
            } else {
                MaterialDirectionStatus::None
            };
        }
        return Some((
            p,
            repair(
                "material_direction_status",
                "MULTIPLE_OPEN_OPTIONS",
                "SINGLE_OPEN_OPTION",
                "Named stone belongs under its parent material type",
            ),
        ));
    }
    if has("customer directly asks project cost/estimate but quote_requested is false") {
        p.quote_requested = true;
        return Some((
            p,
            repair(
                "quote_requested",
                "false",
                "true",
                "Direct pricing or estimate request implies quote_requested",
            ),
        ));
    }
    if has("open actionable salesperson CALL task must set manual_action_required") {
        p.manual_action_required = Some(ManualActionRequired::PhoneCall);
        p.manual_action_status = ManualActionStatus::Pending;
        p.manual_action_description = pressure
            .manual_action_description
            .clone()
            .or_else(|| pressure.unfinished_next_step.clone())
            .or_else(|| Some("pending salesperson phone call".into()));
        pause_sequence(&mut p);
        return Some((
            p,
            repair(
                "manual_action_required",
                "none",
                "PHONE_CALL",
                "Open salesperson call task requires manual action gating",
            ),
        ));
    }
    if has("known full-remodel/package project must not collapse to COUNTERTOPS-only scope") {
        p.deal_product_scope = DealProductScope::FullRemodel;
        return Some((
            p,
            repair(
                "deal_product_scope",
                "COUNTERTOPS",
                "FULL_REMODEL",
                "Kitchen remodel package scope must not collapse to countertops only",
            ),
        ));
    }
    if has("target-specific CRM remodel note must populate customer_project_components") {
        let mut corpus = vec![
            pressure
                .last_customer_originated_information
                .clone()
                .unwrap_or_default(),
            pressure.hold_reason_evidence.clone().unwrap_or_default(),
            pressure.known_scope.clone().unwrap_or_default(),
        ];
        corpus.extend(pressure.project_details.iter().cloned());
        let extracted = extract_customer_project_scope(&corpus.join("\n"), "");
        p.customer_project_components = extracted.customer_project_components;
        let to = p.customer_project_components.join(", ");
        return Some((
            p,
            repair(
                "customer_project_components",
                "none",
                if to.is_empty() {
                    "populated".to_string()
                } else {
                    to
                },
                "CRM remodel evidence must populate explicit project components",
            ),
        ));
    }
    if has("partial quote review must not leave latest_customer_state UNKNOWN") {
        p.latest_customer_state = LatestCustomerState::Evaluating;
        return Some((
            p,
            repair(
                "latest_customer_state",
                "UNKNOWN",
                "EVALUATING",
                "Partial quote review is meaningful evaluating state",
            ),
        ));
    }
    if has("pricing request needing dimensions must populate active_unresolved_facts") {
        let additions = [
            "dimensions needed to determine remaining square footage",
            "project measurements needed for pricing",
        ];
        for fact in additions {
            if !p
                .active_unresolved_facts
                .iter()
                .any(|item| item.to_lowercase() == fact.to_lowercase())
            {
                p.active_unresolved_facts.push(fact.into());
            }
        }
        return Some((
            p,
            repair(
                "active_unresolved_facts",
                "none",
                additions.join("; "),
                "Pricing request without measurements needs unresolved dimension facts",
            ),
        ));
    }
    if has("superseded manual action must not remain preferred_next_step PHONE_CALL") {
        p.preferred_next_step = super::super::facts::PreferredNextStep::None;
        p.preferred_next_step_source = super::super::facts::PreferredNextStepSource::None;
        p.preferred_next_step_confidence = EvidenceConfidence::Low;
        return Some((
            p,
            repair(
                "preferred_next_step",
                "PHONE_CALL",
                "NONE",
                "Superseded manual action is not the current preferred next step",
            ),
        ));
    }
    if has("pending manual action must pause automated nurture") {
        pause_sequence(&mut p);
        return Some((
            p,
            repair(
                "automation_contact_status",
                "ACTIVE",
                "PAUSED",
                "Pending manual action must pause automated nurture",
            ),
        ));
    }
    if has("terminal customer state must populate latest_terminal_status_at") {
        p.latest_terminal_status_at = pressure.last_customer_message_at;
        let to = pressure
            .last_customer_message_at
            .map_or_else(|| "none".to_string(), super::super::dates::iso);
        return Some((
            p,
            repair(
                "latest_terminal_status_at",
                "none",
                to,
                "Terminal customer evidence must carry a terminal timestamp",
            ),
        ));
    }
    if has("completed or superseded manual action must not remain unfinished_next_step") {
        p.unfinished_next_step = None;
        p.unfinished_next_step_kind = None;
        p.historical_task = true;
        return Some((
            p,
            repair(
                "unfinished_next_step",
                pressure
                    .unfinished_next_step
                    .clone()
                    .unwrap_or_else(|| "none".into()),
                "none",
                "Completed or superseded manual actions must not remain the current unfinished step",
            ),
        ));
    }
    if has("estimated_ready_at_source customer_sms has no supporting customer SMS") {
        if super::validate::customer_sms_supports_ready_timing(pressure) {
            return None;
        }
        let to = if pressure.estimated_ready_at.is_some() {
            EvidenceSource::CustomerMessage
        } else {
            EvidenceSource::None
        };
        p.estimated_ready_at_source = to;
        return Some((
            p,
            repair(
                "estimated_ready_at_source",
                pressure.estimated_ready_at_source.as_str(),
                to.as_str(),
                "Ready-at provenance repaired from recoverable customer evidence",
            ),
        ));
    }
    if has("money value must not become project size") {
        p.project_size_raw = None;
        let from = pressure
            .project_size_raw
            .map_or_else(|| "none".to_string(), super::super::js::js_number);
        return Some((
            p,
            repair(
                "project_size_raw",
                from,
                "none",
                "Money/discount amounts must not populate project size",
            ),
        ));
    }
    if has("existing granite removal must not become a target granite preference") {
        let granite = re!(r"^granite$", "i");
        let kept: Vec<String> = pressure
            .material_type_preferences
            .iter()
            .filter(|item| !granite.is_match(item))
            .cloned()
            .collect();
        p.material_type_preferences.clone_from(&kept);
        p.existing_countertop_material = pressure
            .existing_countertop_material
            .clone()
            .or_else(|| Some("Granite".into()));
        p.existing_countertop_removal = true;
        return Some((
            p,
            repair(
                "material_type_preferences",
                pressure.material_type_preferences.join(", "),
                kept.join(", "),
                "Existing granite being removed is not a target material preference",
            ),
        ));
    }
    if has("signed contract/payment progression must set quote_received_or_discussed") {
        p.quote_received_or_discussed = true;
        p.quote_sent = true;
        p.quote_created = true;
        p.quote_evidence = true;
        return Some((
            p,
            repair(
                "quote_received_or_discussed",
                "false",
                "true",
                "Signed contract/payment implies the quote was received",
            ),
        ));
    }
    if has("company scheduling obligation must set pending manual action") {
        p.manual_action_required = Some(ManualActionRequired::SalesTask);
        p.manual_action_status = ManualActionStatus::Pending;
        if p.automation_contact_status != AutomationContactStatus::Stopped {
            p.automation_contact_status = AutomationContactStatus::Paused;
        }
        if p.followup_sequence_status != FollowupSequenceStatus::StoppedTerminalDeal {
            p.followup_sequence_status = FollowupSequenceStatus::Paused;
        }
        let from = pressure
            .manual_action_required
            .map_or("none", ManualActionRequired::as_str);
        return Some((
            p,
            repair(
                "manual_action_required",
                from,
                "SALES_TASK",
                "Company-owed scheduling contact is a pending sales task",
            ),
        ));
    }
    if has("ordinary-language fragment in material_preferences") {
        let valid = |name: &String| super::super::facts::materials::is_valid_material_entity(name);
        let kept: Vec<String> = pressure
            .material_preferences
            .iter()
            .filter(|n| valid(n))
            .cloned()
            .collect();
        p.material_preferences.clone_from(&kept);
        p.historical_material_preferences = pressure
            .historical_material_preferences
            .iter()
            .filter(|n| valid(n))
            .cloned()
            .collect();
        p.favorite_material = pressure
            .favorite_material
            .clone()
            .filter(|m| !m.is_empty() && valid(m));
        p.current_confirmed_material = pressure
            .current_confirmed_material
            .clone()
            .filter(|m| !m.is_empty() && valid(m));
        p.known_material = pressure
            .known_material
            .clone()
            .filter(|m| !m.is_empty() && valid(m));
        let to = kept.join(", ");
        return Some((
            p,
            repair(
                "material_preferences",
                pressure.material_preferences.join(", "),
                if to.is_empty() {
                    "none".to_string()
                } else {
                    to
                },
                "Drop ordinary-language fragments that are not material names",
            ),
        ));
    }
    if has("derived island area does not reconcile with stored dimensions")
        || has("11ft by 5ft 4in cannot become a tiny island_sqft")
    {
        let calculated = pressure
            .calculated_area_from_dimensions_sqft
            .filter(|area| *area > 10.0);
        p.island_sqft = calculated.map(|value| super::super::facts::ExtractedMeasurement {
            value,
            measurement_type: super::super::facts::MeasurementComponent::Island,
            source: EvidenceSource::CustomerMessage,
            source_text: pressure
                .measurement_raw_text
                .clone()
                .unwrap_or_else(|| "dimensions".into()),
            confidence: EvidenceConfidence::Medium,
        });
        p.measurement_conflict = calculated.is_none();
        let from = pressure.island_sqft.as_ref().map_or_else(
            || "none".to_string(),
            |m| super::super::js::js_number(m.value),
        );
        let to = calculated.map_or_else(|| "none".to_string(), super::super::js::js_number);
        return Some((
            p,
            repair(
                "island_sqft",
                from,
                to,
                "Store derived island area from normalized dimensions or leave unknown",
            ),
        ));
    }
    if has("kitchen and vanity square footage are reversed or misattributed")
        || has("explicit total and component fields do not reconcile")
    {
        let origin = format!(
            "{} {}",
            pressure
                .last_customer_originated_information
                .as_deref()
                .unwrap_or(""),
            pressure.last_conversation_reply.as_deref().unwrap_or("")
        );
        let number = |pattern: &regex::Regex| {
            pattern
                .captures(&origin)
                .and_then(|caps| caps.get(1))
                .filter(|m| !m.as_str().is_empty())
                .and_then(|m| super::super::js::js_parse_number(m.as_str()))
        };
        let kitchen = number(re!(
            r"(\d+(?:\.\d+)?)\s*(?:sq\.?\s*ft|sqft|sf)\s+of\s+kitchen",
            "i"
        ))?;
        let vanity = number(re!(
            r"(\d+(?:\.\d+)?)\s*(?:sq\.?\s*ft|sqft|sf)\s+of\s+vanity",
            "i"
        ))?;
        let total = number(re!(r"\b(\d+(?:\.\d+)?)\s+total\b", "i"));
        let source_text =
            super::super::js::trim(&re!(r"\s+").replace_all(&origin, " ")).to_string();
        let measured = |value, kind| super::super::facts::ExtractedMeasurement {
            value,
            measurement_type: kind,
            source: EvidenceSource::CustomerMessage,
            source_text: source_text.clone(),
            confidence: EvidenceConfidence::High,
        };
        p.countertop_sqft = Some(measured(
            kitchen,
            super::super::facts::MeasurementComponent::Countertop,
        ));
        p.vanity_sqft = Some(measured(
            vanity,
            super::super::facts::MeasurementComponent::Vanity,
        ));
        if let Some(total) = total {
            p.total_project_sqft = Some(measured(
                total,
                super::super::facts::MeasurementComponent::TotalProject,
            ));
        }
        let from = pressure.vanity_sqft.as_ref().map_or_else(
            || "none".to_string(),
            |m| super::super::js::js_number(m.value),
        );
        return Some((
            p,
            repair(
                "vanity_sqft",
                from,
                super::super::js::js_number(vanity),
                "Attribute each square-footage number to its nearby component label",
            ),
        ));
    }
    if has("template approved but old quote task is still treated as current") {
        p.manual_action_status = ManualActionStatus::Superseded;
        p.unfinished_next_step = None;
        return Some((
            p,
            repair(
                "manual_action_status",
                pressure.manual_action_status.as_str(),
                "SUPERSEDED",
                "Template approval supersedes old quote or estimate tasks",
            ),
        ));
    }
    if has("semantic evidence shows several projects but multi_project_customer is false") {
        p.multi_project_customer = true;
        return Some((
            p,
            repair(
                "multi_project_customer",
                "false",
                "true",
                "Conversation references more than one project even with one CRM deal",
            ),
        ));
    }
    if has("truly_new known-scope lead must not start in REACTIVATION") {
        p.followup_starting_phase = super::types::FollowupPhase::FirstFollowup;
        return Some((
            p,
            repair(
                "followup_starting_phase",
                pressure.followup_starting_phase.as_str(),
                "FIRST_FOLLOWUP",
                "A new known-scope lead is initial outreach, not reactivation",
            ),
        ));
    }
    None
}

/// Validates the pressure, applies repairs (up to eight passes), and says
/// whether what remains blocks the follow-up.
pub fn prepare_followup_validation(pressure: &FollowupContactPressure) -> FollowupValidationResult {
    let mut current = pressure.clone();
    let mut repairs = Vec::new();
    for _ in 0..8 {
        let repairable: Vec<String> = super::validate::validate_followup_pressure(&current)
            .into_iter()
            .filter(|e| is_repairable_validation_error(e))
            .collect();
        if repairable.is_empty() {
            break;
        }
        let mut changed = false;
        for error in &repairable {
            if let Some((next, applied)) = apply_validation_repair(&current, error) {
                current = next;
                repairs.push(applied);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let remaining_errors = super::validate::validate_followup_pressure(&current);
    let hard_block_errors: Vec<String> = remaining_errors
        .iter()
        .filter(|e| !is_repairable_validation_error(e))
        .cloned()
        .collect();
    let validation_status = if !hard_block_errors.is_empty() {
        ValidationStatus::HardBlocked
    } else if !repairs.is_empty() {
        ValidationStatus::Repaired
    } else {
        ValidationStatus::Valid
    };
    FollowupValidationResult {
        pressure: current,
        validation_status,
        validation_repairs: repairs,
        hard_block_errors,
        remaining_errors,
    }
}
