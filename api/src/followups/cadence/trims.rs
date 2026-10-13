//! Plan clean-ups: no two steps asking the same thing, shorter sequences
//! for low-information leads, no close-loop before the customer's window,
//! only available channels (part of `smsFlowAiCadence.ts`).

use super::super::dates::{Instant, from_millis};
use super::super::facts::{DealProductScope, LatestCustomerState, TimelinePosition};
use super::super::intelligence::eligibility::{
    ObjectivePlanProbe, eligibility_from_facts, objective_fingerprint,
};
use super::super::intent::MeaningfulCustomerIntentType as I;
use super::super::pressure::state::timeline_position_at;
use super::super::pressure::{
    CommunicationHistoryStatus, ContactPressureLevel, CustomerFollowupState, FollowupAngle as A,
    FollowupContactPressure, FollowupPhase, InformationDensity,
};
use super::super::send_window::FollowupChannel;
use super::entries::facts_from_pressure;
use super::plan::{planner_copy_objective, semantic_objective_for_step};
use super::ramp::{
    AI_MAX_DELAY_MINUTES, MAX_CADENCE_GAP_DAYS, MINUTES_PER_DAY, cap_cadence_gap_days,
    delay_label_from_minutes,
};
use super::shapes::NURTURE_PURPOSE;
use super::types::{CopySemanticObjective, FollowupSemanticObjective as S, PlannedAiCadenceStep};
use std::collections::HashSet;

/// Low-information repeated non-responders: five messages, not a full ramp.
const LOW_INFO_REPEATED_NON_RESPONDER_GAPS: [f64; 5] = [7.0, 30.0, 45.0, 60.0, 90.0];

fn objective_of(step: &PlannedAiCadenceStep) -> S {
    step.semantic_objective.unwrap_or_else(|| {
        semantic_objective_for_step(step.angle, step.subangle.as_deref(), Some(&step.goal))
    })
}

pub(crate) fn assign_semantic_objectives(
    planned: Vec<PlannedAiCadenceStep>,
) -> Vec<PlannedAiCadenceStep> {
    planned
        .into_iter()
        .map(|mut step| {
            step.semantic_objective = Some(objective_of(&step));
            step
        })
        .collect()
}

/// Drops repeats: two visual-style asks in a row, a second project-active
/// check on a low-information lead, a second identical late nurture, the
/// same angle twice, the same material-and-timing ask twice.
pub(crate) fn drop_consecutive_semantic_objectives(
    planned: Vec<PlannedAiCadenceStep>,
    pressure: Option<&FollowupContactPressure>,
) -> Vec<PlannedAiCadenceStep> {
    let mut kept: Vec<PlannedAiCadenceStep> = Vec::new();
    let mut seen_late: HashSet<S> = HashSet::new();
    let mut seen_timing = false;
    let mut seen_fact_keys: HashSet<String> = HashSet::new();
    let mut seen_copy_fingerprints: HashSet<String> = HashSet::new();
    let mut seen_angle_keys: HashSet<String> = HashSet::new();
    let eligibility = pressure.map(|p| eligibility_from_facts(&facts_from_pressure(Some(p))));
    let named_material = pressure
        .and_then(|p| {
            p.favorite_material
                .clone()
                .or_else(|| p.material_preferences.first().cloned())
                .or_else(|| p.historical_material_preferences.first().cloned())
        })
        .map(|m| super::super::js::trim(&m).to_lowercase())
        .unwrap_or_default();
    let has_timing_fact = pressure.is_some_and(|p| {
        p.latest_customer_state == LatestCustomerState::Waiting
            || p.estimated_ready_at.is_some()
            || p.original_timeline_evidence.is_some()
    });
    for step in planned {
        let objective = objective_of(&step);
        if objective == S::ChooseVisualStyle
            && kept
                .last()
                .is_some_and(|previous| objective_of(previous) == S::ChooseVisualStyle)
        {
            continue;
        }
        if pressure.is_some_and(|p| p.information_density == InformationDensity::Low)
            && objective == S::ConfirmProjectActive
            && seen_timing
        {
            continue;
        }
        if step.angle == A::LongTermNurture && !seen_late.insert(objective) {
            continue;
        }
        let angle_key = format!("{}:{}", step.angle, step.subangle.as_deref().unwrap_or(""));
        if eligibility.is_some()
            && step.angle != A::LongTermNurture
            && seen_angle_keys.contains(&angle_key)
        {
            continue;
        }
        seen_angle_keys.insert(angle_key);
        if objective == S::ConfirmProjectActive {
            seen_timing = true;
        }
        let fact_key = if named_material.is_empty() || !has_timing_fact {
            None
        } else if objective == S::ConfirmProjectActive {
            Some("timing".to_string())
        } else if matches!(objective, S::ConfirmMaterial | S::ReviewQuote) {
            Some(format!("material+timing:{named_material}"))
        } else {
            None
        };
        if let Some(fact_key) = fact_key {
            if !seen_fact_keys.insert(fact_key) {
                continue;
            }
        }
        let mut step = step;
        step.semantic_objective = Some(objective);
        if let Some(eligibility) = &eligibility {
            let copy_objective = planner_copy_objective(&step);
            let fingerprint = objective_fingerprint(
                copy_objective,
                eligibility,
                Some(&ObjectivePlanProbe {
                    angle: Some(step.angle.as_str()),
                    subangle: step.subangle.as_deref(),
                    goal: Some(&step.goal),
                }),
            );
            if !matches!(
                copy_objective,
                CopySemanticObjective::Nurture | CopySemanticObjective::CloseLoop
            ) && seen_copy_fingerprints.contains(&fingerprint)
            {
                continue;
            }
            seen_copy_fingerprints.insert(fingerprint);
        }
        kept.push(step);
    }
    kept
}

fn is_style_like_step(step: &PlannedAiCadenceStep) -> bool {
    matches!(
        objective_of(step),
        S::ChooseVisualStyle | S::EducateMaterial
    ) || matches!(
        step.angle,
        A::DesignHelp | A::HelpfulInformation | A::MaterialPreference
    )
}

/// A later step that replaces the planned intent.
struct Override {
    goal: &'static str,
    angle: A,
    subangle: &'static str,
    purpose: String,
    semantic_objective: S,
    phase: FollowupPhase,
}

fn apply_override(base: &PlannedAiCadenceStep, entry: &Override) -> PlannedAiCadenceStep {
    let mut step = base.clone();
    step.goal = entry.goal.into();
    step.angle = entry.angle;
    step.subangle = Some(entry.subangle.into());
    step.purpose.clone_from(&entry.purpose);
    step.semantic_objective = Some(entry.semantic_objective);
    step.phase = Some(entry.phase);
    step.manual_action = None;
    step
}

fn gap_delay(gap_days: f64) -> f64 {
    super::super::js::js_round(gap_days * MINUTES_PER_DAY).min(AI_MAX_DELAY_MINUTES)
}

fn other_channel(channel: FollowupChannel) -> FollowupChannel {
    if channel == FollowupChannel::Sms {
        FollowupChannel::Email
    } else {
        FollowupChannel::Sms
    }
}

/// Low-information repeated non-responders get five messages backing off.
pub(crate) fn trim_low_information_sequence(
    planned: Vec<PlannedAiCadenceStep>,
    pressure: Option<&FollowupContactPressure>,
) -> Vec<PlannedAiCadenceStep> {
    let Some(p) = pressure else {
        return planned;
    };
    if p.information_density != InformationDensity::Low
        || p.customer_state != CustomerFollowupState::RepeatedNonResponder
        || (p.global_contact_pressure != ContactPressureLevel::Elevated
            && p.unanswered_total_since_last_reply < 4.0)
    {
        return planned;
    }
    let Some(opening) = planned
        .iter()
        .find(|s| is_style_like_step(s) && s.angle != A::CloseLoop)
        .or_else(|| planned.iter().find(|s| s.angle != A::CloseLoop))
        .or_else(|| planned.first())
        .cloned()
    else {
        return planned;
    };
    let later = [
        Override {
            goal: "Check whether the project is still relevant",
            angle: A::LongTermNurture,
            subangle: "project_still_relevant",
            purpose: NURTURE_PURPOSE.into(),
            semantic_objective: S::ProjectStillRelevant,
            phase: FollowupPhase::LongTermNurture,
        },
        Override {
            goal: "Offer an easy restart",
            angle: A::LongTermNurture,
            subangle: "easy_restart",
            purpose: NURTURE_PURPOSE.into(),
            semantic_objective: S::EasyRestart,
            phase: FollowupPhase::LongTermNurture,
        },
        Override {
            goal: "Close the loop",
            angle: A::CloseLoop,
            subangle: "close_loop",
            purpose: "Use close_loop once only. Softly offer to pause outreach. Do not invent keyword replies (“keep open”, “active”, “paused”, “fronts”). Later messages back off.".into(),
            semantic_objective: S::CloseLoop,
            phase: FollowupPhase::LongTermNurture,
        },
        Override {
            goal: "Light nurture",
            angle: A::Reactivation,
            subangle: "light_checkin",
            purpose: NURTURE_PURPOSE.into(),
            semantic_objective: S::RestartProject,
            phase: FollowupPhase::LongTermNurture,
        },
    ];
    let mix = planned.iter().any(|s| s.channel == FollowupChannel::Sms)
        && planned.iter().any(|s| s.channel == FollowupChannel::Email);
    let first_channel = opening.channel;
    let mut gaps = LOW_INFO_REPEATED_NON_RESPONDER_GAPS;
    if let Some(style_opening) = planned
        .iter()
        .find(|s| is_style_like_step(s) && s.angle != A::CloseLoop)
    {
        if style_opening.offset_days > gaps[0] {
            gaps[0] = style_opening.offset_days;
        }
    }
    let mut rebuilt = vec![opening.clone()];
    for (index, entry) in later.iter().enumerate() {
        let mut step = apply_override(&opening, entry);
        step.channel = if mix {
            if index % 2 == 0 {
                other_channel(first_channel)
            } else {
                first_channel
            }
        } else {
            planned.get(index + 1).map_or(first_channel, |s| s.channel)
        };
        rebuilt.push(step);
    }
    let mut offset_days = 0.0;
    rebuilt
        .into_iter()
        .take(5)
        .enumerate()
        .map(|(index, mut step)| {
            let gap_days =
                cap_cadence_gap_days(gaps.get(index).copied().unwrap_or(MAX_CADENCE_GAP_DAYS));
            offset_days += gap_days;
            let delay_minutes = gap_delay(gap_days);
            step.offset_days = offset_days;
            step.delay_minutes = delay_minutes;
            step.delay_label = delay_label_from_minutes(delay_minutes, &step.delay_label);
            step
        })
        .collect()
}

/// A brand-new lead with a known scope: continue the first outreach.
pub(crate) fn trim_truly_new_known_scope_sequence(
    planned: Vec<PlannedAiCadenceStep>,
    pressure: Option<&FollowupContactPressure>,
) -> Vec<PlannedAiCadenceStep> {
    let Some(p) = pressure else {
        return planned;
    };
    if p.communication_history_status != CommunicationHistoryStatus::TrulyNew
        || (p.deal_product_scope == DealProductScope::Unknown
            && p.known_scope.as_deref().is_none_or(str::is_empty))
        || planned.is_empty()
    {
        return planned;
    }
    let known = p
        .known_scope
        .clone()
        .or_else(|| p.known_package_interest.clone())
        .unwrap_or_else(|| p.deal_product_scope.as_str().to_lowercase());
    planned
        .into_iter()
        .enumerate()
        .map(|(index, mut step)| {
            if index == 0 {
                step.goal = "Confirm interest".into();
                step.angle = A::InitialInterest;
                step.subangle = Some("known_project_context".into());
                step.purpose = format!(
                    "Continue initial outreach on the known {known} request. Confirm they received the first message or whether they still want help with that project. One easy next step. Do not call this a reactivation. Do not ask kitchen vs bathroom. Do not ask whether the project remains open. Do not describe this as a conversation that already took place. Do not say they are waiting, paused, or not ready."
                );
                step.semantic_objective = Some(S::ScheduleNextStep);
                step.phase = Some(FollowupPhase::FirstFollowup);
                return step;
            }
            if step.angle == A::Reactivation
                || (step.angle == A::ProjectScope && step.subangle.as_deref() != Some("multi_unit_readiness"))
                || step.semantic_objective == Some(S::ConfirmProjectActive)
            {
                step.goal = "Offer an easy next step".into();
                step.angle = A::PreferredNextStep;
                step.subangle = Some("easy_response".into());
                step.purpose = "One easy next step on the known project. Do not ask kitchen vs bathroom. Do not ask whether the project is still active. Do not write as if they are waiting or not ready.".into();
                step.semantic_objective = Some(S::ScheduleNextStep);
                if index < 3 {
                    step.phase = Some(FollowupPhase::EarlyFollowup);
                }
            }
            step
        })
        .collect()
}

/// Unknown scope and almost nothing known: a short generic sequence.
#[allow(clippy::too_many_lines)]
pub(crate) fn trim_unknown_low_information_sequence(
    planned: Vec<PlannedAiCadenceStep>,
    pressure: Option<&FollowupContactPressure>,
) -> Vec<PlannedAiCadenceStep> {
    let Some(p) = pressure else {
        return planned;
    };
    let filled = |v: &Option<String>| v.as_deref().is_some_and(|s| !s.is_empty());
    let intent = p.latest_meaningful_customer_intent.as_ref().map(|i| i.kind);
    if p.deal_product_scope != DealProductScope::Unknown
        || filled(&p.known_scope)
        || p.information_density != InformationDensity::Low
        || !p.material_preferences.is_empty()
        || p.quote_evidence
        || filled(&p.explicit_customer_request)
        || intent.is_some_and(|i| i != I::Unknown && i != I::ActiveInterest)
        || p.estimated_ready_at.is_some()
        || filled(&p.original_timeline_evidence)
        || p.customer_state == CustomerFollowupState::RepeatedNonResponder
    {
        return planned;
    }
    let Some(opening) = planned.first().cloned() else {
        return planned;
    };
    let truly_new = p.communication_history_status == CommunicationHistoryStatus::TrulyNew;
    let generic = |goal, angle, subangle, purpose: String, semantic_objective, phase| Override {
        goal,
        angle,
        subangle,
        purpose,
        semantic_objective,
        phase,
    };
    let nurture = |suffix: &str| format!("{NURTURE_PURPOSE}{suffix}");
    let scope_question = "Ask whether this is a kitchen, bathroom, or another area. Do not introduce a product category.".to_string();
    let mut entries: Vec<Override> = if truly_new {
        vec![
            generic(
                "Confirm interest",
                A::InitialInterest,
                "new_request",
                "Continue initial outreach on this new request. Confirm they received the first message or whether they still want help. Prefer project, plans, next step, or what matters most. Do not mention a material or product category. Do not call this a reactivation. Do not describe this as a conversation that already took place. Do not write as if they are waiting or not ready.".into(),
                S::ScheduleNextStep,
                FollowupPhase::FirstFollowup,
            ),
            generic(
                "Ask what matters most",
                A::PreferredNextStep,
                "what_matters_most",
                "Ask what matters most or the next step. Stay generic. Do not mention material direction. Do not introduce a product category.".into(),
                S::ScheduleNextStep,
                FollowupPhase::ActivePursuit,
            ),
            generic("Clarify project type", A::ProjectScope, "kitchen_bath_or_other", scope_question, S::Other, FollowupPhase::ActivePursuit),
        ]
    } else {
        vec![
            generic(
                "Gentle reactivation",
                A::Reactivation,
                "light_checkin",
                "Gentle reactivation. Do not introduce a product category.".into(),
                S::RestartProject,
                FollowupPhase::Reactivation,
            ),
            generic(
                "Clarify project type",
                A::ProjectScope,
                "kitchen_bath_or_other",
                scope_question,
                S::Other,
                FollowupPhase::Reactivation,
            ),
            generic(
                "Check project status",
                A::ProjectStatus,
                "project_still_open",
                "Generic project-status check. Do not introduce a product category.".into(),
                S::ConfirmProjectActive,
                FollowupPhase::Reactivation,
            ),
        ]
    };
    entries.push(generic(
        "Long-term nurture",
        A::LongTermNurture,
        "project_still_relevant",
        nurture(" Stay generic. Do not introduce a product category."),
        S::ProjectStillRelevant,
        FollowupPhase::LongTermNurture,
    ));
    entries.push(generic(
        "Easy restart",
        A::LongTermNurture,
        "easy_restart",
        nurture(" Stay generic. Do not introduce a product category."),
        S::EasyRestart,
        FollowupPhase::LongTermNurture,
    ));
    let gaps: [f64; 5] = if truly_new {
        [1.0, 3.0, 7.0, 14.0, 30.0]
    } else {
        [7.0, 14.0, 21.0, 30.0, 45.0]
    };
    let mix = planned.iter().any(|s| s.channel == FollowupChannel::Sms)
        && planned.iter().any(|s| s.channel == FollowupChannel::Email);
    let mut offset_days = 0.0;
    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let gap_days =
                cap_cadence_gap_days(gaps.get(index).copied().unwrap_or(MAX_CADENCE_GAP_DAYS));
            offset_days += gap_days;
            let delay_minutes = gap_delay(gap_days);
            let mut step = apply_override(&opening, entry);
            step.channel = if mix && index % 2 == 1 {
                other_channel(opening.channel)
            } else {
                opening.channel
            };
            step.offset_days = offset_days;
            step.delay_minutes = delay_minutes;
            step.delay_label = delay_label_from_minutes(delay_minutes, &opening.delay_label);
            step
        })
        .collect()
}

#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
fn send_at_offset(now: Instant, offset_days: f64) -> Instant {
    let ms = now.timestamp_millis() as f64 + offset_days * MINUTES_PER_DAY * 60_000.0;
    from_millis(ms as i64).unwrap_or(now)
}

/// No close-loop before the customer's expected project window.
pub(crate) fn suppress_close_loop_before_expected_window(
    planned: Vec<PlannedAiCadenceStep>,
    pressure: Option<&FollowupContactPressure>,
    now: Instant,
) -> Vec<PlannedAiCadenceStep> {
    let Some(p) = pressure else {
        return planned;
    };
    if p.estimated_ready_window_start
        .or(p.estimated_ready_at)
        .is_none()
    {
        return planned;
    }
    planned
        .into_iter()
        .map(|mut step| {
            if step.angle != A::CloseLoop && step.semantic_objective != Some(S::CloseLoop) {
                return step;
            }
            let send_at = send_at_offset(now, step.offset_days);
            if timeline_position_at(send_at, p.estimated_ready_at, p.estimated_ready_window_start, p.estimated_ready_window_end)
                != Some(TimelinePosition::BeforeExpectedDate)
            {
                return step;
            }
            step.goal = "Light timeline check".into();
            step.angle = A::LongTermNurture;
            step.subangle = Some("project_still_relevant".into());
            step.semantic_objective = Some(S::ProjectStillRelevant);
            step.phase = Some(FollowupPhase::LongTermNurture);
            step.purpose = "The customer expected window is still ahead. Ask whether timing is still the plan. Do not close the loop before the expected project window.".into();
            step
        })
        .collect()
}

/// A close-loop around a planned customer event (a showroom visit) becomes a
/// check on that event, moved past it.
#[allow(clippy::cast_precision_loss)]
pub(crate) fn rewrite_close_loop_for_planned_events(
    planned: Vec<PlannedAiCadenceStep>,
    pressure: Option<&FollowupContactPressure>,
    now: Instant,
) -> Vec<PlannedAiCadenceStep> {
    let Some(p) = pressure else {
        return planned;
    };
    let near_term = matches!(
        p.latest_customer_state,
        LatestCustomerState::NearTermProject | LatestCustomerState::Active
    ) || p.planned_customer_event_type.is_some();
    let day_ms = MINUTES_PER_DAY * 60.0 * 1000.0;
    planned
        .into_iter()
        .map(|mut step| {
            let send_ms = now.timestamp_millis() as f64 + step.offset_days * MINUTES_PER_DAY * 60_000.0;
            let overlaps_event = match (p.planned_customer_event_window_start, p.planned_customer_event_window_end) {
                (Some(start), Some(end)) => {
                    send_ms >= start.timestamp_millis() as f64 - day_ms && send_ms <= end.timestamp_millis() as f64 + 4.0 * day_ms
                }
                _ => false,
            };
            if step.angle != A::CloseLoop || !(overlaps_event || (near_term && step.offset_days <= 14.0)) {
                return step;
            }
            let extra_days = p.planned_customer_event_window_end.map_or_else(
                || (16.0 - step.offset_days).max(0.0),
                |end| ((end.timestamp_millis() as f64 + 4.0 * day_ms - send_ms) / day_ms).ceil().max(0.0),
            );
            let delay_minutes = (step.delay_minutes + extra_days * MINUTES_PER_DAY).min(AI_MAX_DELAY_MINUTES);
            step.offset_days += extra_days;
            step.delay_minutes = delay_minutes;
            step.delay_label = delay_label_from_minutes(delay_minutes, &step.delay_label);
            step.angle = A::ProjectTiming;
            step.subangle = Some("showroom_or_next_step".into());
            step.goal = "Confirm showroom or next step".into();
            step.semantic_objective = Some(S::ScheduleNextStep);
            step.purpose = match p.planned_customer_event_date_label.as_deref().filter(|l| !l.is_empty()) {
                Some(label) => format!(
                    "The customer said they might visit the showroom on {label}. Confirm whether that visit is still the plan. Do not say you are planning to be there. Do not close the loop."
                ),
                None => "A known near-term customer event is coming. Confirm showroom/timing or the next measurement step. Do not close the loop.".into(),
            };
            step
        })
        .collect()
}

/// Steps on an unavailable channel move to the other one, or drop.
pub(crate) fn restrict_channels_to_available(
    steps: Vec<PlannedAiCadenceStep>,
    pressure: Option<&FollowupContactPressure>,
) -> Vec<PlannedAiCadenceStep> {
    let Some(p) = pressure else {
        return steps;
    };
    steps
        .into_iter()
        .filter_map(|mut step| {
            match step.channel {
                FollowupChannel::Email if !p.email_available => {
                    if !p.sms_available {
                        return None;
                    }
                    step.channel = FollowupChannel::Sms;
                }
                FollowupChannel::Sms if !p.sms_available => {
                    if !p.email_available {
                        return None;
                    }
                    step.channel = FollowupChannel::Email;
                }
                _ => {}
            }
            Some(step)
        })
        .filter(|step| match step.channel {
            FollowupChannel::Sms => p.sms_available,
            FollowupChannel::Email => p.email_available,
        })
        .collect()
}
