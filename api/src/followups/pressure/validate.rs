//! Consistency checks on a built contact pressure (`validateFollowupPressure`):
//! each message names a contradiction the engine must not plan from.

use super::super::classify::InboundMessageType;
use super::super::contact_resume::{AutomationContactStatus, ContactResumeMode};
use super::super::facts::ManualActionStatus as MS;
use super::super::facts::{
    AppointmentCompanion, ContactMethod, DealProductScope, DecisionFactor, DecisionObstacle,
    EvidenceSource, LatestCustomerState, ManualActionRequired, PriceContext, QuoteType,
};
use super::super::intent::MeaningfulCustomerIntentType as I;
use super::super::lifecycle::FollowupSequenceStatus as Seq;
use super::types::{
    ChannelAttemptState, ContactPressureLevel, CustomerFollowupState, DealState,
    EmailChannelStatus, FollowupContactPressure, FollowupPhase, InformationDensity,
    PhoneChannelStatus, RelationshipState, SmsChannelStatus,
};
use crate::re;

fn filled(value: Option<&String>) -> bool {
    value.is_some_and(|v| !v.is_empty())
}

fn text(value: Option<&String>) -> &str {
    value.map_or("", String::as_str)
}

fn contains_ci(list: &[String], value: &str) -> bool {
    let value = value.to_lowercase();
    list.iter().any(|item| item.to_lowercase() == value)
}

/// Every contradiction in the pressure (empty when consistent).
#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
pub fn validate_followup_pressure(p: &FollowupContactPressure) -> Vec<String> {
    let mut errors: Vec<String> = Vec::new();
    let mut push = |message: &str| errors.push(message.to_string());
    let intent = p.latest_meaningful_customer_intent.as_ref().map(|i| i.kind);
    if p.unanswered_sms_since_last_reply > 0.0 && p.sms_channel_status == SmsChannelStatus::Untested
    {
        push("SMS outbound exists but sms_channel_status is untested");
    }
    if p.unanswered_email_since_last_reply > 0.0
        && p.email_channel_status == EmailChannelStatus::Untested
    {
        push("email outbound exists but email_channel_status is untested");
    }
    if p.customer_state == CustomerFollowupState::NewLeadNoResponse
        && matches!(
            p.relationship_state,
            RelationshipState::ExistingOldLead | RelationshipState::ExistingActive
        )
    {
        push("customer_state new_lead_no_response contradicts existing relationship");
    }
    if p.customer_state == CustomerFollowupState::UnreachedLead
        && p.relationship_state == RelationshipState::ExistingOldLead
    {
        push("customer_state unreached_lead contradicts existing_old_lead");
    }
    if p.followup_sequence_status == Seq::StoppedTerminalDeal
        && p.target_deal_id.is_some()
        && !matches!(p.deal_state, DealState::Won | DealState::Lost)
    {
        push("terminal sequence status on a non-terminal target deal");
    }
    if matches!(intent, Some(I::Waiting | I::Delayed | I::ComparingQuotes))
        && p.followup_starting_phase == FollowupPhase::ActivePursuit
        && p.global_contact_pressure == ContactPressureLevel::Elevated
    {
        push("low-pressure intent must not stay in ACTIVE_PURSUIT under elevated pressure");
    }
    if !matches!(
        p.quote_evidence_source,
        EvidenceSource::None | EvidenceSource::DealRecord
    ) && !p.quote_evidence
    {
        push("quote evidence source is set but quote_evidence is false");
    }
    let wrong_phone = p.sms_channel_status == SmsChannelStatus::WrongNumber
        || p.phone_channel_status == PhoneChannelStatus::WrongNumber;
    if wrong_phone && p.sms_available {
        push("wrong phone must not keep sms_available true");
    }
    if wrong_phone && p.phone_available {
        push("wrong phone must not keep phone_available true");
    }
    if p.quote_evidence && p.quote_evidence_source == EvidenceSource::None {
        push("quote_evidence is true but quote_evidence_source is none");
    }
    if !p.quote_evidence
        && filled(p.unfinished_next_step.as_ref())
        && re!(r"\bquote\b", "i").is_match(text(p.unfinished_next_step.as_ref()))
    {
        push("activity references quote but quote_evidence is false");
    }
    if p.quote_requested
        && p.quote_evidence
        && !p.quote_created
        && !p.quote_sent
        && !p.quote_received_or_discussed
    {
        push("quote request must not be treated as quote_evidence");
    }
    if p.phone_channel_status == PhoneChannelStatus::Untested
        && p.phone_channel_state == ChannelAttemptState::AttemptedNoResponse
    {
        push("CRM call activity must not leave phone_channel_status untested");
    }
    if re!(r"postponed|something came up|can'?t push", "i").is_match(&format!(
        "{} {}",
        text(p.last_conversation_reply.as_ref()),
        text(p.last_customer_message_summary.as_ref())
    )) && matches!(intent, None | Some(I::Unknown))
    {
        push("postponed customer message must not stay UNKNOWN intent");
    }
    if p.unanswered_total_since_last_reply >= 4.0
        && p.followup_starting_phase == FollowupPhase::ActivePursuit
    {
        push("repeated non-response must not stay ACTIVE_PURSUIT");
    }
    let inbound_at = p.latest_genuine_inbound_at.or(p.last_customer_message_at);
    let inbound_already_passed = inbound_at.is_some_and(|inbound| {
        p.last_conversational_rep_reply_at
            .is_some_and(|rep| inbound < rep)
            || p.last_outbound_at.is_some_and(|out| inbound < out)
    });
    let generated = p.sequence_generated_at.is_some();
    if p.latest_inbound_type == Some(InboundMessageType::CustomerReply)
        && !matches!(
            p.followup_sequence_status,
            Seq::StoppedCustomerReplied | Seq::StoppedOptOut | Seq::StoppedTerminalDeal
        )
        && !(generated && (p.customer_reply_handled || inbound_already_passed))
    {
        push("CUSTOMER_REPLY must stop the current sequence as STOPPED_CUSTOMER_REPLIED");
    }
    if p.customer_reply_handled
        && p.followup_sequence_status == Seq::StoppedCustomerReplied
        && generated
    {
        push("handled reply on a new sequence must not stay STOPPED_CUSTOMER_REPLIED");
    }
    if p.contact_resume_mode == ContactResumeMode::CustomerInitiates
        && p.hard_contact_hold_until.is_some()
        && p.automation_contact_status != AutomationContactStatus::WaitForCustomer
    {
        push("customer-initiates resume must not become a finite automatic resume date");
    }
    if p.sms_channel_status == SmsChannelStatus::CustomerRequestedNoText && p.sms_available {
        push("customer asked not to text but sms_available is true");
    }
    if p.quote_received_or_discussed && !p.quote_evidence {
        push("quote discussed must still set quote_evidence");
    }
    if filled(p.budget_obstacle_evidence.as_ref())
        && p.decision_obstacle_evidence.is_none()
        && !p.active_decision_factors.contains(&DecisionFactor::Budget)
    {
        push("explicit budget concern exists but decision factors omit BUDGET");
    }
    if p.project_details
        .iter()
        .any(|item| re!(r"no backsplash", "i").is_match(item))
        && !p.backsplash_known
    {
        push("project_details has no backsplash but backsplash_known is false");
    }
    if p.sms_channel_status == SmsChannelStatus::Untested && !p.never_tried_sms {
        push("sms_channel_status is untested but never_tried_sms is false");
    }
    if let Some(favorite) = p.favorite_material.as_deref().filter(|f| !f.is_empty())
        && !contains_ci(&p.material_preferences, favorite)
    {
        push("favorite_material is missing from material_preferences");
    }
    if p.budget_target_approx.is_some() && !p.budget_evidence {
        push("budget_target_approx is set without budget_evidence");
    }
    if p.budget_target_approx.is_some()
        && matches!(
            p.price_context,
            PriceContext::PackagePrice | PriceContext::PromotionPrice
        )
    {
        push("package or promotion price must not populate budget_target_approx");
    }
    let originated = text(p.last_customer_originated_information.as_ref());
    let details = p.project_details.join(" ");
    if re!(r"remove and dispose:\s*yes", "i").is_match(&format!("{originated} {}", text(p.hold_reason_evidence.as_ref())))
        && !re!(r"replace cabinet fronts|cabinet fronts, flooring|kitchen cabinetry", "i")
            .is_match(&format!("{originated} {}", text(p.known_scope.as_ref())))
        && p.existing_countertop_removal
        && !re!(r"existing countertop removal|existing granite removal|remove (existing )?(countertops?|granite)", "i")
            .is_match(&format!("{details} {originated}"))
    {
        push("generic remove and dispose must not set countertop_removal without component evidence");
    }
    if p.existing_countertop_removal
        && !re!(
            r"countertop removal|granite removal|remove (existing )?(countertops?|granite)",
            "i"
        )
        .is_match(&format!(
            "{details} {originated} {}",
            text(p.hold_reason_evidence.as_ref())
        ))
    {
        push("countertop removal requires component provenance");
    }
    if matches!(intent, Some(I::Waiting | I::Delayed))
        && p.latest_customer_state == LatestCustomerState::Unknown
    {
        push("strong intent must not leave latest_customer_state UNKNOWN");
    }
    if p.followup_sequence_status == Seq::StoppedCustomerReplied
        && p.automation_contact_status == AutomationContactStatus::Active
        && !p.customer_waiting_for_human_reply
        && generated
        && inbound_already_passed
    {
        push("historical inbound must not keep a fresh generation at STOPPED_CUSTOMER_REPLIED");
    }
    let scope = text(p.known_scope.as_ref());
    if p.quote_type == Some(QuoteType::Cabinet)
        && p.deal_product_scope == DealProductScope::CabinetsAndCountertops
        && !p.existing_countertop_removal
        && p.countertop_preferences.is_empty()
        && !re!(r"countertop|island", "i").is_match(&details)
        && !re!(r"countertop|island", "i").is_match(scope)
    {
        push("cabinet-only evidence must not expand deal_product_scope to both");
    }
    if p.appointment_companion == Some(AppointmentCompanion::Spouse)
        && p.active_decision_factors
            .contains(&DecisionFactor::SpouseOrPartnerDecision)
    {
        push("spouse attending an appointment is not SPOUSE_OR_PARTNER_DECISION");
    }
    if p.customer_requested_contact_method == Some(ContactMethod::Phone)
        && p.manual_action_required != Some(ManualActionRequired::PhoneCall)
    {
        push("customer-requested phone must set manual_action_required PHONE_CALL");
    }
    if p.uncertain_material_direction
        && (p.current_confirmed_material.is_some() || p.known_material.is_some())
    {
        push("undecided material must not set a singular confirmed/known material");
    }
    if p.uncertain_material_direction
        && p.material_type_preferences.len() > 1
        && p.material_preferences.len() == 1
    {
        let sole = p.material_preferences[0].to_lowercase();
        if p.material_type_preferences
            .iter()
            .any(|t| t.to_lowercase() == sole)
        {
            push("multiple supported materials must not collapse to one preference");
        }
    }
    if p.information_density == InformationDensity::Low {
        let signals = [
            filled(p.known_scope.as_ref()),
            p.sink_required || p.sink_source.is_some(),
            p.backsplash_known,
            p.existing_countertop_removal,
            !p.material_preferences.is_empty()
                || !p.material_type_preferences.is_empty()
                || p.uncertain_material_direction,
            p.quote_evidence,
            p.estimated_ready_at.is_some() || filled(p.original_timeline_evidence.as_ref()),
            filled(p.explicit_customer_request.as_ref()),
            p.decision_obstacle != DecisionObstacle::None,
            p.project_details.len() >= 2,
        ];
        if signals.iter().filter(|s| **s).count() >= 5 {
            push("5+ concrete project details must not stay information_density LOW");
        }
    }
    let summary = text(p.last_customer_message_summary.as_ref());
    let reply = text(p.last_conversation_reply.as_ref());
    let hold_ev = text(p.hold_reason_evidence.as_ref());
    let unfinished = text(p.unfinished_next_step.as_ref());
    let constr = text(p.construction_dependency_evidence.as_ref());
    let explicit = text(p.explicit_customer_request.as_ref());
    let cat = |parts: &[&str]| parts.join(" ");
    let has_will_recontact = intent == Some(I::CustomerWillRecontact)
        || p.additional_customer_intents
            .iter()
            .any(|i| i.kind == I::CustomerWillRecontact);
    if (p.material_preferences.len() > 1 || p.material_type_preferences.len() > 1)
        && p.current_confirmed_material.is_none()
        && !p.uncertain_material_direction
        && p.material_direction_status
            != super::super::facts::MaterialDirectionStatus::MultipleOpenOptions
    {
        push("multiple material preferences must not look confirmed or singular");
    }
    if re!(r"still in the planning process|still planning", "i").is_match(&cat(&[summary, reply]))
        && intent == Some(I::Unknown)
    {
        push("explicit planning language must not stay UNKNOWN intent");
    }
    if re!(r"late march|ready in march|started in late", "i")
        .is_match(&cat(&[summary, reply, hold_ev]))
        && p.estimated_ready_at.is_none()
    {
        push("explicit month timeline must populate estimated_ready_at");
    }
    if re!(r"white granite", "i").is_match(&cat(&[originated, hold_ev]))
        && p.countertop_color_preferences.is_empty()
    {
        push("white granite note must populate countertop_color_preferences");
    }
    if re!(r"didn'?t find anything", "i").is_match(&cat(&[originated, hold_ev]))
        && p.decision_obstacle == DecisionObstacle::None
    {
        push("did not find a suitable option must set a decision obstacle");
    }
    if (p.sms_channel_status == SmsChannelStatus::InvalidFormat
        || p.phone_channel_status == PhoneChannelStatus::InvalidFormat)
        && (p.sms_available || p.phone_available)
    {
        push("invalid phone format must not leave SMS or phone available");
    }
    if p.outbound_last30_days >= 20.0 && p.global_contact_pressure == ContactPressureLevel::Low {
        push("extreme outbound volume must not stay global_contact_pressure LOW");
    }
    if filled(p.last_customer_message_summary.as_ref())
        && p.latest_inbound_type == Some(InboundMessageType::InternalMessage)
        && p.operational_contact_state == super::types::OperationalContactState::Responsive
        && p.last_customer_message_at.is_none()
    {
        push("salesperson-authored inbound must not count as a customer reply");
    }
    if !p.matched_customer_identity
        && p.latest_inbound_type == Some(InboundMessageType::UnknownInbound)
        && p.operational_contact_state == super::types::OperationalContactState::Responsive
        && p.last_customer_message_at.is_none()
    {
        push("unmatched UNKNOWN_INBOUND must not set RESPONSIVE by itself");
    }
    if re!(
        r"highest of all my bids|highest bid|better price elsewhere",
        "i"
    )
    .is_match(&cat(&[summary, reply]))
        && !p.comparing_quotes
    {
        push("competitive bid language must set comparing_quotes");
    }
    if re!(
        r"highest of all my bids|numbers came in at the highest|your numbers are too high",
        "i"
    )
    .is_match(&cat(&[summary, reply]))
        && p.decision_obstacle == DecisionObstacle::None
    {
        push("highest-bid pricing must not leave decision_obstacle NONE");
    }
    if re!(
        r"highest of all my bids|quote is too expensive|can you change this price|i like the quote",
        "i"
    )
    .is_match(&cat(&[summary, reply]))
        && !p.quote_received_or_discussed
    {
        push("customer pricing evaluation must set quote_received_or_discussed");
    }
    if re!(r"just sent (all )?(the |your )?quotes?", "i").is_match(reply) && !p.quote_sent {
        push("rep said quotes were sent but quote_sent is false");
    }
    if re!(r"kitchen cabinetry", "i").is_match(scope)
        && p.deal_product_scope == DealProductScope::Countertops
        && !re!(r"\bcountertops?\b", "i").is_match(originated)
    {
        push("structured cabinetry must not become COUNTERTOPS without customer evidence");
    }
    if re!(r"days would be best|\d+\s*[-–]\s*\d+\s+days", "i").is_match(&cat(&[summary, reply]))
        && p.customer_requested_contact_window_start.is_none()
        && p.customer_requested_contact_window_end.is_none()
        && p.customer_requested_followup_at.is_none()
    {
        push("known customer contact window must populate a timing-window field");
    }
    if re!(r"white stone|light brown|\btan\b", "i").is_match(hold_ev)
        && p.countertop_color_preferences.is_empty()
    {
        push("CRM color notes must populate countertop_color_preferences");
    }
    if re!(r"i('ll| will) contact you when|will contact you when\s*ever|contact you whenever|whenever i plan", "i")
        .is_match(&cat(&[reply, summary, originated, hold_ev]))
        && p.contact_resume_mode != ContactResumeMode::CustomerInitiates
    {
        push("customer-controlled resume must set CUSTOMER_INITIATES");
    }
    if p.approx_unit_count.is_some()
        && !re!(
            r"\b(unit|units|apartment|apartments|appartment|appartments|kitchen|kitchens|bathroom|bathrooms|condo|condos|home|homes)\b",
            "i"
        )
        .is_match(&cat(&[scope, &details]))
    {
        push("approx_unit_count requires nearby project-unit language");
    }
    if re!(r"compare prices|wants to compare|shop(ping)? around", "i")
        .is_match(&cat(&[originated, hold_ev, reply, summary, unfinished]))
        && !p.comparing_quotes
    {
        push("compare-prices evidence must set comparing_quotes");
    }
    if super::super::intent::intent_stops_automated_pursuit(
        p.latest_meaningful_customer_intent.as_ref(),
    ) && p.customer_waiting_for_human_reply
    {
        push("terminal customer decline must not stay CUSTOMER_WAITING_FOR_HUMAN_REPLY");
    }
    if re!(r"to be honest.{0,24}price|it'?s the price", "i").is_match(&cat(&[hold_ev, reply]))
        && p.decision_obstacle == DecisionObstacle::None
    {
        push("explicit price obstacle must set decision_obstacle");
    }
    if re!(r"did look it over|look(ed)? (it )?over", "i").is_match(&cat(&[reply, hold_ev]))
        && !p.quote_received_or_discussed
    {
        push("customer quote review must set quote_received_or_discussed");
    }
    if p.quote_type == Some(QuoteType::Cabinet)
        && re!(r"cabinet quote|quote for cabinets", "i").is_match(&cat(&[reply, hold_ev]))
        && p.deal_product_scope == DealProductScope::Unknown
    {
        push("cabinet quote evidence must not repair scope to UNKNOWN");
    }
    if re!(r"full[- ]?height backsplash", "i").is_match(&details) && !p.backsplash_known {
        push("full-height backsplash must set backsplash_known");
    }
    if re!(r"in-?home est|schedule.{0,24}estimate", "i").is_match(unfinished)
        && p.unfinished_next_step_kind.is_none()
    {
        push("open in-home estimate activity must set unfinished_next_step_kind");
    }
    if re!(r"find ", "i").is_match(unfinished) && p.material_preferences.is_empty() {
        push("open activity named materials must populate material_preferences");
    }
    if matches!(
        intent,
        Some(I::NotNow | I::CustomerWillRecontact | I::Waiting)
    ) && p.latest_customer_state == LatestCustomerState::Unknown
    {
        push("strong intent must not leave latest_customer_state UNKNOWN");
    }
    if has_will_recontact && p.contact_resume_mode != ContactResumeMode::CustomerInitiates {
        push("CUSTOMER_WILL_RECONTACT must set CUSTOMER_INITIATES");
    }
    if has_will_recontact && p.followup_sequence_status == Seq::Active {
        push("CUSTOMER_WILL_RECONTACT must not leave followup_sequence_status ACTIVE");
    }
    if super::super::contact_resume::text_looks_like_customer_owns_next_contact(&cat(&[
        reply, summary,
    ])) && p.contact_resume_mode != ContactResumeMode::CustomerInitiates
    {
        push("customer owns next contact but contact_resume_mode is not CUSTOMER_INITIATES");
    }
    if p.quote_requested
        && super::super::facts::quotes::text_looks_like_competitor_quote_reference(&cat(&[
            explicit, reply, summary,
        ]))
    {
        push("competitor quote reference must not set quote_requested");
    }
    if p.followup_sequence_status == Seq::StoppedTerminalDeal
        && p.automation_contact_status != AutomationContactStatus::Stopped
    {
        push("terminal target deal must set automation_contact_status STOPPED");
    }
    if re!(
        r"wrong\s+(?:number\s+and\s+)?email|gave a wrong (?:number\s+and\s+)?email|invalid email|bad email|email (?:is )?(?:wrong|invalid)",
        "i"
    )
    .is_match(&cat(&[hold_ev, unfinished]))
        && matches!(p.email_channel_status, EmailChannelStatus::Untested | EmailChannelStatus::Valid)
    {
        push("CRM wrong email evidence must not leave email_channel_status usable");
    }
    if re!(r"countertops? waiting on|almost ready for counter|cabinets? (are |is )?(unlevel|not level|bot leveled|unleveled)", "i")
        .is_match(&cat(&[originated, hold_ev, constr]))
        && p.deal_product_scope == DealProductScope::Cabinets
    {
        push("countertops waiting on cabinets must not become CABINETS scope");
    }
    if re!(
        r"just sent you (the |your )?(updated )?quote|quote i sent",
        "i"
    )
    .is_match(&cat(&[reply, summary]))
        && !p.quote_sent
    {
        push("conversation says quote was sent but quote_sent is false");
    }
    if re!(r"put off the project", "i").is_match(&cat(&[originated, hold_ev, reply]))
        && p.latest_customer_state == LatestCustomerState::Unknown
    {
        push("project put off must not leave latest_customer_state UNKNOWN");
    }
    if p.project_scope_source == EvidenceSource::None
        && p.project_scope_confidence == super::super::facts::EvidenceConfidence::Low
        && p.deal_product_scope != DealProductScope::Unknown
        && !filled(p.known_scope.as_ref())
        && !(p.quote_type == Some(QuoteType::Cabinet)
            && p.deal_product_scope == DealProductScope::Cabinets)
        && !(p.quote_type == Some(QuoteType::Countertop) && p.quote_evidence)
        && !has_direct_scope_conversation_evidence(p)
    {
        push("low/none scope source must not assert a specific product scope");
    }
    if re!(
        r"i('ll| will) (contact|call|reach out|co\s*tact).{0,40}(once|after|when)",
        "i"
    )
    .is_match(&cat(&[reply, originated]))
        && p.contact_resume_mode != ContactResumeMode::CustomerInitiates
    {
        push("customer-controlled resume must set CUSTOMER_INITIATES");
    }
    if re!(r"\bcabinets?\b", "i").is_match(originated)
        && re!(r"\bcountertops?\b", "i").is_match(originated)
        && p.deal_product_scope == DealProductScope::Cabinets
    {
        push("direct cabinets-and-countertops statement must not become CABINETS-only scope");
    }
    if re!(r"\b(get|like to get|want).{0,24}\bestimate\b", "i")
        .is_match(&cat(&[originated, explicit]))
        && !p.quote_requested
    {
        push("estimate request must set quote_requested");
    }
    if re!(r"text or email|sms or email|email or text", "i").is_match(originated)
        && p.customer_requested_contact_methods.is_empty()
    {
        push("preferred SMS/email channels must be captured");
    }
    if p.measurement_conflict && (p.island_sqft.is_some() || p.unknown_sqft.is_some()) {
        push("measurement conflict must not keep trusted island or unknown sqft");
    }
    let customer_origin = cat(&[originated, hold_ev]);
    if super::super::js::fancy_test(
        crate::fre!(
            r"chang(?:ing|e)\s+(?:my\s+)?(?:current\s+)?\w+\s+(?:countertops?\s+)?to",
            "i"
        ),
        &customer_origin,
    ) && p.material_type_preferences.len() > 1
        && p.existing_countertop_material.is_some()
    {
        push("material replacement must not keep both existing and target as open preferences");
    }
    if re!(r"backsplash", "i").is_match(&customer_origin)
        && re!(r"not sure|unsure|might depend|thinking about", "i").is_match(&customer_origin)
        && !p.backsplash_known
    {
        push("explicit backsplash discussion must not leave backsplash_known false");
    }
    if re!(r"should send .{0,30}measur|measurments|measurements", "i").is_match(unfinished)
        && p.unfinished_next_step_kind.is_none()
    {
        push("pending measurements activity must populate unfinished_next_step");
    }
    if (re!(
        r"complete kitchen repair|complete kitchen remodel|including cabinets",
        "i"
    )
    .is_match(&customer_origin)
        || p.customer_project_components
            .iter()
            .any(|c| c == "kitchen remodel"))
        && p.deal_product_scope == DealProductScope::Unknown
    {
        push("direct remodel/cabinets language must not leave deal_product_scope UNKNOWN");
    }
    if re!(r"\bphone doesn'?t work\b|\bphone doesnt work\b", "i")
        .is_match(&cat(&[hold_ev, originated]))
        && matches!(
            p.phone_channel_status,
            PhoneChannelStatus::Untested | PhoneChannelStatus::Valid
        )
    {
        push("phone does not work note must not leave voice channel untested or valid");
    }
    if re!(r"\bbianco absoluto\b", "i").is_match(&customer_origin)
        && !p
            .material_preferences
            .iter()
            .any(|item| re!(r"bianco absoluto", "i").is_match(item))
    {
        push("named remnant request must populate material_preferences");
    }
    if re!(r"customer supplied|\(customer supplied\)", "i").is_match(&customer_origin)
        && re!(r"sink", "i").is_match(&customer_origin)
        && !p.sink_required
    {
        push("customer-supplied sink must set sink_required");
    }
    if re!(
        r#"\d+(?:\.\d+)?\s*(?:in|inch|inches|["”])?\s*[×xX*]\s*\d+(?:\.\d+)?"#,
        "i"
    )
    .is_match(&customer_origin)
        && p.measurement_dimensions_length_in.is_none()
        && p.measurement_dimensions_width_in.is_none()
        && p.countertop_sqft.is_none()
        && p.island_sqft.is_none()
        && p.backsplash_sqft.is_none()
    {
        push("explicit customer dimensions must populate measurement fields");
    }
    if super::super::lifecycle::is_terminal_customer_state(Some(p.latest_customer_state))
        && p.contact_resume_mode == ContactResumeMode::Date
    {
        push("terminal customer response must not keep contact_resume_mode DATE");
    }
    let wall_now = chrono::Utc::now();
    let expired = p.quote_expiration_date.is_some_and(|date| date < wall_now);
    if expired && !p.historical_quote_expiration_warning {
        push("past quote_expiration_date must set historical_quote_expiration_warning");
    }
    if p.unfinished_next_step.is_some()
        && re!(r"expiration quote|quote expir", "i").is_match(unfinished)
        && (p.historical_quote_expiration_warning || expired)
        && !p.historical_task
    {
        push("expired-quote task must be marked historical, not current unfinished_next_step");
    }
    if p.unfinished_next_step.is_some()
        && super::super::facts::manual::task_references_past_event(unfinished, wall_now)
        && !p.historical_task
    {
        push("stale dated CRM task must not remain a current unfinished_next_step");
    }
    if p.estimated_ready_at_source == EvidenceSource::CustomerSms
        && !customer_sms_supports_ready_timing(p)
    {
        push("estimated_ready_at_source customer_sms has no supporting customer SMS");
    }
    if p.hold_reason_type == Some(super::types::HoldReasonType::HigherPriorityDelay)
        && p.latest_customer_state == LatestCustomerState::Unknown
    {
        push("short-term customer delay must not leave latest_customer_state UNKNOWN");
    }
    if re!(r"^yes\b", "i").is_match(reply)
        && intent == Some(I::Waiting)
        && p.deal_product_scope == DealProductScope::Unknown
        && p.project_scope_source == EvidenceSource::None
    {
        push("affirmative reply to a project question must not leave deal_product_scope UNKNOWN");
    }
    if re!(r"\bbudget is set\b", "i").is_match(&customer_origin)
        && p.decision_obstacle == DecisionObstacle::Budget
        && p.budget_obstacle_evidence.is_none()
    {
        push("budget is set must not create a budget decision obstacle");
    }
    if re!(
        r"looking to get (a |an )?quote|looking for (a |an )?quote",
        "i"
    )
    .is_match(&customer_origin)
        && !p.quote_requested
    {
        push("explicit quote request must set quote_requested");
    }
    if matches!(
        p.explicit_customer_request_kind,
        Some(
            super::super::facts::ExplicitCustomerRequestKind::EstimatePricing
                | super::super::facts::ExplicitCustomerRequestKind::EstimateRequest
                | super::super::facts::ExplicitCustomerRequestKind::OnsiteQuote
        )
    ) && !p.quote_requested
    {
        push("customer directly asks project cost/estimate but quote_requested is false");
    }
    let manual_description = text(p.manual_action_description.as_ref());
    if re!(
        r"^Activity:.*\bcall (him|her|them|back|the customer)\b",
        "i"
    )
    .is_match(&cat(&[manual_description, unfinished]))
        && p.manual_action_required.is_none()
        && p.manual_action_status != super::super::facts::ManualActionStatus::Superseded
    {
        push("open actionable salesperson CALL task must set manual_action_required");
    }
    if p.known_package_interest.as_deref().is_some_and(|package| {
        re!(r"(?:11,999|11999|kitchen remodel|remodel package)", "i").is_match(package)
    }) && p.deal_product_scope == DealProductScope::Countertops
    {
        push("known full-remodel/package project must not collapse to COUNTERTOPS-only scope");
    }
    if re!(
        r"redo .{0,12}kitchen|kitchen remodel package|remove (?:the )?wall|island or a bar",
        "i"
    )
    .is_match(&customer_origin)
        && p.customer_project_components.is_empty()
    {
        push("target-specific CRM remodel note must populate customer_project_components");
    }
    if (p.quote_received_or_discussed
        || re!(r"\bbriefly looked at (?:it|the quote)\b", "i")
            .is_match(&cat(&[originated, summary])))
        && re!(r"(?:have not|haven't) .{0,40}review", "i")
            .is_match(&cat(&[originated, summary, hold_ev]))
        && p.latest_customer_state == LatestCustomerState::Unknown
    {
        push("partial quote review must not leave latest_customer_state UNKNOWN");
    }
    if re!(r"how many square feet|how much (?:square )?footage|(?:left|remaining).{0,24}(?:sq\.?\s*ft|sqft)", "i").is_match(&cat(&[explicit, originated]))
        && !re!(r"price per (?:sq\.?\s*ft|sqft|square foot)", "i").is_match(&cat(&[explicit, originated, &details]))
        && !re!(r"\d+\s*(?:sq\.?\s*ft|sqft)\b", "i").is_match(&cat(&[explicit, originated, &details]))
        && re!(r"\bcost\b|\bprice\b|\bpricing\b", "i").is_match(&cat(&[explicit, originated]))
        && p.active_unresolved_facts.is_empty()
    {
        push("pricing request needing dimensions must populate active_unresolved_facts");
    }
    if p.manual_action_required.is_some()
        && p.manual_action_status == MS::Superseded
        && p.preferred_next_step == super::super::facts::PreferredNextStep::PhoneCall
    {
        push("superseded manual action must not remain preferred_next_step PHONE_CALL");
    }
    // Open CRM sales tasks do not affect follow-ups; only a requested phone call does.
    if p.manual_action_required == Some(ManualActionRequired::PhoneCall)
        && p.manual_action_status == MS::Pending
        && p.automation_contact_status == AutomationContactStatus::Active
        && p.followup_sequence_status == Seq::Active
    {
        push("pending manual action must pause automated nurture");
    }
    if (p.latest_customer_state == LatestCustomerState::Declined
        || matches!(intent, Some(I::NotInterested | I::ProjectCompleted)))
        && p.last_customer_message_at.is_some()
        && p.latest_terminal_status_at.is_none()
    {
        push("terminal customer state must populate latest_terminal_status_at");
    }
    if matches!(p.manual_action_status, MS::Completed | MS::Superseded)
        && p.unfinished_next_step.is_some()
        && super::super::facts::manual::unfinished_step_matches_manual_action(
            unfinished,
            p.manual_action_description.as_deref(),
        )
    {
        push("completed or superseded manual action must not remain unfinished_next_step");
    }
    let summary_for_attribution = super::super::attribution::ProjectAttributionSummary {
        multi_project_customer: p.multi_project_customer,
        target_deal_id: p.target_deal_id,
        target_deal_status_label: p.target_deal_status.clone(),
        target_deal_is_terminal: matches!(p.deal_state, DealState::Won | DealState::Lost)
            || re!(r"closed\/won|closed\/lost", "i").is_match(text(p.target_deal_status.as_ref())),
        target_address: p.target_project_address.clone(),
        other_deal_ids: p.other_deal_ids.clone(),
        project_markers: p.other_project_markers.clone(),
        other_project_markers: p.other_project_markers.clone(),
    };
    for error in super::super::attribution::cross_project_attribution_errors(
        &summary_for_attribution,
        &super::super::attribution::AttributionCheck {
            explicit_customer_request: p.explicit_customer_request.as_deref(),
            unfinished_next_step: p.unfinished_next_step.as_deref(),
            material_preferences: &p.material_preferences,
            last_customer_message_summary: p.last_customer_message_summary.as_deref(),
            last_conversation_reply: p.last_conversation_reply.as_deref(),
        },
    ) {
        push(&error);
    }
    if re!(
        r"should (check|send)|send (him|her|them ) (a )?quote|send quote|check the backsplash|should send (?:the )?(?:measurements|pictures)",
        "i"
    )
    .is_match(&cat(&[unfinished, manual_description]))
        && p.manual_action_required.is_none()
    {
        push("pending salesperson deliverable must set manual_action_required");
    }
    let pronoun = re!(r"^(it|this|that|them|one)$", "i");
    if p.material_preferences
        .iter()
        .any(|name| pronoun.is_match(name))
        || p.known_material
            .as_deref()
            .is_some_and(|m| pronoun.is_match(m))
    {
        push("material_preferences contains pronoun entity");
    }
    if p.quote_received_or_discussed && !p.quote_sent {
        push("quote_received_or_discussed true but quote_sent false");
    }
    if p.material_preferences
        .iter()
        .any(|name| re!(r"white spring", "i").is_match(name))
        && p.material_preferences
            .iter()
            .any(|name| re!(r"^granite$", "i").is_match(name))
        && p.material_direction_status
            == super::super::facts::MaterialDirectionStatus::MultipleOpenOptions
    {
        push("named stone and parent material type classified as competing materials");
    }
    if re!(r"\bgot (?:a |the |your )?(?:\w+\s+){0,2}quote\b", "i")
        .is_match(&cat(&[hold_ev, originated]))
        && !p.quote_sent
    {
        push("CRM got-quote note but quote_sent is false");
    }
    if re!(
        r"will be ready in (about |approximately |approx\.? )?(a |one )?month|ready in (about |approximately |approx\.? )?(a |one )?month",
        "i"
    )
    .is_match(&cat(&[hold_ev, originated]))
        && p.latest_customer_state == LatestCustomerState::Waiting
        && p.estimated_ready_at.is_none()
    {
        push("readiness statement must populate estimated_ready_at");
    }
    if re!(
        r"\$[\d,]+(?:\.\d+)?\s+(?:kitchen remodel package|special)|looking for (?:the )?\$[\d,]+",
        "i"
    )
    .is_match(&cat(&[hold_ev, originated]))
        && p.known_package_interest.is_none()
    {
        push("package interest evidence must set known_package_interest");
    }
    if re!(r"text or email|sms or email|email or text", "i").is_match(&customer_origin)
        && p.customer_requested_contact_methods.len() == 1
    {
        push("dual preferred contact channels must not collapse to one method");
    }
    if re!(r"\bnot floor\b", "i").is_match(&cat(&[reply, originated, hold_ev]))
        && p.customer_project_components
            .iter()
            .any(|item| re!(r"floor", "i").is_match(item))
        && !p
            .unsupported_requested_components
            .iter()
            .any(|item| re!(r"floor", "i").is_match(item))
    {
        push(
            "salesperson unsupported flooring note must populate unsupported_requested_components",
        );
    }
    if p.material_preferences.len() > 1
        && p.favorite_material.is_none()
        && p.current_confirmed_material.is_none()
        && re!(r"leaning toward [^,]+(?:,|$)", "i").is_match(unfinished)
    {
        push("material list order must not imply a favorite without preference evidence");
    }
    if re!(r"\bpictures of the\s*$", "i").is_match(unfinished) && p.activity_object.is_some() {
        push("incomplete CRM task must not invent activity_object");
    }
    if p.manual_action_required == Some(ManualActionRequired::PhoneCall)
        && p.manual_action_status == MS::Pending
        && (p.quote_sent
            || super::super::facts::quotes::text_looks_like_quote_sent(&cat(&[reply, summary])))
    {
        push("manual action pending but newer evidence satisfies the action");
    }
    if re!(
        r"just sent (you )?(?:\d+|three|two|four|five|several)\s+(the |your )?(updated )?quotes?|sent (you )?(?:\d+|three|two|four|five|several)\s+quotes?",
        "i"
    )
    .is_match(&cat(&[reply, summary]))
        && !p.quote_sent
    {
        push("conversation says quote was sent but quote_sent is false");
    }
    if re!(r"\bcountertops?\b", "i").is_match(&cat(&[originated, reply, &details]))
        && p.deal_product_scope == DealProductScope::Unknown
        && has_direct_scope_conversation_evidence(p)
    {
        push("direct countertop conversation must not leave scope UNKNOWN after invalid repair");
    }
    if re!(
        r"promised to send|will send (you )?(the |your )?(updated )?quote|never sent it to you|updated your quote but never sent",
        "i"
    )
    .is_match(&cat(&[reply, unfinished]))
        && p.manual_action_required.is_none()
    {
        push("company promised quote must set manual_action_required");
    }
    if !p.measurement_conflict
        && p.measurement_dimensions_length_in.is_some()
        && p.measurement_dimensions_width_in.is_some()
        && matches!((p.customer_reported_area_sqft, p.calculated_area_from_dimensions_sqft), (Some(a), Some(b)) if (a - b).abs() > 2.0)
    {
        push("conflicting dimensions and area must set measurement_conflict");
    }
    if re!(
        r"approximate square footage|drawing still needed|does not have measurements yet|measurements still needed",
        "i"
    )
    .is_match(&cat(&[originated, &p.active_unresolved_facts.join(" ")]))
        && p.active_unresolved_facts.is_empty()
    {
        push("source says measurements still needed but active_unresolved_facts is empty");
    }
    if re!(r"\bwhite shaker\b", "i").is_match(&cat(&[summary, reply]))
        && p.cabinet_preferences.is_empty()
    {
        push("customer stated white shaker cabinets but cabinet_preferences is empty");
    }
    if p.project_areas.len() > 1
        && re!(r"\bbathroom\b", "i").is_match(&details)
        && re!(r"\bkitchen\b", "i").is_match(&details)
        && !p
            .project_details
            .iter()
            .any(|item| re!(r"bathroom", "i").is_match(item))
        && !p
            .project_details
            .iter()
            .any(|item| re!(r"kitchen", "i").is_match(item))
    {
        push("multiple project areas should appear in structured project details");
    }
    let size_source = [originated, reply, summary, unfinished, manual_description].join("\n");
    if let Some(size) = p.project_size_raw {
        let size = super::super::js::js_number(size);
        if super::super::js::js_regex(
            &format!(r"\$\s*{size}\b|\b{size}\s*(?:dollars?|discount|off|credit)\b"),
            "i",
        )
        .is_match(&size_source)
        {
            push("money value must not become project size");
        }
    }
    if (p.existing_countertop_removal
        || re!(r"existing granite removal|remove her granite", "i").is_match(&details))
        && p.material_type_preferences
            .iter()
            .any(|item| re!(r"^granite$", "i").is_match(item))
        && !re!(
            r"\bwant(?:s|ed)? (?:new )?granite\b|\blooking at granite\b|\bconsidering granite\b",
            "i"
        )
        .is_match(&cat(&[originated, explicit]))
    {
        push("existing granite removal must not become a target granite preference");
    }
    if re!(r#"\d+\s*["”]\s*x\s*\d+"#, "i").is_match(&cat(&[explicit, summary, reply]))
        && p.measurement_dimensions_length_in.is_none()
        && p.countertop_sqft.is_none()
        && p.project_size_raw.is_none()
        && p.unknown_sqft.is_none()
    {
        push("direct customer dimensions exist but typed measurement fields are empty");
    }
    if re!(r"signed (the |our |your )?contract", "i").is_match(&cat(&[summary, reply, originated]))
        && re!(
            r"received (your |the )?(check|payment|deposit)|received your check",
            "i"
        )
        .is_match(&cat(&[originated, reply, unfinished]))
        && !p.quote_received_or_discussed
    {
        push("signed contract/payment progression must set quote_received_or_discussed");
    }
    if p.manual_action_required.is_none()
        && re!(
            r"scheduling manager should call|our scheduling (manager|team) (should|will) call",
            "i"
        )
        .is_match(&cat(&[reply, originated]))
    {
        push("company scheduling obligation must set pending manual action");
    }
    let invalid_material = p
        .material_preferences
        .iter()
        .chain(&p.historical_material_preferences)
        .chain(p.favorite_material.iter())
        .chain(p.current_confirmed_material.iter())
        .chain(p.known_material.iter())
        .filter(|name| !name.is_empty())
        .any(|name| !super::super::facts::materials::is_valid_material_entity(name));
    if invalid_material {
        push("ordinary-language fragment in material_preferences");
    }
    if let (Some(length), Some(width), Some(island)) = (
        p.measurement_dimensions_length_in,
        p.measurement_dimensions_width_in,
        p.island_sqft.as_ref(),
    ) {
        if p.calculated_area_from_dimensions_sqft
            .is_some_and(|area| (island.value - area).abs() > 2.0)
        {
            push("derived island area does not reconcile with stored dimensions");
        }
        if (island.value - (length * width) / 144.0).abs() > 2.0 {
            push("11ft by 5ft 4in cannot become a tiny island_sqft");
        }
    }
    let origin_for_measures = cat(&[originated, reply]);
    let sqft_of = |pattern: &regex::Regex| {
        pattern
            .captures(&origin_for_measures)
            .and_then(|caps| caps.get(1))
            .filter(|m| !m.as_str().is_empty())
            .and_then(|m| super::super::js::js_parse_number(m.as_str()))
    };
    let kitchen_sqft = sqft_of(re!(
        r"(\d+(?:\.\d+)?)\s*(?:sq\.?\s*ft|sqft|sf)\s+of\s+kitchen",
        "i"
    ));
    let vanity_sqft = sqft_of(re!(
        r"(\d+(?:\.\d+)?)\s*(?:sq\.?\s*ft|sqft|sf)\s+of\s+vanity",
        "i"
    ));
    if let (Some(kitchen), Some(vanity), Some(stored)) =
        (kitchen_sqft, vanity_sqft, p.vanity_sqft.as_ref())
        && (stored.value - kitchen).abs() < 0.01
        && (kitchen - vanity).abs() > 1.0
    {
        push("kitchen and vanity square footage are reversed or misattributed");
    }
    let explicit_total = sqft_of(re!(r"\b(\d+(?:\.\d+)?)\s+total\b", "i"));
    if let (Some(kitchen), Some(vanity), Some(total)) = (kitchen_sqft, vanity_sqft, explicit_total)
    {
        let fields_reconcile = match (&p.total_project_sqft, &p.vanity_sqft, &p.countertop_sqft) {
            (Some(_), Some(vanity_field), Some(countertop)) => {
                (countertop.value + vanity_field.value - total).abs() <= 1.0
            }
            _ => false,
        };
        if (kitchen + vanity - total).abs() <= 1.0 && !fields_reconcile {
            push("explicit total and component fields do not reconcile");
        }
    }
    if p.latest_customer_state == LatestCustomerState::TemplateApproved
        && p.followup_starting_phase == FollowupPhase::ActivePursuit
        && p.quote_evidence
        && p.unfinished_next_step.is_some()
        && re!(r"\bquote\b|\bestimate\b|material selection", "i").is_match(unfinished)
        && !matches!(p.manual_action_status, MS::Superseded | MS::Completed)
    {
        push("template approved but old quote task is still treated as current");
    }
    if ((re!(r"\bfireplace\b", "i").is_match(&origin_for_measures)
        && re!(r"\bkitchen\b", "i").is_match(&origin_for_measures))
        || re!(
            r"\bother project\b|\blater kitchen\b|\banother (house|home|property)\b",
            "i"
        )
        .is_match(&origin_for_measures))
        && !p.multi_project_customer
    {
        push("semantic evidence shows several projects but multi_project_customer is false");
    }
    if p.communication_history_status == super::types::CommunicationHistoryStatus::TrulyNew
        && (p.deal_product_scope != DealProductScope::Unknown || filled(p.known_scope.as_ref()))
        && p.last_customer_message_at.is_none()
        && p.followup_starting_phase == FollowupPhase::Reactivation
    {
        push("truly_new known-scope lead must not start in REACTIVATION");
    }
    errors
}

pub(crate) fn customer_sms_supports_ready_timing(p: &FollowupContactPressure) -> bool {
    let text = [
        p.last_customer_message_summary.as_deref().unwrap_or(""),
        p.last_conversation_reply.as_deref().unwrap_or(""),
        p.last_customer_originated_information
            .as_deref()
            .unwrap_or(""),
        p.original_timeline_evidence.as_deref().unwrap_or(""),
        p.hold_reason_evidence.as_deref().unwrap_or(""),
        p.explicit_customer_request.as_deref().unwrap_or(""),
    ]
    .join(" ");
    re!(
        r"ready in|next spring|next year|in a year|started in|be ready|plan to have this work started|late (january|february|march|april|may|june|july|august|september|october|november|december)|early (january|february|march|april|may|june|july|august|september|october|november|december)",
        "i"
    )
    .is_match(&text)
}

/// Direct conversation evidence of the product (countertops, cabinets, a material).
pub(crate) fn has_direct_scope_conversation_evidence(p: &FollowupContactPressure) -> bool {
    let corpus = [
        p.last_customer_originated_information
            .clone()
            .unwrap_or_default(),
        p.last_conversation_reply.clone().unwrap_or_default(),
        p.last_customer_message_summary.clone().unwrap_or_default(),
        p.project_details.join(" "),
        p.countertop_preferences.join(" "),
        p.material_preferences.join(" "),
        p.explicit_customer_request.clone().unwrap_or_default(),
        p.known_scope.clone().unwrap_or_default(),
        p.customer_project_components.join(" "),
    ]
    .join("\n");
    re!(r"\bcountertops?\b", "i").is_match(&corpus)
        || re!(r"\bcabinets?\b", "i").is_match(&corpus)
        || re!(r"\bkitchen remodel\b", "i").is_match(&corpus)
        || re!(r"\bbathroom\b", "i").is_match(&corpus)
        || p.customer_project_components
            .iter()
            .any(|c| c == "countertops" || c == "cabinets")
        || !p.material_type_preferences.is_empty()
        || re!(
            r"\b(quartz|soap\s*stone|granite|marble|quartzite|dekton)\b",
            "i"
        )
        .is_match(&corpus)
}

/// Automated follow-ups may run unless the customer ended the pursuit.
pub fn automated_followup_may_execute(p: &FollowupContactPressure) -> bool {
    !super::super::lifecycle::is_terminal_customer_state(Some(p.latest_customer_state))
}
