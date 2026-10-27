//! The contact-pressure block of the follow-up prompt
//! (`formatContactPressureBlock`).

use super::super::dates::{Instant, iso};
use super::super::facts::ExtractedMeasurement;
use super::super::js::{js_number, js_to_fixed};
use super::types::FollowupContactPressure;
use std::fmt::Write as _;

pub fn day_of(at: Option<Instant>) -> String {
    at.map_or_else(|| "none".to_string(), |at| iso(at)[..10].to_string())
}

pub fn minute_of(at: Option<Instant>) -> String {
    at.map_or_else(
        || "none".to_string(),
        |at| iso(at)[..16].replacen('T', " ", 1),
    )
}

pub const fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

pub fn or_none<T: std::fmt::Display>(value: Option<T>) -> String {
    value.map_or_else(|| "none".to_string(), |v| v.to_string())
}

pub fn number_or_none(value: Option<f64>) -> String {
    value.map_or_else(|| "none".to_string(), js_number)
}

pub fn list_or_none<T: std::fmt::Display>(items: &[T], separator: &str) -> String {
    if items.is_empty() {
        return "none".to_string();
    }
    items
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(separator)
}

fn measurement(value: Option<&ExtractedMeasurement>) -> String {
    value.map_or_else(
        || "none".to_string(),
        |m| format!("{} ({})", js_number(m.value), m.source),
    )
}

/// The calculated contact-pressure facts, one `key: value` per line.
#[allow(clippy::too_many_lines)]
pub fn format_contact_pressure_block(p: &FollowupContactPressure) -> String {
    let mut out = String::from("CONTACT PRESSURE (calculated — trust these numbers):\n");
    let mut line = |key: &str, value: String| {
        let _ = writeln!(out, "{key}: {value}");
    };
    line(
        "days_since_last_customer_reply",
        number_or_none(p.days_since_last_customer_reply),
    );
    line(
        "days_since_last_outbound",
        number_or_none(p.days_since_last_outbound),
    );
    line(
        "unanswered_sms_since_last_reply",
        js_number(p.unanswered_sms_since_last_reply),
    );
    line(
        "unanswered_email_since_last_reply",
        js_number(p.unanswered_email_since_last_reply),
    );
    line(
        "unanswered_total_since_last_reply",
        js_number(p.unanswered_total_since_last_reply),
    );
    line("outbound_last_7_days", js_number(p.outbound_last7_days));
    line("outbound_last_30_days", js_number(p.outbound_last30_days));
    line("outbound_last_90_days", js_number(p.outbound_last90_days));
    line(
        "average_gap_last_5_outbounds",
        p.average_gap_last5_outbounds.map_or_else(
            || "none".to_string(),
            |gap| format!("{} days", js_to_fixed(gap, 1)),
        ),
    );
    line(
        "customer_requested_followup_date",
        day_of(p.customer_requested_followup_at),
    );
    line("crm_hold_until", day_of(p.crm_hold_until));
    line(
        "last_customer_message_at",
        minute_of(p.last_customer_message_at),
    );
    line(
        "latest_inbound_event_at",
        minute_of(p.latest_inbound_event_at),
    );
    line(
        "latest_meaningful_customer_message_at",
        minute_of(p.latest_meaningful_customer_message_at),
    );
    line(
        "last_customer_message_summary",
        or_none(p.last_customer_message_summary.as_ref()),
    );
    line(
        "latest_terminal_status_at",
        minute_of(p.latest_terminal_status_at),
    );
    line(
        "customer_state",
        format!(
            "{} (legacy operational cadence label — prefer latest_customer_state + automation_contact_status for decisions)",
            p.customer_state
        ),
    );
    line(
        "customer_intent",
        format!(
            "{} (legacy summary — prefer latest_meaningful_customer_intent)",
            p.customer_intent
        ),
    );
    line(
        "global_contact_pressure",
        p.global_contact_pressure.to_string(),
    );
    line(
        "sms_contact_pressure",
        format!("{} ({})", p.sms_contact_pressure, p.sms_channel_state),
    );
    line(
        "email_contact_pressure",
        format!("{} ({})", p.email_contact_pressure, p.email_channel_state),
    );
    line(
        "phone_contact_pressure",
        format!("{} ({})", p.phone_contact_pressure, p.phone_channel_state),
    );
    line(
        "used_followup_angles",
        list_or_none(&p.used_followup_angles, ", "),
    );
    line("quote_requested", yes_no(p.quote_requested).into());
    line("quote_evidence", yes_no(p.quote_evidence).into());
    line("quote_created", yes_no(p.quote_created).into());
    line("quote_sent", yes_no(p.quote_sent).into());
    line(
        "quote_received_or_discussed",
        yes_no(p.quote_received_or_discussed).into(),
    );
    line("quote_evidence_source", p.quote_evidence_source.to_string());
    line(
        "quote_evidence_confidence",
        p.quote_evidence_confidence.to_string(),
    );
    line(
        "explicit_customer_request",
        or_none(p.explicit_customer_request.as_ref()),
    );
    line(
        "explicit_customer_request_kind",
        or_none(p.explicit_customer_request_kind),
    );
    line("needs_sink", yes_no(p.needs_sink).into());
    line("sink_required", yes_no(p.sink_required).into());
    line("sink_source", or_none(p.sink_source));
    line(
        "needs_sink_from_company",
        yes_no(p.needs_sink_from_company).into(),
    );
    line("backsplash_known", yes_no(p.backsplash_known).into());
    line("backsplash_type", p.backsplash_type.to_string());
    line(
        "stone_backsplash_required",
        p.stone_backsplash_required
            .map_or_else(|| "none".to_string(), |v| yes_no(v).into()),
    );
    line("backsplash_finish", p.backsplash_finish.to_string());
    line(
        "backsplash_material",
        or_none(p.backsplash_material.as_ref()),
    );
    line(
        "backsplash_description",
        or_none(p.backsplash_description.as_ref()),
    );
    line(
        "uncertain_material_direction",
        yes_no(p.uncertain_material_direction).into(),
    );
    line(
        "material_direction_status",
        p.material_direction_status.to_string(),
    );
    line("preferred_material", or_none(p.preferred_material.as_ref()));
    line(
        "did_not_find_suitable_option",
        yes_no(p.did_not_find_suitable_option).into(),
    );
    line(
        "quote_expiration_evidence",
        or_none(p.quote_expiration_evidence.as_ref()),
    );
    line("quote_expiration_date", day_of(p.quote_expiration_date));
    line("quote_type", or_none(p.quote_type));
    line("crm_deal_stage", or_none(p.crm_deal_stage.as_ref()));
    line(
        "operational_contact_state",
        p.operational_contact_state.to_string(),
    );
    line(
        "countertop_preferences",
        list_or_none(&p.countertop_preferences, ", "),
    );
    line(
        "cabinet_preferences",
        list_or_none(&p.cabinet_preferences, ", "),
    );
    line(
        "unfinished_next_step_kind",
        or_none(p.unfinished_next_step_kind),
    );
    line(
        "followup_starting_phase",
        p.followup_starting_phase.to_string(),
    );
    line(
        "timeline_evidence_source",
        format!(
            "{} ({})",
            p.timeline_evidence_source, p.timeline_evidence_confidence
        ),
    );
    line(
        "project_scope_source",
        format!(
            "{} ({})",
            p.project_scope_source, p.project_scope_confidence
        ),
    );
    line(
        "construction_delay_source",
        format!(
            "{} ({})",
            p.construction_delay_source, p.construction_delay_confidence
        ),
    );
    line(
        "financial_delay_source",
        format!(
            "{} ({})",
            p.financial_delay_source, p.financial_delay_confidence
        ),
    );
    line("budget_evidence", yes_no(p.budget_evidence).into());
    line(
        "budget_evidence_source",
        p.budget_evidence_source.to_string(),
    );
    line(
        "budget_evidence_confidence",
        p.budget_evidence_confidence.to_string(),
    );
    line(
        "followup_sequence_status",
        p.followup_sequence_status.to_string(),
    );
    line(
        "latest_meaningful_customer_intent",
        p.latest_meaningful_customer_intent.as_ref().map_or_else(
            || "none".to_string(),
            |intent| format!("{} ({})", intent.kind, intent.confidence),
        ),
    );
    line(
        "additional_customer_intents",
        list_or_none(
            &p.additional_customer_intents
                .iter()
                .map(|i| i.kind)
                .collect::<Vec<_>>(),
            ", ",
        ),
    );
    line(
        "customer_reply_handled",
        yes_no(p.customer_reply_handled).into(),
    );
    line(
        "customer_waiting_for_human_reply",
        yes_no(p.customer_waiting_for_human_reply).into(),
    );
    line(
        "last_conversational_rep_reply_at",
        minute_of(p.last_conversational_rep_reply_at),
    );
    line("sms_available", yes_no(p.sms_available).into());
    line("email_available", yes_no(p.email_available).into());
    line("phone_available", yes_no(p.phone_available).into());
    line("sms_permission", p.sms_permission.to_string());
    line("email_permission", p.email_permission.to_string());
    line("phone_permission", p.phone_permission.to_string());
    line("contact_resume_mode", p.contact_resume_mode.to_string());
    line(
        "automation_contact_status",
        p.automation_contact_status.to_string(),
    );
    line(
        "known_package_interest",
        or_none(p.known_package_interest.as_ref()),
    );
    line("decision_obstacle", p.decision_obstacle.to_string());
    line(
        "decision_obstacle_evidence",
        or_none(p.decision_obstacle_evidence.as_ref()),
    );
    line(
        "decision_criteria",
        list_or_none(&p.decision_criteria, ", "),
    );
    line("comparing_quotes", yes_no(p.comparing_quotes).into());
    line(
        "budget_target_approx",
        number_or_none(p.budget_target_approx),
    );
    line("price_context", p.price_context.to_string());
    line(
        "active_decision_factors",
        list_or_none(&p.active_decision_factors, ", "),
    );
    line("latest_customer_state", p.latest_customer_state.to_string());
    line(
        "active_unresolved_facts",
        list_or_none(&p.active_unresolved_facts, "; "),
    );
    line("project_details", list_or_none(&p.project_details, ", "));
    line("project_scale", p.project_scale.to_string());
    line("approx_unit_count", number_or_none(p.approx_unit_count));
    line("preferred_next_step", p.preferred_next_step.to_string());
    line(
        "preferred_next_step_source",
        p.preferred_next_step_source.to_string(),
    );
    line(
        "preferred_next_step_confidence",
        p.preferred_next_step_confidence.to_string(),
    );
    line(
        "customer_requested_contact_method",
        or_none(p.customer_requested_contact_method),
    );
    line(
        "salesperson_planned_contact_method",
        or_none(p.salesperson_planned_contact_method),
    );
    line("manual_action_required", or_none(p.manual_action_required));
    line("manual_action_due_at", day_of(p.manual_action_due_at));
    line(
        "manual_action_status",
        if p.manual_action_required.is_none() {
            "none".into()
        } else {
            p.manual_action_status.to_string()
        },
    );
    line("appointment_companion", or_none(p.appointment_companion));
    line(
        "completed_estimate_satisfied",
        yes_no(p.completed_estimate_satisfied).into(),
    );
    line(
        "customer_requested_contact_window",
        if p.customer_requested_contact_window_start.is_some()
            || p.customer_requested_contact_window_end.is_some()
        {
            format!(
                "{}–{}",
                day_of(p.customer_requested_contact_window_start),
                day_of(p.customer_requested_contact_window_end)
            )
        } else {
            "none".into()
        },
    );
    line(
        "planned_customer_event",
        p.planned_customer_event_type.map_or_else(
            || "none".to_string(),
            |kind| {
                format!(
                    "{kind} {} {}–{} ({})",
                    p.planned_customer_event_date_label
                        .as_deref()
                        .unwrap_or("unspecified date"),
                    day_of(p.planned_customer_event_window_start),
                    day_of(p.planned_customer_event_window_end),
                    or_none(p.planned_customer_event_confidence)
                )
            },
        ),
    );
    line("visual_style", or_none(p.visual_style));
    line("showroom_interest", yes_no(p.showroom_interest).into());
    line("information_density", p.information_density.to_string());
    line(
        "customer_message_semantic_type",
        p.customer_message_semantic_type.to_string(),
    );
    line(
        "previous_sequence_stop_reason",
        or_none(p.previous_sequence_stop_reason),
    );
    line(
        "current_sequence_stop_reason",
        or_none(p.current_sequence_stop_reason),
    );
    line("latest_outbound_type", or_none(p.latest_outbound_type));
    line("relationship_age", p.relationship_age.to_string());
    line("engagement_state", p.engagement_state.to_string());
    line("deal_state", p.deal_state.to_string());
    line("target_deal_id", or_none(p.target_deal_id));
    line("target_deal_status", or_none(p.target_deal_status.as_ref()));
    line(
        "other_deals",
        list_or_none(
            &p.other_deals
                .iter()
                .map(|deal| deal.status_label.clone())
                .collect::<Vec<_>>(),
            ", ",
        ),
    );
    line(
        "multi_project_customer",
        yes_no(p.multi_project_customer).into(),
    );
    line("deal_product_scope", p.deal_product_scope.to_string());
    line(
        "historical_material_preferences",
        list_or_none(&p.historical_material_preferences, ", "),
    );
    line(
        "material_type_preferences",
        list_or_none(&p.material_type_preferences, ", "),
    );
    line(
        "countertop_color_preferences",
        list_or_none(&p.countertop_color_preferences, ", "),
    );
    line(
        "material_preferences",
        list_or_none(&p.material_preferences, ", "),
    );
    line("favorite_material", or_none(p.favorite_material.as_ref()));
    line(
        "current_confirmed_material",
        or_none(p.current_confirmed_material.as_ref()),
    );
    line(
        "material_selection_history",
        yes_no(p.material_selection_history).into(),
    );
    line(
        "material_selection_evidence",
        yes_no(p.material_selection_evidence).into(),
    );
    line(
        "material_selection_evidence_source",
        p.material_selection_evidence_source.to_string(),
    );
    line("deal_stage_hold", yes_no(p.deal_stage_hold).into());
    line("hard_contact_hold_until", day_of(p.hard_contact_hold_until));
    line("activity_actor", or_none(p.activity_actor));
    line("activity_object", or_none(p.activity_object.as_ref()));
    line(
        "unfinished_possibly_satisfied",
        yes_no(p.unfinished_possibly_satisfied).into(),
    );
    line(
        "pricing_change_valid_until",
        day_of(p.pricing_change_valid_until),
    );
    line(
        "customer_replied_after_last_outbound",
        p.customer_replied_after_last_outbound.to_string(),
    );
    line("latest_inbound_type", or_none(p.latest_inbound_type));
    line(
        "latest_inbound_sender",
        or_none(p.latest_inbound_sender.as_ref()),
    );
    line(
        "matched_customer_identity",
        yes_no(p.matched_customer_identity).into(),
    );
    line(
        "reply_classification_confidence",
        or_none(p.reply_classification_confidence),
    );
    line(
        "reply_classification_reason",
        or_none(p.reply_classification_reason.as_ref()),
    );
    line(
        "original_timeline_evidence",
        or_none(p.original_timeline_evidence.as_ref()),
    );
    line("known_material", or_none(p.known_material.as_ref()));
    line(
        "quoted_material_context",
        or_none(p.quoted_material_context.as_ref()),
    );
    line("known_scope", or_none(p.known_scope.as_ref()));
    line(
        "countertop_measurements_status",
        or_none(p.countertop_measurements_status),
    );
    line("project_size_raw", number_or_none(p.project_size_raw));
    line("countertop_sqft", measurement(p.countertop_sqft.as_ref()));
    line("backsplash_sqft", measurement(p.backsplash_sqft.as_ref()));
    line("island_sqft", measurement(p.island_sqft.as_ref()));
    line("vanity_sqft", measurement(p.vanity_sqft.as_ref()));
    line(
        "total_project_sqft",
        measurement(p.total_project_sqft.as_ref()),
    );
    line("unknown_sqft", measurement(p.unknown_sqft.as_ref()));
    line(
        "measurement_raw_text",
        or_none(p.measurement_raw_text.as_ref()),
    );
    line("future_volume_sqft", number_or_none(p.future_volume_sqft));
    line(
        "project_dependencies",
        list_or_none(
            &p.project_dependencies
                .iter()
                .map(|d| format!("{}:{}", d.kind, d.status))
                .collect::<Vec<_>>(),
            ", ",
        ),
    );
    line("never_tried_sms", yes_no(p.never_tried_sms).into());
    line("estimated_ready_at", day_of(p.estimated_ready_at));
    line(
        "estimated_ready_at_source",
        p.estimated_ready_at_source.to_string(),
    );
    line(
        "estimated_ready_at_confidence",
        p.estimated_ready_at_confidence.to_string(),
    );
    line(
        "estimated_ready_at_precision",
        or_none(p.estimated_ready_at_precision),
    );
    line(
        "estimated_ready_window_start",
        day_of(p.estimated_ready_window_start),
    );
    line(
        "estimated_ready_window_end",
        day_of(p.estimated_ready_window_end),
    );
    line("timeline_position", or_none(p.timeline_position));
    line(
        "existing_countertop_material",
        or_none(p.existing_countertop_material.as_ref()),
    );
    line(
        "existing_countertop_removal",
        yes_no(p.existing_countertop_removal).into(),
    );
    line("removal_required", yes_no(p.removal_required).into());
    line("removal_component", or_none(p.removal_component));
    line(
        "customer_project_components",
        list_or_none(&p.customer_project_components, ", "),
    );
    line(
        "unsupported_requested_components",
        list_or_none(&p.unsupported_requested_components, ", "),
    );
    line(
        "historical_quote_expiration_warning",
        yes_no(p.historical_quote_expiration_warning).into(),
    );
    line("historical_task", yes_no(p.historical_task).into());
    line(
        "alternative_material_offered",
        or_none(p.alternative_material_offered.as_ref()),
    );
    line(
        "installation_requested",
        yes_no(p.installation_requested).into(),
    );
    line(
        "unfinished_next_step",
        or_none(p.unfinished_next_step.as_ref()),
    );
    line(
        "construction_dependency_evidence",
        or_none(p.construction_dependency_evidence.as_ref()),
    );
    line(
        "communication_history_status",
        p.communication_history_status.to_string(),
    );
    line("relationship_state", p.relationship_state.to_string());
    line("hold_reason_type", or_none(p.hold_reason_type));
    line("hold_reason_source", or_none(p.hold_reason_source));
    line(
        "hold_reason_evidence",
        or_none(p.hold_reason_evidence.as_ref()),
    );
    line("hold_reason_at", day_of(p.hold_reason_at));
    line("hold_reason_confidence", or_none(p.hold_reason_confidence));
    line("hold_source_type", or_none(p.hold_source_type));
    line("sms_channel_status", p.sms_channel_status.to_string());
    line("email_channel_status", p.email_channel_status.to_string());
    line("phone_channel_status", p.phone_channel_status.to_string());
    line(
        "last_conversation_reply",
        or_none(p.last_conversation_reply.as_ref()),
    );
    line(
        "last_customer_originated_information",
        or_none(p.last_customer_originated_information.as_ref()),
    );
    let _ = write!(
        out,
        "has_relationship_history: {}",
        yes_no(p.has_relationship_history)
    );
    out
}
