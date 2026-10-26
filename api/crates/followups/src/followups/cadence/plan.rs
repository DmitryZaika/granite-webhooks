//! Which steps a sequence takes, and when: the shape of the plan for this
//! customer (`skipUsedAngles`, `fillShape`, `conversationPlan` and the step
//! objectives of `smsFlowAiCadence.ts`).

use super::super::dates::Instant;
use super::super::facts::extract::{angle_has_required_evidence, angle_specificity_score};
use super::super::facts::quotes::quote_evidence_is_verified;
use super::super::facts::{
    DealProductScope, DecisionFactor, DecisionObstacle, ExplicitCustomerRequestKind, FollowupFacts,
    LatestCustomerState, ManualActionStatus, MeasurementsStatus, PreferredNextStep, ProjectScale,
    UnfinishedNextStepKind,
};
use super::super::intelligence::eligibility::{
    ObjectivePlanProbe, eligibility_from_facts, is_objective_supported, objective_fingerprint,
};
use super::super::intent::MeaningfulCustomerIntentType as I;
use super::super::pressure::channels::{email_channel_is_usable, sms_channel_is_usable};
use super::super::pressure::state::is_gentle_post_hold;
use super::super::pressure::{
    ChannelAttemptState, CommunicationHistoryStatus, CustomerFollowupState, CustomerIntent,
    FollowupAngle as A, FollowupContactPressure, FollowupPhase, HoldReasonType, RelationshipAge,
    RelationshipState, SmsChannelStatus,
};
use super::super::send_window::FollowupChannel;
use super::entries::{
    agreed_specials_entry, budget_obstacle_entry, color_direction_entry, company_owed_quote_entry,
    comparison_support_entry, customer_measurements_entry, customer_request_entry,
    delayed_timing_entry, dormant_project_status_entry, facts_from_pressure,
    has_scheduling_evidence, in_home_estimate_entry, material_fit_entry, material_preference_entry,
    material_type_entry, multi_unit_entry, package_interest_entry, preferred_next_step_entry,
    previous_stone_selection_entry, project_context_entry, project_dependency_entry,
    quote_status_entry, schedule_measurement_entry, showroom_appointment_entry,
};
use super::ramp::{
    AI_MAX_DELAY_MINUTES, CadenceSlot, DECAY_GAPS, DELAYED_REACTIVATION_GAPS,
    ESTABLISHED_CONVERSATION_GAPS, LONG_TERM_NURTURE_GAPS, MAX_CADENCE_GAP_DAYS, MINUTES_PER_DAY,
    MONTHLY_RAMP_POSITION, NEW_CONVERSATION_GAPS, POST_HOLD_CHANNELS, POST_HOLD_GAPS,
    WAITING_REACTIVATION_GAPS, cap_cadence_gap_days, classify_ai_cadence_mode,
    delay_label_from_days, delay_label_from_minutes, interleave_emails,
    is_established_conversation_customer, never_tried_sms_yet, offsets_from_gaps, phase_for_step,
    ramp_position, sms_offsets_from_ramp, untested_sms_offsets, untested_sms_slots,
};
use super::shapes::{
    EARLY_SHAPE, EXISTING_OLD_LEAD_SHAPE, MONTHLY_SHAPE, NURTURE_PURPOSE, POST_HOLD_SHAPE,
    STALLED_SHAPE, ShapeDef, UNREACHED_SHAPE,
};
use super::types::{
    AiCadenceMode, CadenceShapeEntry, CopySemanticObjective, FollowupSemanticObjective,
    FollowupSituation, PlannedAiCadenceStep,
};
use crate::re;

fn key_of(entry: &CadenceShapeEntry) -> String {
    format!(
        "{}:{}",
        entry.angle,
        entry.subangle.as_deref().unwrap_or("")
    )
}

fn probe_of(entry: &CadenceShapeEntry) -> ObjectivePlanProbe<'_> {
    ObjectivePlanProbe {
        angle: Some(entry.angle.as_str()),
        subangle: entry.subangle.as_deref(),
        goal: Some(&entry.goal),
    }
}

const fn is_backoff_angle(angle: A) -> bool {
    matches!(angle, A::CloseLoop | A::LongTermNurture)
}

fn close_loop_allowed_for_pressure(pressure: Option<&FollowupContactPressure>) -> bool {
    let Some(p) = pressure else {
        return true;
    };
    if p.latest_customer_state == LatestCustomerState::FutureProject
        || matches!(
            p.hold_reason_type,
            Some(HoldReasonType::ConstructionDependency | HoldReasonType::ProjectTimeline)
        )
    {
        return false;
    }
    let anchor = p.sequence_generated_at.unwrap_or_else(chrono::Utc::now);
    !p.estimated_ready_at
        .is_some_and(|ready| (ready - anchor).num_milliseconds() > 120 * 24 * 60 * 60 * 1000)
}

fn intent_kinds(p: Option<&FollowupContactPressure>) -> (Option<I>, Vec<I>) {
    p.map_or((None, Vec::new()), |p| {
        (
            p.latest_meaningful_customer_intent.as_ref().map(|i| i.kind),
            p.additional_customer_intents
                .iter()
                .map(|i| i.kind)
                .collect(),
        )
    })
}

fn has_intent(p: Option<&FollowupContactPressure>, kind: I) -> bool {
    let (latest, extra) = intent_kinds(p);
    latest == Some(kind) || extra.contains(&kind)
}

fn filled(value: Option<&String>) -> bool {
    value.is_some_and(|v| !v.is_empty())
}

fn has_specific_customer_facts(
    f: &FollowupFacts,
    pressure: Option<&FollowupContactPressure>,
) -> bool {
    has_intent(pressure, I::Delayed)
        || filled(f.known_material.as_ref())
        || !f.material_preferences.is_empty()
        || filled(f.unfinished_next_step.as_ref())
        || filled(f.original_timeline_evidence.as_ref())
        || f.quote_evidence
        || f.quote_requested
        || filled(f.explicit_customer_request.as_ref())
        || filled(f.construction_dependency_evidence.as_ref())
        || filled(f.budget_obstacle_evidence.as_ref())
        || filled(f.decision_obstacle_evidence.as_ref())
        || f.comparing_quotes
        || filled(f.known_scope.as_ref())
        || f.needs_sink
        || f.backsplash_known
        || filled(f.known_package_interest.as_ref())
        || f.material_selection_history
        || filled(f.favorite_material.as_ref())
        || !f.material_type_preferences.is_empty()
        || !f.cabinet_preferences.is_empty()
        || f.estimated_ready_at.is_some()
}

fn verified_quote(f: &FollowupFacts) -> bool {
    quote_evidence_is_verified(
        f.quote_evidence,
        f.quote_evidence_source,
        f.quote_evidence_confidence,
    )
}

fn shape_entries(shape: &[ShapeDef]) -> Vec<CadenceShapeEntry> {
    shape.iter().map(CadenceShapeEntry::from).collect()
}

/// Pads the picked steps to the shape's length with steps the facts support
/// (no two asking the same thing), keeping one close-loop when allowed.
fn fill_shape(
    mut picked: Vec<CadenceShapeEntry>,
    shape: &[CadenceShapeEntry],
    pressure: Option<&FollowupContactPressure>,
) -> Vec<CadenceShapeEntry> {
    let fallback = shape
        .iter()
        .find(|entry| entry.angle == A::LongTermNurture)
        .cloned()
        .unwrap_or_else(|| {
            CadenceShapeEntry::new(
                FollowupSituation::StalledConversation,
                "Light nurture",
                A::LongTermNurture,
                None,
                NURTURE_PURPOSE,
            )
        });
    let Some(p) = pressure else {
        while picked.len() < shape.len() {
            picked.push(fallback.clone());
        }
        picked.truncate(shape.len());
        return picked;
    };
    let facts = eligibility_from_facts(&facts_from_pressure(Some(p)));
    let mut used_keys: Vec<String> = picked.iter().map(key_of).collect();
    let mut used_fingerprints: Vec<String> = picked
        .iter()
        .map(|entry| {
            objective_fingerprint(
                planner_copy_objective(entry),
                &facts,
                Some(&probe_of(entry)),
            )
        })
        .collect();
    for entry in shape {
        if picked.len() >= shape.len() {
            break;
        }
        if entry.angle == A::CloseLoop {
            continue;
        }
        let key = key_of(entry);
        if used_keys.contains(&key) {
            continue;
        }
        let objective = planner_copy_objective(entry);
        if !is_objective_supported(objective, &facts, Some(&probe_of(entry))) {
            continue;
        }
        let fingerprint = objective_fingerprint(objective, &facts, Some(&probe_of(entry)));
        if used_fingerprints.contains(&fingerprint) {
            continue;
        }
        picked.push(entry.clone());
        used_keys.push(key);
        used_fingerprints.push(fingerprint);
    }
    if let Some(close) = shape.iter().find(|entry| entry.angle == A::CloseLoop) {
        if !picked.iter().any(|entry| entry.angle == A::CloseLoop)
            && close_loop_allowed_for_pressure(pressure)
        {
            picked.push(close.clone());
        }
    }
    picked.truncate(shape.len());
    picked
}

/// The shape for this customer: the steps their facts call for first, then
/// the template, without angles already used.
#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
pub fn skip_used_angles(
    shape_defs: &[ShapeDef],
    used: &[A],
    pressure: Option<&FollowupContactPressure>,
) -> Vec<CadenceShapeEntry> {
    let shape = shape_entries(shape_defs);
    let f = facts_from_pressure(pressure);
    if !has_specific_customer_facts(&f, pressure) {
        let remaining: Vec<CadenceShapeEntry> = shape
            .iter()
            .filter(|entry| {
                let angle = entry.angle;
                if angle == A::CloseLoop && !close_loop_allowed_for_pressure(pressure) {
                    return false;
                }
                if used.contains(&angle)
                    && angle != A::LongTermNurture
                    && angle != A::HelpfulInformation
                {
                    return false;
                }
                if angle == A::OriginalTimeline && !filled(f.original_timeline_evidence.as_ref()) {
                    return false;
                }
                if matches!(angle, A::QuoteStatus | A::QuoteReview)
                    && entry.subangle.as_deref() != Some("promised_quote_delivery")
                    && !verified_quote(&f)
                {
                    return false;
                }
                if angle == A::ProjectDependency
                    && !filled(f.construction_dependency_evidence.as_ref())
                {
                    return false;
                }
                if angle == A::BudgetObstacle && !filled(f.budget_obstacle_evidence.as_ref()) {
                    return false;
                }
                if angle == A::DecisionObstacle && !filled(f.decision_obstacle_evidence.as_ref()) {
                    return false;
                }
                if pressure.is_some_and(|p| {
                    p.communication_history_status == CommunicationHistoryStatus::TrulyNew
                }) && (f.deal_product_scope != DealProductScope::Unknown
                    || filled(f.known_scope.as_ref()))
                    && (angle == A::Reactivation
                        || (angle == A::ProjectScope
                            && entry.subangle.as_deref() != Some("multi_unit_readiness")))
                {
                    return false;
                }
                if f.latest_customer_state == LatestCustomerState::TemplateApproved
                    && matches!(
                        angle,
                        A::MaterialPreference | A::QuoteStatus | A::QuoteReview
                    )
                {
                    return false;
                }
                !(f.deal_product_scope == DealProductScope::Cabinets
                    && matches!(angle, A::HelpfulInformation | A::DesignHelp))
            })
            .cloned()
            .collect();
        let picked = if remaining.is_empty() {
            shape.clone()
        } else {
            remaining
        };
        let Some(p) = pressure else {
            return fill_shape(picked, &shape, None);
        };
        let eligibility = eligibility_from_facts(&f);
        let supported: Vec<CadenceShapeEntry> = picked
            .iter()
            .filter(|entry| {
                is_objective_supported(
                    planner_copy_objective(entry),
                    &eligibility,
                    Some(&probe_of(entry)),
                )
            })
            .cloned()
            .collect();
        return fill_shape(
            if supported.is_empty() {
                picked
            } else {
                supported
            },
            &shape,
            Some(p),
        );
    }

    let mut preferred: Vec<CadenceShapeEntry> = Vec::new();
    if pressure.is_some_and(|p| {
        p.manual_action_required == Some(super::super::facts::ManualActionRequired::SalesTask)
            && p.manual_action_status == ManualActionStatus::Pending
    }) && f.manual_action_tasks.iter().any(|task| {
        re!(r"SEND_.*QUOTE|SEND_QUOTE|SEND_UPDATED_QUOTE|SEND_REVISED_QUOTE").is_match(task)
    }) {
        preferred.push(company_owed_quote_entry(&f));
    }
    if f.unfinished_next_step_kind == Some(UnfinishedNextStepKind::ShowroomAppointment) {
        preferred.push(showroom_appointment_entry(&f));
    }
    if f.unfinished_next_step_kind == Some(UnfinishedNextStepKind::InHomeEstimate) {
        preferred.push(in_home_estimate_entry(&f));
    }
    if filled(f.construction_dependency_evidence.as_ref()) || !f.project_dependencies.is_empty() {
        preferred.push(project_dependency_entry(&f));
    }
    if f.unfinished_next_step_kind == Some(UnfinishedNextStepKind::ScheduleMeasurement) {
        preferred.push(schedule_measurement_entry());
    }
    if f.unfinished_next_step_kind == Some(UnfinishedNextStepKind::CustomerMeasurements) {
        preferred.push(customer_measurements_entry(&f));
    }
    let measurement_facts = f
        .active_unresolved_facts
        .iter()
        .any(|item| re!(r"measurement|dimension|square footage", "i").is_match(item));
    if f.countertop_measurements_status == Some(MeasurementsStatus::Needed) || measurement_facts {
        preferred.push(customer_measurements_entry(&f));
    }
    if let Some(alternative) = f
        .alternative_material_offered
        .as_deref()
        .filter(|a| !a.is_empty())
    {
        preferred.push(CadenceShapeEntry::stalled(
            "Resolve the remnant alternative",
            A::MaterialPreference,
            "remnant_alternative",
            format!(
                "A salesperson offered {alternative} as an alternative to the requested remnant/material. Ask whether that alternative works. Do not ask generic quartz or granite direction questions."
            ),
        ));
    }
    if f.preferred_next_step == PreferredNextStep::SendCurrentSpecials {
        preferred.push(agreed_specials_entry());
    }
    if f.decision_obstacle == DecisionObstacle::MaterialFit || f.did_not_find_suitable_option {
        preferred.push(material_fit_entry(&f));
    }
    let delayed_intent = has_intent(pressure, I::Delayed);
    if delayed_intent {
        preferred.push(delayed_timing_entry(&f));
    }
    if (filled(f.explicit_customer_request.as_ref()) || f.quote_requested)
        && !f.completed_estimate_satisfied
    {
        preferred.push(customer_request_entry(&f));
    }
    if matches!(
        f.explicit_customer_request_kind,
        Some(
            ExplicitCustomerRequestKind::FabricationRequest
                | ExplicitCustomerRequestKind::ProductAvailability
        )
    ) {
        preferred.push(customer_request_entry(&f));
    }
    if f.project_scale == ProjectScale::MultiUnit {
        preferred.push(multi_unit_entry(&f));
    }
    if filled(f.budget_obstacle_evidence.as_ref())
        || f.budget_target_approx.is_some()
        || f.active_decision_factors.contains(&DecisionFactor::Budget)
    {
        preferred.push(budget_obstacle_entry(&f));
    }
    if f.comparing_quotes
        || f.active_decision_factors
            .contains(&DecisionFactor::PriceComparison)
    {
        preferred.push(comparison_support_entry());
    }
    if filled(f.known_package_interest.as_ref()) {
        preferred.push(package_interest_entry(&f));
    }
    if f.latest_customer_state == LatestCustomerState::TemplateApproved {
        preferred.push(preferred_next_step_entry(&f));
    } else if f.material_type_preferences.len() > 1
        && f.material_preferences.is_empty()
        && !filled(f.favorite_material.as_ref())
    {
        preferred.push(material_type_entry(&f));
    } else if filled(f.known_material.as_ref())
        || !f.material_preferences.is_empty()
        || filled(f.favorite_material.as_ref())
    {
        preferred.push(material_preference_entry(&f));
    }
    if !f.countertop_color_preferences.is_empty() {
        preferred.push(color_direction_entry(&f));
    }
    if f.material_selection_history && f.material_preferences.is_empty() {
        preferred.push(previous_stone_selection_entry());
    }
    if filled(f.known_scope.as_ref())
        && (f.needs_sink || f.backsplash_known || f.uncertain_material_direction)
    {
        preferred.push(project_context_entry(&f));
    }
    if verified_quote(&f) && f.latest_customer_state != LatestCustomerState::TemplateApproved {
        preferred.push(quote_status_entry(&f));
        if f.deal_product_scope == DealProductScope::Cabinets {
            preferred.push(preferred_next_step_entry(&f));
        }
    } else if f.comparing_quotes || has_intent(pressure, I::ComparingQuotes) {
        preferred.push(comparison_support_entry());
    }
    let preferred_keys: Vec<String> = preferred.iter().map(key_of).collect();
    let eligibility = eligibility_from_facts(&f);
    let mut seen: Vec<String> = Vec::new();
    let mut unique: Vec<CadenceShapeEntry> = Vec::new();
    for entry in preferred.iter().chain(&shape) {
        let key = key_of(entry);
        if seen.contains(&key)
            || (entry.angle == A::CloseLoop && !close_loop_allowed_for_pressure(pressure))
            || !angle_has_required_evidence(entry.angle.as_str(), &f)
            || !is_objective_supported(
                planner_copy_objective(entry),
                &eligibility,
                Some(&probe_of(entry)),
            )
            || (used.contains(&entry.angle)
                && !preferred_keys.contains(&key)
                && entry.angle != A::LongTermNurture
                && entry.angle != A::HelpfulInformation)
        {
            continue;
        }
        seen.push(key);
        unique.push(entry.clone());
    }
    let primary: Vec<&CadenceShapeEntry> = unique
        .iter()
        .filter(|e| !is_backoff_angle(e.angle))
        .collect();
    let backoff: Vec<&CadenceShapeEntry> = unique
        .iter()
        .filter(|e| is_backoff_angle(e.angle))
        .collect();
    let by = |keep: &dyn Fn(&CadenceShapeEntry) -> bool| {
        primary
            .iter()
            .copied()
            .filter(|e| keep(e))
            .collect::<Vec<_>>()
    };
    let request = by(&|e| e.angle == A::EstimateOffer);
    let hold = by(&|e| e.angle == A::HoldReactivation);
    let education = by(&|e| e.angle == A::HelpfulInformation);
    let mut other = by(&|e| {
        !matches!(
            e.angle,
            A::EstimateOffer | A::HoldReactivation | A::HelpfulInformation
        )
    });
    super::super::js::js_sort_by(&mut other, |left, right| {
        f64::from(angle_specificity_score(right.angle.as_str(), &f))
            - f64::from(angle_specificity_score(left.angle.as_str(), &f))
    });
    use ExplicitCustomerRequestKind as K;
    let actionable_request = filled(f.explicit_customer_request.as_ref())
        || f.quote_requested
        || matches!(
            f.unfinished_next_step_kind,
            Some(
                UnfinishedNextStepKind::InHomeEstimate
                    | UnfinishedNextStepKind::ScheduleMeasurement
                    | UnfinishedNextStepKind::CustomerMeasurements
            )
        )
        || f.countertop_measurements_status == Some(MeasurementsStatus::Needed)
        || measurement_facts
        || matches!(
            f.explicit_customer_request_kind,
            Some(
                K::EstimatePricing
                    | K::SquareFootage
                    | K::OnsiteQuote
                    | K::QuoteRequest
                    | K::FabricationRequest
                    | K::ProductAvailability
            )
        );
    let has_obstacle = filled(f.budget_obstacle_evidence.as_ref())
        || f.comparing_quotes
        || f.active_decision_factors.contains(&DecisionFactor::Budget)
        || f.active_decision_factors
            .contains(&DecisionFactor::PriceComparison);
    let obstacle_angle = |e: &CadenceShapeEntry| {
        matches!(
            e.angle,
            A::BudgetObstacle | A::ComparisonSupport | A::DecisionObstacle
        )
    };
    let obstacles = by(&obstacle_angle);
    let sub = |e: &CadenceShapeEntry, value: &str| e.subangle.as_deref() == Some(value);
    let specials = by(&|e| sub(e, "send_current_specials"));
    let material_fit = by(&|e| sub(e, "unresolved_material_fit"));
    let other_without_ranked: Vec<&CadenceShapeEntry> = other
        .iter()
        .copied()
        .filter(|e| {
            !obstacle_angle(e)
                && !sub(e, "send_current_specials")
                && !sub(e, "unresolved_material_fit")
        })
        .collect();
    let post_hold = uses_post_hold_cadence(pressure);
    let promised_quote = by(&|e| sub(e, "promised_quote_delivery"));
    let groups: Vec<&Vec<&CadenceShapeEntry>> = if delayed_intent {
        vec![
            &promised_quote,
            &hold,
            &other_without_ranked,
            &request,
            &obstacles,
            &specials,
            &material_fit,
            &education,
        ]
    } else if actionable_request {
        vec![
            &promised_quote,
            &specials,
            &material_fit,
            &request,
            &obstacles,
            &hold,
            &other_without_ranked,
            &education,
        ]
    } else if has_obstacle {
        vec![
            &promised_quote,
            &specials,
            &material_fit,
            &obstacles,
            &hold,
            &request,
            &other_without_ranked,
            &education,
        ]
    } else if post_hold {
        vec![
            &promised_quote,
            &specials,
            &material_fit,
            &hold,
            &request,
            &other_without_ranked,
            &education,
        ]
    } else {
        vec![
            &promised_quote,
            &specials,
            &material_fit,
            &request,
            &other_without_ranked,
            &hold,
            &education,
        ]
    };
    let allow_schedule = has_scheduling_evidence(&f, pressure);
    let remapped: Vec<CadenceShapeEntry> = groups
        .into_iter()
        .flatten()
        .copied()
        .chain(backoff)
        .map(|entry| {
            if entry.angle == A::HelpfulInformation && f.material_selection_history {
                return previous_stone_selection_entry();
            }
            if (entry.angle == A::HelpfulInformation || sub(entry, "quartz_vs_granite_maintenance"))
                && (f.project_scale == ProjectScale::MultiUnit
                    || f.deal_product_scope == DealProductScope::Cabinets)
            {
                return if f.project_scale == ProjectScale::MultiUnit {
                    multi_unit_entry(&f)
                } else if verified_quote(&f) {
                    quote_status_entry(&f)
                } else {
                    preferred_next_step_entry(&f)
                };
            }
            if entry.angle == A::ProjectStatus && !allow_schedule {
                return dormant_project_status_entry(&f);
            }
            entry.clone()
        })
        .collect();
    fill_shape(remapped, &shape, pressure)
}

pub fn uses_post_hold_cadence(pressure: Option<&FollowupContactPressure>) -> bool {
    let Some(p) = pressure else {
        return false;
    };
    p.customer_state == CustomerFollowupState::Hold
        || (p.crm_hold_until.is_some()
            && (is_gentle_post_hold(p.hold_reason_type) || p.days_since_last_outbound.is_none()))
        || p.communication_history_status
            == CommunicationHistoryStatus::ExistingCustomerNoDigitalHistory
        || (p.relationship_state == RelationshipState::ExistingOldLead
            && p.days_since_last_outbound.is_none())
}

fn is_heavy_post_hold_pressure(pressure: Option<&FollowupContactPressure>) -> bool {
    pressure.is_some_and(|p| {
        p.unanswered_total_since_last_reply >= 4.0
            && p.last_customer_message_at.is_none()
            && (matches!(
                p.relationship_age,
                RelationshipAge::Existing | RelationshipAge::Old
            ) || matches!(
                p.relationship_state,
                RelationshipState::ExistingOldLead | RelationshipState::ExistingActive
            ))
    })
}

const FUTURE_PROJECT_GAPS: [f64; 5] = [0.0, 30.0, 30.0, 30.0, 30.0];

fn post_hold_gaps(pressure: Option<&FollowupContactPressure>) -> Vec<f64> {
    if pressure.is_some_and(|p| p.estimated_ready_at.is_some()) {
        return FUTURE_PROJECT_GAPS.to_vec();
    }
    if is_heavy_post_hold_pressure(pressure) {
        return vec![0.0, 12.0, 21.0, 30.0, 45.0, 60.0, 90.0];
    }
    POST_HOLD_GAPS.to_vec()
}

fn post_hold_slots(
    mix_sms_and_email: bool,
    fallback: FollowupChannel,
    pressure: Option<&FollowupContactPressure>,
) -> Vec<CadenceSlot> {
    offsets_from_gaps(&post_hold_gaps(pressure))
        .into_iter()
        .enumerate()
        .map(|(index, offset_days)| CadenceSlot {
            offset_days,
            channel: if mix_sms_and_email {
                POST_HOLD_CHANNELS.get(index).copied().unwrap_or(fallback)
            } else {
                fallback
            },
        })
        .collect()
}

fn is_fresh_unanswered_lead(pressure: Option<&FollowupContactPressure>) -> bool {
    let Some(p) = pressure else {
        return false;
    };
    if p.communication_history_status != CommunicationHistoryStatus::TrulyNew
        || p.last_customer_message_at.is_some()
        || p.relationship_state != RelationshipState::None
        || p.crm_hold_until.is_some()
        || p.deal_stage_hold
        || p.unanswered_total_since_last_reply < 1.0
        || p.unanswered_total_since_last_reply > 2.0
        || matches!(
            p.customer_state,
            CustomerFollowupState::Hold
                | CustomerFollowupState::Dormant
                | CustomerFollowupState::QuoteGhost
                | CustomerFollowupState::Waiting
        )
    {
        return false;
    }
    !matches!(
        p.latest_meaningful_customer_intent.as_ref().map(|i| i.kind),
        Some(
            I::Waiting
                | I::Delayed
                | I::NotNow
                | I::FutureProject
                | I::BudgetConcern
                | I::ComparingQuotes
        )
    )
}

fn known_project_phrase(p: &FollowupContactPressure) -> String {
    let known = super::super::js::trim(p.known_scope.as_deref().unwrap_or(""));
    if known.is_empty() {
        return "the project".into();
    }
    if re!(r"kitchen", "i").is_match(known) {
        return "the kitchen renovation".into();
    }
    format!("the {known} project")
}

fn fresh_unanswered_lead_shape(p: &FollowupContactPressure) -> Vec<CadenceShapeEntry> {
    let project = known_project_phrase(p);
    let scope = p.known_scope.as_deref().unwrap_or("");
    let no_kitchen_vs_bath = if re!(r"kitchen", "i").is_match(scope) {
        " The project is already a kitchen renovation. Never ask kitchen vs bathroom vs other."
            .to_string()
    } else if !scope.is_empty() {
        format!(" Use the known scope ({scope}) naturally. Never ask kitchen vs bathroom vs other.")
    } else {
        String::new()
    };
    let no_wait_copy = " Do not assume they are not ready, paused, or waiting. Do not say “whenever your renovation is ready”, “when the timing is right”, “has the timing changed”, or “restart the conversation”. This is a new unanswered lead, not a reactivation or dormant lead.";
    let entry = |situation, goal: &str, angle, subangle: &str, purpose: String| {
        CadenceShapeEntry::new(situation, goal, angle, Some(subangle), purpose)
    };
    use FollowupSituation::{SecondFollowup, StalledConversation};
    vec![
        entry(
            SecondFollowup,
            "Continue initial outreach",
            A::InitialInterest,
            "known_project_context",
            format!(
                "FIRST_FOLLOWUP on a brand-new unanswered lead. First follow-up should be soon after the initial outreach — not a 7-day wait. Confirm they received the first message, or whether they are still looking for help with {project}. One objective. Do not repeat the last outbound color/style discovery question.{no_kitchen_vs_bath}{no_wait_copy}"
            ),
        ),
        entry(
            SecondFollowup,
            "Offer an easy next step",
            A::PreferredNextStep,
            "measurements_or_estimate",
            format!(
                "EARLY_FOLLOWUP. Ask whether they have rough countertop measurements, or offer help with an estimate for {project}. One objective. Do not repeat the original color/style question.{no_kitchen_vs_bath}{no_wait_copy}"
            ),
        ),
        entry(
            SecondFollowup,
            "Help narrow options",
            A::DesignHelp,
            "narrow_options",
            format!(
                "EARLY_FOLLOWUP. Offer help narrowing countertop options for {project}. Do not repeat the original color/style question.{no_kitchen_vs_bath}{no_wait_copy}"
            ),
        ),
        entry(
            StalledConversation,
            "Provide a useful next step",
            A::HelpfulInformation,
            "easy_next_step",
            format!(
                "Another short follow-up on {project} with one new useful next step. Do not use weekly or monthly spacing yet.{no_kitchen_vs_bath}{no_wait_copy}"
            ),
        ),
        entry(
            StalledConversation,
            "Keep outreach moving",
            A::PreferredNextStep,
            "easy_response",
            format!(
                "Gradually wider spacing. One easy next step on {project}.{no_kitchen_vs_bath}{no_wait_copy}"
            ),
        ),
        entry(
            StalledConversation,
            "Stay available",
            A::HelpfulInformation,
            "light_value",
            format!(
                "Wider spacing after several unanswered attempts. Stay useful on {project}. Do not write as reactivation.{no_kitchen_vs_bath}{no_wait_copy}"
            ),
        ),
        entry(
            StalledConversation,
            "Light nurture",
            A::LongTermNurture,
            "project_still_relevant",
            format!(
                "{NURTURE_PURPOSE} Only after multiple unanswered attempts.{no_kitchen_vs_bath}"
            ),
        ),
        entry(
            StalledConversation,
            "Long-term nurture",
            A::LongTermNurture,
            "easy_restart",
            format!("{NURTURE_PURPOSE}{no_kitchen_vs_bath}"),
        ),
    ]
}

/// Steps pinned to their slots: the shape repeats its last entry, and
/// nurture before the nurture phase becomes a reconnect.
fn anchors_from_slots(
    slots: &[CadenceSlot],
    shape: &[CadenceShapeEntry],
    starting_phase: FollowupPhase,
) -> Vec<PlannedAiCadenceStep> {
    slots
        .iter()
        .enumerate()
        .map(|(index, slot)| {
            let template = shape.get(index).or_else(|| shape.last());
            let phase = phase_for_step(starting_phase, slot.offset_days);
            let angle = template.map_or(A::Reactivation, |t| t.angle);
            let nurture_too_early =
                angle == A::LongTermNurture && phase != FollowupPhase::LongTermNurture;
            PlannedAiCadenceStep {
                offset_days: slot.offset_days,
                delay_label: delay_label_from_days(slot.offset_days),
                situation: template.map_or(FollowupSituation::StalledConversation, |t| t.situation),
                goal: if nurture_too_early {
                    "Reconnect".into()
                } else {
                    template.map_or_else(|| "Re-engage".into(), |t| t.goal.clone())
                },
                purpose: template.map_or_else(|| NURTURE_PURPOSE.into(), |t| t.purpose.clone()),
                angle: if nurture_too_early {
                    A::Reactivation
                } else {
                    angle
                },
                subangle: Some(
                    template
                        .and_then(|t| t.subangle.clone())
                        .or_else(|| template.map(|t| t.angle.to_string()))
                        .unwrap_or_else(|| "reactivation".into()),
                ),
                phase: Some(phase),
                semantic_objective: None,
                delay_minutes: 0.0,
                channel: slot.channel,
                manual_action: None,
                original_step_index: None,
                preflight_status: None,
                preflight_reason: None,
                expected_send_on: None,
            }
        })
        .collect()
}

/// What [`conversation_plan`] reads.
#[derive(Debug, Clone, Default)]
pub struct ConversationPlanInputs<'a> {
    pub days_since: Option<f64>,
    pub outbound_count: f64,
    pub chase_days: Option<f64>,
    pub channel: Option<FollowupChannel>,
    pub mix_sms_and_email: bool,
    pub customer_state: Option<CustomerFollowupState>,
    pub unanswered_total: Option<f64>,
    pub contact_pressure: Option<&'a FollowupContactPressure>,
    pub now: Option<Instant>,
}

/// The planned steps (with offsets, not yet delays) and the cadence mode.
#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
pub fn conversation_plan(
    params: &ConversationPlanInputs<'_>,
) -> (Vec<PlannedAiCadenceStep>, AiCadenceMode) {
    let pressure = params.contact_pressure;
    let used: Vec<A> = pressure
        .map(|p| p.used_followup_angles.clone())
        .unwrap_or_default();
    let sms_usable = pressure.is_none_or(|p| sms_channel_is_usable(p.sms_channel_status));
    let channel = if sms_usable {
        params.channel.unwrap_or(FollowupChannel::Sms)
    } else {
        FollowupChannel::Email
    };
    let email_available = pressure.is_none_or(|p| p.email_available);
    let mix_sms_and_email = sms_usable && params.mix_sms_and_email && email_available;
    let starting_phase =
        pressure.map_or(FollowupPhase::ActivePursuit, |p| p.followup_starting_phase);
    let email_usable = pressure.is_none_or(|p| email_channel_is_usable(p.email_channel_status));
    let phase_slots = |gaps: &[f64]| {
        untested_sms_slots(
            &offsets_from_gaps(gaps),
            mix_sms_and_email,
            channel,
            email_usable,
        )
    };
    let (intent, extra) = intent_kinds(pressure);
    let has = |kind: I| intent == Some(kind) || extra.contains(&kind);
    if pressure.is_some() && has(I::NotNow) {
        let shape = skip_used_angles(MONTHLY_SHAPE, &used, pressure);
        return (
            anchors_from_slots(
                &phase_slots(&[45.0, 60.0, 90.0]),
                &shape,
                FollowupPhase::LongTermNurture,
            ),
            AiCadenceMode::Monthly,
        );
    }
    let low_pressure_intent = [
        I::Waiting,
        I::Delayed,
        I::ComparingQuotes,
        I::EvaluatingOptions,
        I::BudgetConcern,
        I::Planning,
    ]
    .into_iter()
    .any(has);
    if let Some(p) = pressure.filter(|_| low_pressure_intent) {
        let gaps: Vec<f64> = if has(I::Delayed) {
            DELAYED_REACTIVATION_GAPS.to_vec()
        } else if p.estimated_ready_at.is_some() || has(I::FutureProject) {
            FUTURE_PROJECT_GAPS.to_vec()
        } else {
            WAITING_REACTIVATION_GAPS.to_vec()
        };
        let shape = skip_used_angles(STALLED_SHAPE, &used, pressure);
        return (
            anchors_from_slots(&phase_slots(&gaps), &shape, FollowupPhase::Reactivation),
            AiCadenceMode::Unanswered,
        );
    }
    if pressure.is_some() && starting_phase == FollowupPhase::LongTermNurture {
        let shape = skip_used_angles(MONTHLY_SHAPE, &used, pressure);
        return (
            anchors_from_slots(
                &phase_slots(&LONG_TERM_NURTURE_GAPS),
                &shape,
                FollowupPhase::LongTermNurture,
            ),
            AiCadenceMode::Monthly,
        );
    }
    if pressure.is_some()
        && starting_phase == FollowupPhase::Decay
        && !uses_post_hold_cadence(pressure)
    {
        let shape = skip_used_angles(STALLED_SHAPE, &used, pressure);
        return (
            anchors_from_slots(&phase_slots(&DECAY_GAPS), &shape, FollowupPhase::Decay),
            AiCadenceMode::Unanswered,
        );
    }
    if pressure.is_some()
        && is_established_conversation_customer(pressure)
        && !uses_post_hold_cadence(pressure)
    {
        let shape = skip_used_angles(STALLED_SHAPE, &used, pressure);
        let established_phase = match starting_phase {
            FollowupPhase::FirstFollowup
            | FollowupPhase::EarlyFollowup
            | FollowupPhase::ActivePursuit => FollowupPhase::Decay,
            other => other,
        };
        return (
            anchors_from_slots(
                &phase_slots(&ESTABLISHED_CONVERSATION_GAPS),
                &shape,
                established_phase,
            ),
            AiCadenceMode::Unanswered,
        );
    }
    if let Some(p) = pressure.filter(|_| is_fresh_unanswered_lead(pressure)) {
        let offsets = sms_offsets_from_ramp(1.0, params.days_since.unwrap_or(0.0));
        let slots = untested_sms_slots(
            &offsets,
            mix_sms_and_email,
            channel,
            email_channel_is_usable(p.email_channel_status),
        );
        let starting_fresh = if p.followup_starting_phase == FollowupPhase::EarlyFollowup {
            FollowupPhase::EarlyFollowup
        } else {
            FollowupPhase::FirstFollowup
        };
        return (
            anchors_from_slots(&slots, &fresh_unanswered_lead_shape(p), starting_fresh),
            AiCadenceMode::Unanswered,
        );
    }
    let recent_unanswered_sms = pressure.is_some_and(|p| {
        params.days_since.is_some_and(|days| days < 14.0)
            && params.outbound_count >= 1.0
            && params.outbound_count <= 2.0
            && p.sms_channel_status != SmsChannelStatus::Untested
            && p.sms_channel_state != ChannelAttemptState::Untested
    });
    if recent_unanswered_sms && !uses_post_hold_cadence(pressure) {
        let now = params.now.unwrap_or_else(chrono::Utc::now);
        let time_sensitive = pressure
            .and_then(|p| p.pricing_change_valid_until)
            .is_some_and(|until| until > now);
        // The conversation's ramp from where it is: the next day after our
        // first message, 3 days after the second, then a week…
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let sent = (params.outbound_count as usize).clamp(1, NEW_CONVERSATION_GAPS.len());
        let first_gap = if time_sensitive {
            0.0
        } else {
            (NEW_CONVERSATION_GAPS[sent - 1] - params.days_since.unwrap_or(0.0)).max(0.0)
        };
        let mut gaps = vec![first_gap];
        gaps.extend_from_slice(&NEW_CONVERSATION_GAPS[sent..]);
        gaps.resize(8, MAX_CADENCE_GAP_DAYS);
        let mut offsets: Vec<f64> = Vec::with_capacity(gaps.len());
        for gap in gaps {
            let next = offsets.last().map_or(gap, |previous| previous + gap);
            offsets.push(next);
        }
        let slots = untested_sms_slots(&offsets, mix_sms_and_email, channel, email_usable);
        let shape = skip_used_angles(STALLED_SHAPE, &used, pressure);
        return (
            anchors_from_slots(&slots, &shape, starting_phase),
            AiCadenceMode::Unanswered,
        );
    }
    if uses_post_hold_cadence(pressure) {
        let has_hold = pressure.is_some_and(|p| p.crm_hold_until.is_some());
        let shape = skip_used_angles(
            if has_hold {
                POST_HOLD_SHAPE
            } else {
                EXISTING_OLD_LEAD_SHAPE
            },
            &used,
            pressure,
        );
        let slots = post_hold_slots(mix_sms_and_email, channel, pressure);
        return (
            anchors_from_slots(&slots, &shape, FollowupPhase::Reactivation),
            AiCadenceMode::Unanswered,
        );
    }
    let never_tried_sms = never_tried_sms_yet(
        params.outbound_count,
        params.mix_sms_and_email,
        params.channel,
        params.days_since,
    );
    let sms_untested_after_email = sms_usable
        && params.days_since.is_some()
        && pressure.map_or(never_tried_sms, |p| {
            p.sms_channel_state == ChannelAttemptState::Untested
                && p.email_channel_state != ChannelAttemptState::Untested
        });
    let both_channels_tried = !sms_untested_after_email
        && (pressure.is_some_and(|p| {
            matches!(
                p.email_channel_state,
                ChannelAttemptState::AttemptedNoResponse | ChannelAttemptState::Replied
            )
        }) || params.unanswered_total.unwrap_or(0.0) > 0.0);
    let history_heavy = params.customer_state == Some(CustomerFollowupState::Dormant)
        || (params.customer_state == Some(CustomerFollowupState::RepeatedNonResponder)
            && both_channels_tried)
        || (params.customer_state == Some(CustomerFollowupState::QuoteGhost)
            && both_channels_tried);
    let unreached = sms_untested_after_email
        && (matches!(
            params.customer_state,
            Some(CustomerFollowupState::NewLeadNoResponse | CustomerFollowupState::UnreachedLead)
        ) || pressure
            .is_some_and(|p| p.email_channel_state == ChannelAttemptState::AttemptedNoResponse));
    let outbound_count = if history_heavy {
        params.outbound_count.max(MONTHLY_RAMP_POSITION)
    } else {
        params.outbound_count
    };
    let mode = if unreached {
        AiCadenceMode::Unanswered
    } else {
        classify_ai_cadence_mode(
            outbound_count,
            params.chase_days,
            params.days_since,
            sms_untested_after_email,
        )
    };
    let raw_shape = if unreached {
        UNREACHED_SHAPE
    } else {
        match mode {
            AiCadenceMode::New => EARLY_SHAPE,
            AiCadenceMode::Monthly => MONTHLY_SHAPE,
            AiCadenceMode::Unanswered => STALLED_SHAPE,
        }
    };
    let shape = skip_used_angles(raw_shape, &used, pressure);
    if unreached {
        let offsets = untested_sms_offsets(
            params.days_since.unwrap_or(0.0),
            pressure.is_some_and(|p| p.customer_intent == CustomerIntent::HighUrgency),
        );
        let slots = untested_sms_slots(&offsets, mix_sms_and_email, channel, email_usable);
        return (anchors_from_slots(&slots, &shape, starting_phase), mode);
    }
    let start_position = ramp_position(
        outbound_count,
        params.days_since,
        params.chase_days,
        sms_untested_after_email,
    );
    let sms_offsets = sms_offsets_from_ramp(start_position, params.days_since.unwrap_or(0.0));
    let slots: Vec<CadenceSlot> =
        if mix_sms_and_email && !sms_untested_after_email && !history_heavy {
            interleave_emails(&sms_offsets)
        } else {
            sms_offsets
                .iter()
                .map(|offset_days| CadenceSlot {
                    offset_days: *offset_days,
                    channel: if sms_untested_after_email {
                        FollowupChannel::Sms
                    } else {
                        channel
                    },
                })
                .collect()
        };
    (anchors_from_slots(&slots, &shape, starting_phase), mode)
}

/// The first step a plain conversation would take (no pressure known).
pub fn next_cadence_anchor(
    days_since: Option<f64>,
    outbound_count: Option<f64>,
    chase_days: Option<Option<f64>>,
) -> Option<PlannedAiCadenceStep> {
    conversation_plan(&ConversationPlanInputs {
        days_since,
        outbound_count: outbound_count
            .unwrap_or_else(|| if days_since.is_none() { 0.0 } else { 1.0 }),
        chase_days: chase_days.unwrap_or(days_since),
        ..Default::default()
    })
    .0
    .into_iter()
    .next()
}

/// Calendar-day gaps as minutes from the previous step (no hour drift).
pub fn with_step_delays(anchors: Vec<PlannedAiCadenceStep>) -> Vec<PlannedAiCadenceStep> {
    let mut previous_offset = 0.0;
    anchors
        .into_iter()
        .map(|mut anchor| {
            let gap_days = cap_cadence_gap_days((anchor.offset_days - previous_offset).max(0.0));
            let delay_minutes =
                super::super::js::js_round(gap_days * MINUTES_PER_DAY).min(AI_MAX_DELAY_MINUTES);
            previous_offset = anchor.offset_days;
            anchor.delay_minutes = delay_minutes;
            anchor.delay_label = delay_label_from_minutes(delay_minutes, &anchor.delay_label);
            anchor
        })
        .collect()
}

/// A step's semantic objective from its angle and wording.
pub fn semantic_objective_for_step(
    angle: A,
    subangle: Option<&str>,
    goal: Option<&str>,
) -> FollowupSemanticObjective {
    use FollowupSemanticObjective as S;
    let sub = format!("{} {}", subangle.unwrap_or(""), goal.unwrap_or("")).to_lowercase();
    match angle {
        A::InitialInterest | A::EstimateOffer | A::PreferredNextStep => return S::ScheduleNextStep,
        A::BudgetObstacle => return S::IdentifyBudgetObstacle,
        A::QuoteStatus | A::QuoteReview => return S::ReviewQuote,
        A::MaterialPreference => return S::ConfirmMaterial,
        A::CloseLoop => return S::CloseLoop,
        _ => {}
    }
    match subangle {
        Some("easy_restart") => return S::EasyRestart,
        Some("project_still_relevant") => return S::ProjectStillRelevant,
        _ => {}
    }
    if matches!(
        angle,
        A::ProjectTiming | A::ProjectStatus | A::ProjectDependency
    ) {
        return S::ConfirmProjectActive;
    }
    if subangle == Some("multi_unit_readiness")
        || (angle == A::ProjectScope && re!(r"unit|apartment|builder").is_match(&sub))
    {
        return S::MultiUnitReadiness;
    }
    if angle == A::DesignHelp
        || (angle == A::HelpfulInformation
            && re!(r"look|vein|style|character|simple|bold|clean|movement").is_match(&sub))
    {
        return S::ChooseVisualStyle;
    }
    if angle == A::HelpfulInformation {
        return S::EducateMaterial;
    }
    if angle == A::LongTermNurture {
        if re!(r"restart", "i").is_match(&sub) {
            return S::EasyRestart;
        }
        if re!(r"close the loop|whenever", "i").is_match(&sub) {
            return S::CloseLoop;
        }
        return S::ProjectStillRelevant;
    }
    if matches!(angle, A::Reactivation | A::HoldReactivation) {
        return S::RestartProject;
    }
    S::Other
}

/// A step as the objective helpers read it.
pub trait StepIntent {
    fn angle(&self) -> A;
    fn subangle(&self) -> Option<&str>;
    fn goal(&self) -> Option<&str>;
    fn semantic_objective(&self) -> Option<FollowupSemanticObjective>;
}

impl<T: StepIntent> StepIntent for &T {
    fn angle(&self) -> A {
        (*self).angle()
    }
    fn subangle(&self) -> Option<&str> {
        (*self).subangle()
    }
    fn goal(&self) -> Option<&str> {
        (*self).goal()
    }
    fn semantic_objective(&self) -> Option<FollowupSemanticObjective> {
        (*self).semantic_objective()
    }
}

impl StepIntent for CadenceShapeEntry {
    fn angle(&self) -> A {
        self.angle
    }
    fn subangle(&self) -> Option<&str> {
        self.subangle.as_deref()
    }
    fn goal(&self) -> Option<&str> {
        Some(&self.goal)
    }
    fn semantic_objective(&self) -> Option<FollowupSemanticObjective> {
        self.semantic_objective
    }
}

impl StepIntent for PlannedAiCadenceStep {
    fn angle(&self) -> A {
        self.angle
    }
    fn subangle(&self) -> Option<&str> {
        self.subangle.as_deref()
    }
    fn goal(&self) -> Option<&str> {
        Some(&self.goal)
    }
    fn semantic_objective(&self) -> Option<FollowupSemanticObjective> {
        self.semantic_objective
    }
}

/// The copy objective a planned step is meant to pursue.
pub fn planner_copy_objective(step: &impl StepIntent) -> CopySemanticObjective {
    use CopySemanticObjective as C;
    use FollowupSemanticObjective as S;
    let angle = step.angle();
    let planned = step
        .semantic_objective()
        .unwrap_or_else(|| semantic_objective_for_step(angle, step.subangle(), step.goal()));
    let sub = format!(
        "{} {}",
        step.subangle().unwrap_or(""),
        step.goal().unwrap_or("")
    )
    .to_lowercase();
    if planned == S::ConfirmMaterial || angle == A::MaterialPreference {
        return if re!(r"color", "i").is_match(&sub) {
            C::Color
        } else {
            C::MaterialType
        };
    }
    if planned == S::ChooseVisualStyle || angle == A::DesignHelp {
        return C::Design;
    }
    if planned == S::IdentifyBudgetObstacle || angle == A::BudgetObstacle {
        return C::Budget;
    }
    if planned == S::ReviewQuote || matches!(angle, A::QuoteStatus | A::QuoteReview) {
        return C::Quote;
    }
    if planned == S::ScheduleNextStep || angle == A::InitialInterest {
        return C::NextStep;
    }
    if planned == S::CloseLoop || angle == A::CloseLoop {
        return C::CloseLoop;
    }
    if matches!(
        planned,
        S::EasyRestart | S::ProjectStillRelevant | S::RestartProject
    ) || matches!(angle, A::LongTermNurture | A::Reactivation)
    {
        return C::Nurture;
    }
    if angle == A::ProjectScope || re!(r"scope|kitchen vs|bathroom or", "i").is_match(&sub) {
        return C::Scope;
    }
    if planned == S::ConfirmProjectActive
        || matches!(
            angle,
            A::ProjectTiming | A::OriginalTimeline | A::ProjectStatus
        )
    {
        return C::Timing;
    }
    if planned == S::EducateMaterial {
        return C::MaterialType;
    }
    C::Nurture
}
