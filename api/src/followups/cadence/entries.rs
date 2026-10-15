//! Steps built from what we know about the customer: material, quote,
//! request, budget, dependency... (part of `smsFlowAiCadence.ts`).

use super::super::facts::extract::empty_followup_facts;
use super::super::facts::{
    ActivityActor, BacksplashType, DealProductScope, EvidenceSource, ExplicitCustomerRequestKind,
    FollowupFacts, MeasurementsStatus, ProjectDependencyType, SinkSource, TimeSensitiveFact,
    UnfinishedNextStepKind,
};
use super::super::intent::MeaningfulCustomerIntentType;
use super::super::js::js_number;
use super::super::pressure::{FollowupAngle as A, FollowupContactPressure};
use super::types::CadenceShapeEntry;
use crate::re;

fn filled(value: Option<&String>) -> bool {
    value.is_some_and(|v| !v.is_empty())
}

/// The facts the planner reads, rebuilt from a contact pressure.
#[allow(clippy::too_many_lines)]
pub(crate) fn facts_from_pressure(pressure: Option<&FollowupContactPressure>) -> FollowupFacts {
    let mut f = empty_followup_facts();
    let Some(p) = pressure else {
        return f;
    };
    f.quote_type = p.quote_type;
    f.quote_requested = p.quote_requested;
    f.quote_evidence = p.quote_evidence;
    f.quote_created = p.quote_created;
    f.quote_sent = p.quote_sent;
    f.quote_received_or_discussed = p.quote_received_or_discussed;
    f.quote_evidence_source = p.quote_evidence_source;
    f.quote_evidence_confidence = p.quote_evidence_confidence;
    f.quote_expiration_evidence
        .clone_from(&p.quote_expiration_evidence);
    f.quote_expiration_date = p.quote_expiration_date;
    f.explicit_customer_request
        .clone_from(&p.explicit_customer_request);
    f.explicit_customer_request_kind = p.explicit_customer_request_kind;
    f.needs_sink = p.needs_sink;
    f.sink_required = p.sink_required;
    f.sink_source = p.sink_source;
    f.needs_sink_from_company = p.needs_sink_from_company;
    f.backsplash_known = p.backsplash_known;
    f.backsplash_type = p.backsplash_type;
    f.stone_backsplash_required = p.stone_backsplash_required;
    f.backsplash_finish = p.backsplash_finish;
    f.backsplash_material.clone_from(&p.backsplash_material);
    f.backsplash_description
        .clone_from(&p.backsplash_description);
    f.uncertain_material_direction = p.uncertain_material_direction;
    f.material_direction_status = p.material_direction_status;
    f.preferred_material.clone_from(&p.preferred_material);
    f.did_not_find_suitable_option = p.did_not_find_suitable_option;
    f.rejected_previous_options = p.rejected_previous_options;
    f.does_not_like_current_selection = p.does_not_like_current_selection;
    f.known_material.clone_from(&p.known_material);
    f.quoted_material_context
        .clone_from(&p.quoted_material_context);
    f.historical_material_preferences
        .clone_from(&p.historical_material_preferences);
    f.material_preferences.clone_from(&p.material_preferences);
    f.favorite_material.clone_from(&p.favorite_material);
    f.countertop_preferences
        .clone_from(&p.countertop_preferences);
    f.cabinet_preferences.clone_from(&p.cabinet_preferences);
    f.current_confirmed_material
        .clone_from(&p.current_confirmed_material);
    f.material_selection_history = p.material_selection_history;
    f.material_selection_evidence = p.material_selection_evidence;
    f.material_selection_evidence_source = p.material_selection_evidence_source;
    f.original_timeline_evidence
        .clone_from(&p.original_timeline_evidence);
    f.timeline_evidence_source = p.timeline_evidence_source;
    f.timeline_evidence_confidence = p.timeline_evidence_confidence;
    f.known_scope.clone_from(&p.known_scope);
    f.project_scope_source = p.project_scope_source;
    f.project_scope_confidence = p.project_scope_confidence;
    f.unfinished_next_step.clone_from(&p.unfinished_next_step);
    f.unfinished_next_step_kind = p.unfinished_next_step_kind;
    f.unfinished_possibly_satisfied = p.unfinished_possibly_satisfied;
    f.construction_dependency_evidence
        .clone_from(&p.construction_dependency_evidence);
    f.construction_delay_source = p.construction_delay_source;
    f.construction_delay_confidence = p.construction_delay_confidence;
    f.budget_obstacle_evidence
        .clone_from(&p.budget_obstacle_evidence);
    f.budget_evidence = p.budget_evidence;
    f.budget_evidence_source = p.budget_evidence_source;
    f.budget_evidence_confidence = p.budget_evidence_confidence;
    f.financial_delay_source = p.financial_delay_source;
    f.financial_delay_confidence = p.financial_delay_confidence;
    f.deal_product_scope = p.deal_product_scope;
    f.activity_actor = p.activity_actor;
    f.activity_object.clone_from(&p.activity_object);
    f.time_sensitive_facts = p
        .pricing_change_valid_until
        .map(|until| {
            vec![TimeSensitiveFact {
                kind: "pricing_change",
                valid_from: None,
                valid_until: Some(until),
                evidence: "pricing change".into(),
            }]
        })
        .unwrap_or_default();
    f.known_package_interest
        .clone_from(&p.known_package_interest);
    f.decision_obstacle_evidence
        .clone_from(&p.decision_obstacle_evidence);
    f.decision_criteria.clone_from(&p.decision_criteria);
    f.decision_obstacle = p.decision_obstacle;
    f.project_details.clone_from(&p.project_details);
    f.budget_target_approx = p.budget_target_approx;
    f.comparing_quotes = p.comparing_quotes;
    f.active_decision_factors
        .clone_from(&p.active_decision_factors);
    f.latest_customer_state = p.latest_customer_state;
    f.active_unresolved_facts
        .clone_from(&p.active_unresolved_facts);
    f.existing_countertop_material
        .clone_from(&p.existing_countertop_material);
    f.existing_countertop_removal = p.existing_countertop_removal;
    f.removal_required = p.removal_required;
    f.removal_component = p.removal_component;
    f.customer_project_components
        .clone_from(&p.customer_project_components);
    f.unsupported_requested_components
        .clone_from(&p.unsupported_requested_components);
    f.historical_quote_expiration_warning = p.historical_quote_expiration_warning;
    f.historical_task = p.historical_task;
    f.alternative_material_offered
        .clone_from(&p.alternative_material_offered);
    f.installation_requested = p.installation_requested;
    f.price_context = p.price_context;
    f.material_type_preferences
        .clone_from(&p.material_type_preferences);
    f.countertop_color_preferences
        .clone_from(&p.countertop_color_preferences);
    f.estimated_ready_at = p.estimated_ready_at;
    f.estimated_ready_at_source = p.estimated_ready_at_source;
    f.estimated_ready_at_confidence = p.estimated_ready_at_confidence;
    f.estimated_ready_at_precision = p.estimated_ready_at_precision;
    f.estimated_ready_window_start = p.estimated_ready_window_start;
    f.estimated_ready_window_end = p.estimated_ready_window_end;
    f.project_scale = p.project_scale;
    f.approx_unit_count = p.approx_unit_count;
    f.preferred_next_step = p.preferred_next_step;
    f.preferred_next_step_source = p.preferred_next_step_source;
    f.preferred_next_step_confidence = p.preferred_next_step_confidence;
    f.customer_requested_contact_method = p.customer_requested_contact_method;
    f.salesperson_planned_contact_method = p.salesperson_planned_contact_method;
    f.appointment_companion = p.appointment_companion;
    f.completed_estimate_satisfied = p.completed_estimate_satisfied;
    f.customer_requested_contact_window_start = p.customer_requested_contact_window_start;
    f.customer_requested_contact_window_end = p.customer_requested_contact_window_end;
    f.planned_customer_event_type = p.planned_customer_event_type;
    f.planned_customer_event_window_start = p.planned_customer_event_window_start;
    f.planned_customer_event_window_end = p.planned_customer_event_window_end;
    f.planned_customer_event_confidence = p.planned_customer_event_confidence;
    f.planned_customer_event_date_label
        .clone_from(&p.planned_customer_event_date_label);
    f.visual_style = p.visual_style;
    f.showroom_interest = p.showroom_interest;
    f.countertop_measurements_status = p.countertop_measurements_status;
    f.project_size_raw = p.project_size_raw;
    f.countertop_sqft.clone_from(&p.countertop_sqft);
    f.backsplash_sqft.clone_from(&p.backsplash_sqft);
    f.island_sqft.clone_from(&p.island_sqft);
    f.vanity_sqft.clone_from(&p.vanity_sqft);
    f.total_project_sqft.clone_from(&p.total_project_sqft);
    f.unknown_sqft.clone_from(&p.unknown_sqft);
    f.measurement_conflict = p.measurement_conflict;
    f.measurement_dimensions_length_in = p.measurement_dimensions_length_in;
    f.measurement_dimensions_width_in = p.measurement_dimensions_width_in;
    f.calculated_area_from_dimensions_sqft = p.calculated_area_from_dimensions_sqft;
    f.customer_reported_area_sqft = p.customer_reported_area_sqft;
    f.customer_reported_island_sqft = p.customer_reported_island_sqft;
    f.measurement_raw_text.clone_from(&p.measurement_raw_text);
    f.future_volume_sqft = p.future_volume_sqft;
    f.project_dependencies.clone_from(&p.project_dependencies);
    f.manual_action_required = p.manual_action_required;
    f.manual_action_description
        .clone_from(&p.manual_action_description);
    f.manual_action_tasks.clone_from(&p.manual_action_tasks);
    f
}

fn primary_material_name(f: &FollowupFacts) -> String {
    f.favorite_material
        .clone()
        .or_else(|| f.current_confirmed_material.clone())
        .or_else(|| f.known_material.clone())
        .or_else(|| f.material_preferences.first().cloned())
        .unwrap_or_else(|| "the preferred material".into())
}

fn material_preference_subangle(f: &FollowupFacts) -> &'static str {
    let material = primary_material_name(f);
    if re!(r"black mist", "i").is_match(&material) {
        return "black_mist_status";
    }
    match f
        .current_confirmed_material
        .as_deref()
        .filter(|m| !m.is_empty())
    {
        Some(confirmed) if re!(r"ordered", "i").is_match(confirmed) => "ordered_material",
        Some(_) => "selected_material",
        None if filled(f.favorite_material.as_ref()) => "favorite_option",
        None => "preferred_option",
    }
}

pub(crate) fn material_preference_entry(f: &FollowupFacts) -> CadenceShapeEntry {
    let material = f
        .preferred_material
        .clone()
        .unwrap_or_else(|| primary_material_name(f));
    let mut alternatives: Vec<String> = Vec::new();
    for item in f
        .material_preferences
        .iter()
        .chain(&f.material_type_preferences)
    {
        if !alternatives
            .iter()
            .any(|other| other.to_lowercase() == item.to_lowercase())
        {
            alternatives.push(item.clone());
        }
    }
    let confirmed = f.current_confirmed_material.is_some();
    let quoted = f.quoted_material_context.clone();
    let quote_only_material = quoted.is_some()
        && f.favorite_material.is_none()
        && f.current_confirmed_material.is_none()
        && f.preferred_material.is_none()
        && alternatives.len() <= 1;
    let replacement_target =
        if f.existing_countertop_material.is_some() && f.material_type_preferences.len() == 1 {
            f.material_type_preferences.first().cloned()
        } else {
            None
        };
    let color_hint = if f.countertop_color_preferences.is_empty() {
        String::new()
    } else {
        format!("{} ", f.countertop_color_preferences.join("/"))
    };
    let ranking = if quote_only_material {
        let q = quoted.unwrap_or_default();
        format!(
            "They received a {q} quote. Ask whether they would like to revisit that quote or whether {q} is still something they are considering. Do NOT say they are leaning toward, prefer, or favor {q}. One question only."
        )
    } else if confirmed {
        format!(
            "Ask whether {} is still the selected direction.",
            f.current_confirmed_material.as_deref().unwrap_or("")
        )
    } else if let Some(target) = replacement_target.filter(|t| !t.is_empty()) {
        format!(
            "They want to change {} to {color_hint}{target}. Ask about the {color_hint}{target} direction, backsplash choice, or quote review — do NOT ask granite vs quartz. Treat this as a preference, not a confirmed selection.",
            f.existing_countertop_material.as_deref().unwrap_or("")
        )
    } else if alternatives.len() > 1 {
        let list = alternatives.join(", ");
        let lean = f
            .preferred_material
            .as_deref()
            .filter(|m| !m.is_empty())
            .map_or_else(String::new, |m| format!(", with {m} as the stronger lean"));
        format!(
            "They were considering {list}{lean}. Ask whether they are still considering {list}, or whether one option has moved ahead — do not treat the first listed option as the favorite unless evidence says so. Do not say it is \"the direction\" or that they selected it."
        )
    } else {
        format!(
            "Ask whether they are still leaning toward {material}. Treat this as a preference, not a confirmed selection."
        )
    };
    CadenceShapeEntry::stalled(
        if confirmed {
            "Confirm material selection"
        } else {
            "Check open material preferences"
        },
        A::MaterialPreference,
        material_preference_subangle(f),
        match f.unfinished_next_step.as_deref().filter(|s| !s.is_empty()) {
            Some(step) => {
                format!("Use the unfinished next step ({step}) and {ranking} One question only.")
            }
            None => format!("{ranking} Do not invent a timeline. One question only."),
        },
    )
}

pub(crate) fn agreed_specials_entry() -> CadenceShapeEntry {
    CadenceShapeEntry::stalled(
        "Follow through on the agreed next step",
        A::PreferredNextStep,
        "send_current_specials",
        "They asked you to send current specials closer to their project date. Follow through on that agreed next step. You may say they asked for current specials closer to their timing. Do not invent a discount, expiration, package, or dollar amount unless current verified promotion data is in the record. Do not open with generic budget discovery or comparison shopping.",
    )
}

pub(crate) fn material_fit_entry(f: &FollowupFacts) -> CadenceShapeEntry {
    let color = f
        .countertop_color_preferences
        .first()
        .map_or("the color they wanted", String::as_str);
    let kind = f
        .material_type_preferences
        .first()
        .map_or("stone", String::as_str);
    CadenceShapeEntry::stalled(
        "Help find a suitable material option",
        A::MaterialPreference,
        "unresolved_material_fit",
        format!(
            "They wanted {color} {kind} and did not find a suitable option. Ask whether they want help narrowing current options. Do not claim inventory has changed unless current inventory data is in the record. Do not open with generic timing or \"is {kind} still the direction\"."
        ),
    )
}

pub(crate) fn previous_stone_selection_entry() -> CadenceShapeEntry {
    CadenceShapeEntry::stalled(
        "Revisit stones they already looked at",
        A::PreviousStoneSelection,
        "picked_stones_unnamed",
        "History shows they picked out or looked at stones, but specific names are unknown. Refer to “the stones you looked at / picked out previously”. Do not invent color or material names. Do not use generic quartz-vs-granite education.",
    )
}

pub(crate) fn has_scheduling_evidence(
    f: &FollowupFacts,
    pressure: Option<&FollowupContactPressure>,
) -> bool {
    let moving = |p: &FollowupContactPressure| {
        p.latest_meaningful_customer_intent
            .as_ref()
            .is_some_and(|i| i.kind == MeaningfulCustomerIntentType::MovingForward)
            || p.additional_customer_intents
                .iter()
                .any(|i| i.kind == MeaningfulCustomerIntentType::MovingForward)
    };
    if pressure.is_some_and(moving) {
        return true;
    }
    let request = format!(
        "{} {}",
        f.explicit_customer_request.as_deref().unwrap_or(""),
        f.unfinished_next_step.as_deref().unwrap_or("")
    );
    re!(r"appointment|template|install|schedul", "i").is_match(&request)
}

pub(crate) fn dormant_project_status_entry(f: &FollowupFacts) -> CadenceShapeEntry {
    CadenceShapeEntry::stalled(
        "Check project status",
        A::ProjectStatus,
        "quote_or_options_revisit",
        if f.material_selection_history {
            "Repeated non-responder. Offer to revisit the quote or the stone options they looked at previously. Do not invent stone names. Do not introduce scheduling, appointment, template, or installation."
        } else {
            "Repeated non-responder. Offer to revisit the quote or current options. Do not introduce scheduling, appointment, template, or installation unless there is fresh movement-forward intent."
        },
    )
}

pub(crate) fn quote_status_entry(f: &FollowupFacts) -> CadenceShapeEntry {
    let material = f.known_material.as_deref().filter(|m| !m.is_empty());
    let cabinet = f.deal_product_scope == DealProductScope::Cabinets;
    let expiration_past = f
        .quote_expiration_date
        .is_some_and(|date| date < chrono::Utc::now());
    let historical = f.quote_evidence_source == EvidenceSource::CrmActivity || expiration_past;
    let goal = if cabinet {
        "Review the cabinet quote"
    } else if historical {
        "Revisit historical quote context"
    } else {
        "Check quote status"
    };
    let subangle = if historical {
        "historical_quote"
    } else if material.is_some() {
        material_preference_subangle(f)
    } else {
        "quote_review"
    };
    let purpose = if cabinet {
        if f.quote_received_or_discussed {
            "Review the cabinet quote or design. Do not mention stone inventory, quartz, granite, or countertops unless evidence connects them to this deal.".to_string()
        } else {
            "There is an earlier cabinet quote on file. Offer to revisit it. Do not say the pricing was discussed, reviewed, or received unless quote_received_or_discussed is yes.".to_string()
        }
    } else if historical {
        if f.quote_received_or_discussed {
            "CRM shows a historical quote. Use that old-quote / expiration context early. Do not claim an expired quote is still valid. Ask whether the project is still relevant or they want updated pricing. Do not invent a current quote.".to_string()
        } else {
            "There is an earlier quote on file. Offer to revisit it. Do not say we discussed or they reviewed the pricing unless quote_received_or_discussed is yes.".to_string()
        }
    } else if let Some(material) = material {
        format!(
            "There is verified quote evidence and a known preference for {material}. Ask whether that is still the direction. Do not claim anything was ordered. Do not say the quote was discussed unless quote_received_or_discussed is yes."
        )
    } else if f.quote_received_or_discussed {
        "There is verified quote evidence. Ask a specific quote question. Do not invent a timeline or project scope.".to_string()
    } else {
        "There is a quote on file. Offer to revisit the earlier quote. Do not say we discussed, they reviewed, or they received it unless quote_received_or_discussed is yes.".to_string()
    };
    CadenceShapeEntry::stalled(
        goal,
        if cabinet {
            A::QuoteReview
        } else {
            A::QuoteStatus
        },
        subangle,
        purpose,
    )
}

pub(crate) fn company_owed_quote_entry(f: &FollowupFacts) -> CadenceShapeEntry {
    let materials = f
        .countertop_preferences
        .iter()
        .chain(&f.material_preferences)
        .filter(|m| !m.is_empty())
        .take(2)
        .cloned()
        .collect::<Vec<_>>()
        .join(" / ");
    CadenceShapeEntry::stalled(
        "Deliver the promised quote update",
        A::QuoteStatus,
        "promised_quote_delivery",
        if materials.is_empty() {
            "Granite Depot still owes the customer a promised quote update. Follow up on sending the revised quote now. Do not open with on-site estimate, showroom visit, or generic nurture while this deliverable is unresolved.".to_string()
        } else {
            format!(
                "Granite Depot still owes the customer a promised quote ({materials}). Follow up on the updated pricing or send the revised quote now. Do not open with on-site estimate, showroom visit, or generic nurture while this deliverable is unresolved."
            )
        },
    )
}

pub(crate) fn customer_request_entry(f: &FollowupFacts) -> CadenceShapeEntry {
    use ExplicitCustomerRequestKind as K;
    if f.unfinished_next_step_kind == Some(UnfinishedNextStepKind::CustomerMeasurements) {
        return customer_measurements_entry(f);
    }
    if f.unfinished_next_step_kind == Some(UnfinishedNextStepKind::ScheduleMeasurement)
        || f.explicit_customer_request_kind == Some(K::PricePerSqft)
    {
        return schedule_measurement_entry();
    }
    if matches!(
        f.explicit_customer_request_kind,
        Some(K::FabricationRequest | K::ProductAvailability)
    ) {
        let purpose = match f
            .alternative_material_offered
            .as_deref()
            .filter(|m| !m.is_empty())
        {
            Some(alternative) => format!(
                "{} A salesperson offered {alternative} as an alternative. Ask whether that alternative works — do not ask generic quartz or color questions.",
                f.explicit_customer_request
                    .as_deref()
                    .unwrap_or("They requested a specific fabricated remnant.")
            ),
            None => format!(
                "{} Follow up on availability, fabrication, and installation for that exact request. Do not ask generic quartz-vs-granite questions.",
                f.explicit_customer_request
                    .as_deref()
                    .unwrap_or("They requested a specific fabricated product.")
            ),
        };
        return CadenceShapeEntry::stalled(
            "Follow up on the specific product request",
            A::ProjectStatus,
            "fabrication_or_availability",
            purpose,
        );
    }
    if matches!(
        f.explicit_customer_request_kind,
        Some(K::EstimatePricing | K::SquareFootage)
    ) {
        return CadenceShapeEntry::stalled(
            "Answer remaining square footage and cost",
            A::EstimateOffer,
            "remaining_square_footage",
            "Their strongest known question is remaining square footage and cost. Lead with that. Do not open with a generic “is this back on your radar?” if this request is still unresolved. One question that helps produce an estimate.",
        );
    }
    CadenceShapeEntry::stalled(
        "Follow up on the on-site quote request",
        A::EstimateOffer,
        "onsite_quote_request",
        match f.explicit_customer_request.as_deref().filter(|r| !r.is_empty()) {
            Some(request) => format!(
                "They asked: {request}. Treat this as a quote request, not proof a quote exists. Offer an on-site or business visit / estimate. Do not mention “your quote” or teach quartz vs granite."
            ),
            None => "They asked for a quote. Offer to come give an estimate. Do not claim a quote already exists.".to_string(),
        },
    )
}

pub(crate) fn project_context_entry(f: &FollowupFacts) -> CadenceShapeEntry {
    let mut bits: Vec<String> = Vec::new();
    if let Some(scope) = f.known_scope.as_deref().filter(|s| !s.is_empty()) {
        bits.push(scope.to_string());
    }
    if f.backsplash_known {
        bits.push("known backsplash preference".into());
    }
    if f.needs_sink_from_company {
        bits.push("they need a sink from us".into());
    } else if f.sink_source == Some(SinkSource::CustomerExternal) {
        bits.push("they are sourcing a sink elsewhere".into());
    }
    if f.uncertain_material_direction
        && (!f.material_preferences.is_empty()
            || f.known_material.is_some()
            || f.favorite_material.is_some())
    {
        bits.push("material direction is still open".into());
    }
    let scope_label = if f
        .known_scope
        .as_deref()
        .is_some_and(|s| !s.is_empty() && re!(r"kitchen", "i").is_match(s))
    {
        "kitchen project details"
    } else {
        "known project details"
    };
    let joined = bits.join(", ");
    CadenceShapeEntry::stalled(
        format!("Use known {scope_label}"),
        A::DesignHelp,
        "project_requirements",
        format!(
            "Use the known project details ({}) before generic education. Do not send quartz vs granite maintenance. One useful next step.",
            if joined.is_empty() {
                "project, plans, next step, what matters most"
            } else {
                &joined
            }
        ),
    )
}

pub(crate) fn budget_obstacle_entry(f: &FollowupFacts) -> CadenceShapeEntry {
    let target = f
        .budget_target_approx
        .map_or_else(String::new, |t| format!(" around ${}", js_number(t)));
    CadenceShapeEntry::stalled(
        "Address the budget constraint",
        A::BudgetObstacle,
        "budget_fit",
        match f
            .budget_obstacle_evidence
            .as_deref()
            .filter(|e| !e.is_empty())
        {
            Some(evidence) => format!(
                "They raised a budget concern ({evidence}{target}). Ask whether they still want options reviewed around that goal. Do not invent a price or promise something is below a number unless it is in the record. Do not use generic quartz-vs-granite education."
            ),
            None => format!(
                "They have a budget concern{target}. Ask whether they still want current options reviewed around that goal. Do not invent pricing."
            ),
        },
    )
}

pub(crate) fn showroom_appointment_entry(f: &FollowupFacts) -> CadenceShapeEntry {
    CadenceShapeEntry::stalled(
        "Schedule the showroom visit",
        A::ProjectTiming,
        "showroom_or_next_step",
        match f.unfinished_next_step.as_deref().filter(|s| !s.is_empty()) {
            Some(step) => format!(
                "Open next step: {step}. Offer to schedule the showroom visit so they can see the material in person."
            ),
            None => "Offer to schedule a showroom visit for the material they wanted to see."
                .to_string(),
        },
    )
}

pub(crate) fn in_home_estimate_entry(f: &FollowupFacts) -> CadenceShapeEntry {
    let measurements = if f.countertop_measurements_status == Some(MeasurementsStatus::Provided) {
        " Do not ask for basic countertop measurements again; they were already provided."
    } else {
        ""
    };
    CadenceShapeEntry::stalled(
        "Schedule the in-home estimate",
        A::EstimateOffer,
        "in_home_estimate",
        if f.backsplash_type == BacksplashType::Full {
            format!(
                "Ask whether they still want to schedule the in-home estimate for the full-height backsplash.{measurements} Do not reopen a generic earlier quote as the first ask."
            )
        } else {
            format!(
                "Ask whether they still want to schedule the in-home estimate ({}).{measurements} Do not reopen a generic earlier quote as the first ask.",
                f.unfinished_next_step
                    .as_deref()
                    .unwrap_or("in-home estimate")
            )
        },
    )
}

pub(crate) fn comparison_support_entry() -> CadenceShapeEntry {
    CadenceShapeEntry::stalled(
        "Help with price comparison",
        A::ComparisonSupport,
        "comparing_prices",
        "They are comparing prices or collecting quotes. Offer comparison support. Do not use quote_status, quote_review, \"your quote\", or \"clarify anything in the quote\" unless quote evidence is verified with a source and at least medium confidence.",
    )
}

pub(crate) fn multi_unit_entry(f: &FollowupFacts) -> CadenceShapeEntry {
    let units = f
        .approx_unit_count
        .map_or_else(String::new, |n| format!(" around {} units", js_number(n)));
    CadenceShapeEntry::stalled(
        "Revisit the multi-unit project",
        A::ProjectScope,
        "multi_unit_readiness",
        format!(
            "This is a multi-unit / builder project{units}. Ask about project readiness, unit count or scope, quote update, or current timing. Do not use consumer quartz-vs-granite education. Do not invent commercial discounts, builder pricing, or bulk inventory."
        ),
    )
}

pub(crate) fn preferred_next_step_entry(f: &FollowupFacts) -> CadenceShapeEntry {
    CadenceShapeEntry::stalled(
        "Offer an easy next step",
        A::PreferredNextStep,
        "channel_preference",
        if f.deal_product_scope == DealProductScope::Cabinets {
            "Ask whether a short call or a reply about the cabinet quote is easier. Do not ask kitchen vs vanity. Do not mention stone inventory."
        } else {
            "Ask whether a short call or a reply is easier. One question only."
        },
    )
}

pub(crate) fn project_dependency_entry(f: &FollowupFacts) -> CadenceShapeEntry {
    match f.project_dependencies.first().map(|d| d.kind) {
        Some(ProjectDependencyType::CabinetLeveling) => CadenceShapeEntry::stalled(
            "Check cabinet leveling dependency",
            A::ProjectDependency,
            "cabinet_leveling",
            "Ask whether the cabinets are leveled now so they can move forward with the countertops. Do not ask about cabinet finish, paint, or wood grain. The product being sold is countertops. The cabinets are only a prerequisite.",
        ),
        Some(ProjectDependencyType::FloorInstallation) => CadenceShapeEntry::stalled(
            "Check flooring installation status",
            A::ProjectDependency,
            "floor_installation",
            "Ask whether the floors are in so they can make countertop decisions. Do not invent a ready date from “a couple.” Do not open with quote adjustment or design education.",
        ),
        _ => CadenceShapeEntry::stalled(
            "Check the blocking project dependency",
            A::ProjectDependency,
            "blocking_dependency",
            match f
                .construction_dependency_evidence
                .as_deref()
                .filter(|e| !e.is_empty())
            {
                Some(evidence) => format!(
                    "Ask about the known blocking dependency ({evidence}). Do not invent a different product or life event."
                ),
                None => "Ask about the known blocking dependency before quote or design angles."
                    .to_string(),
            },
        ),
    }
}

pub(crate) fn customer_measurements_entry(f: &FollowupFacts) -> CadenceShapeEntry {
    let actor_hint = match f.activity_actor {
        Some(ActivityActor::Customer) => "They were expected to send measurements.",
        Some(ActivityActor::Salesperson) => {
            "Measurements may need to be taken or sent by the company."
        }
        _ => "Measurements still need to be exchanged or confirmed.",
    };
    CadenceShapeEntry::stalled(
        "Confirm measurement availability",
        A::EstimateOffer,
        "pending_measurements",
        format!(
            "{actor_hint} Confirm the project is still active and ask whether kitchen measurements are now available. Do not open with timing-only if this measurement step is unresolved. Do not claim the customer personally promised to send them unless activity_actor is customer."
        ),
    )
}

pub(crate) fn schedule_measurement_entry() -> CadenceShapeEntry {
    CadenceShapeEntry::stalled(
        "Reschedule the measurement",
        A::EstimateOffer,
        "schedule_measurement",
        "The price-per-sqft question was already answered. Do not multiply a backsplash measurement by a countertop price. Ask to reschedule the measurement. Do not invent a total project estimate.",
    )
}

pub(crate) fn delayed_timing_entry(f: &FollowupFacts) -> CadenceShapeEntry {
    CadenceShapeEntry::stalled(
        "Check whether timing is still delayed",
        A::ProjectTiming,
        "explicit_delay",
        match f.explicit_customer_request.as_deref().filter(|r| !r.is_empty()) {
            Some(request) => format!(
                "They said the project was pushed back ({request}). Ask whether the timing has changed or you should leave it alone for now. Do not open with design, color, or material education."
            ),
            None => "They said the project was pushed back. Ask whether the timing has changed or you should leave it alone for now. Do not open with design, color, or material education.".to_string(),
        },
    )
}

pub(crate) fn material_type_entry(f: &FollowupFacts) -> CadenceShapeEntry {
    let types = f.material_type_preferences.join(" vs ");
    let still_comparing = f.comparing_quotes
        && re!(r"still comparing|comparing quotes|shop(ping)? around", "i").is_match(&format!(
            "{} {}",
            f.explicit_customer_request.as_deref().unwrap_or(""),
            f.decision_obstacle_evidence.as_deref().unwrap_or("")
        ));
    let types_or = if types.is_empty() {
        "more than one material type"
    } else {
        &types
    };
    let purpose = match f
        .known_package_interest
        .as_deref()
        .filter(|p| !p.is_empty())
    {
        Some(package) if f.material_type_preferences.len() > 1 => format!(
            "They showed interest in {package}. For the countertops, ask whether they would prefer {types}. Do not say they were already comparing unless customer evidence exists. Do not treat package catalog choices as prior customer comparison state."
        ),
        _ if still_comparing && f.uncertain_material_direction => format!(
            "They were still comparing {types_or} and the material is undecided. Ask whether they are leaning toward one now or still deciding. Do not treat either option as the current direction."
        ),
        _ => format!(
            "They are considering {types_or}. Ask which direction is still open. Do not introduce pricing unless budget or quote evidence exists. This is not a confirmed selection."
        ),
    };
    CadenceShapeEntry::stalled(
        "Confirm material type direction",
        A::MaterialPreference,
        if re!(r"quartz", "i").is_match(&types) && re!(r"soap", "i").is_match(&types) {
            "quartz_vs_soapstone"
        } else {
            "material_type_direction"
        },
        purpose,
    )
}

pub(crate) fn color_direction_entry(f: &FollowupFacts) -> CadenceShapeEntry {
    let colors = f.countertop_color_preferences.join(", ");
    CadenceShapeEntry::stalled(
        "Confirm color direction",
        A::DesignHelp,
        if re!(r"charcoal|dark green", "i").is_match(&colors) {
            "charcoal_or_dark_green"
        } else {
            "dark_color_direction"
        },
        format!(
            "They mentioned {}. Ask about that color direction. Do not introduce pricing unless budget or quote evidence exists.",
            if colors.is_empty() {
                "medium to dark colors"
            } else {
                &colors
            }
        ),
    )
}

pub(crate) fn package_interest_entry(f: &FollowupFacts) -> CadenceShapeEntry {
    CadenceShapeEntry::stalled(
        "Revisit the package they were interested in",
        A::PackageInterest,
        "package_interest",
        match f
            .known_package_interest
            .as_deref()
            .filter(|p| !p.is_empty())
        {
            Some(package) => format!(
                "They showed interest in {package}. Ask whether that package or a current equivalent is still relevant. Do not claim a historical special or package price is still available. Do not invent budget or design obstacles."
            ),
            None => "Ask about the package they were interested in. Do not invent obstacles."
                .to_string(),
        },
    )
}
