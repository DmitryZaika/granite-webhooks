//! Pinning the plan to real send times: holds, the customer's contact
//! window, send windows, and the minimum sequence (`planAiFollowupCadence`
//! and the schedule helpers of `smsFlowAiCadence.ts`).

use super::super::classify::SMS_FLOW_AI_SENTINEL;
use super::super::crm_hold::AI_HOLD_MAX_DELAY_MINUTES;
use super::super::dates::{Instant, from_millis};
use super::super::facts::{
    ContactMethod, FollowupFacts, ManualActionRequired, MeasurementsStatus, UnfinishedNextStepKind,
};
use super::super::js::js_round;
use super::super::lifecycle::is_terminal_customer_state;
use super::super::pressure::build::earliest_followup_send_at;
use super::super::pressure::state::timeline_position_at;
use super::super::pressure::{
    CustomerFollowupState, FollowupAngle as A, FollowupContactPressure, FollowupPhase,
    InformationDensity,
};
use super::super::send_window::{
    FollowupChannel, StepTiming, add_cadence_gap_from_previous, clamp_followup_send_at,
    expected_followup_send_ats,
};
use super::plan::{ConversationPlanInputs, conversation_plan, with_step_delays};
use super::ramp::{
    AI_MAX_DELAY_MINUTES, MAX_CADENCE_GAP_DAYS, MIN_AI_FOLLOWUP_MESSAGES,
    MIN_AI_FOLLOWUP_SPAN_DAYS, MINUTES_PER_DAY, cap_cadence_gap_days, clamp_ai_cadence_send_at,
    days_between, delay_label_from_minutes,
};
use super::shapes::NURTURE_PURPOSE;
use super::trims::{
    assign_semantic_objectives, drop_consecutive_semantic_objectives,
    restrict_channels_to_available, rewrite_close_loop_for_planned_events,
    suppress_close_loop_before_expected_window, trim_low_information_sequence,
    trim_truly_new_known_scope_sequence, trim_unknown_low_information_sequence,
};
use super::types::{
    FollowupSemanticObjective as S, FollowupSituation, PlanAiFollowupCadenceOptions,
    PlannedAiCadenceStep,
};
use crate::re;
use serde::Serialize;

#[allow(clippy::cast_precision_loss)]
const HOLD_MAX_MINUTES: f64 = AI_HOLD_MAX_DELAY_MINUTES as f64;

#[allow(clippy::cast_precision_loss)]
const fn ms(at: Instant) -> f64 {
    at.timestamp_millis() as f64
}

/// `new Date(ms)`: fractional milliseconds truncate.
#[allow(clippy::cast_possible_truncation)]
fn at_ms(value: f64) -> Instant {
    from_millis(value.trunc() as i64).unwrap_or_default()
}

fn offset_at(now: Instant, offset_days: f64) -> Instant {
    at_ms(ms(now) + offset_days * MINUTES_PER_DAY * 60_000.0)
}

fn minutes_between_ceil(later: Instant, earlier: Instant) -> f64 {
    ((ms(later) - ms(earlier)) / 60_000.0).ceil()
}

/// The full plan: steps, channels, objectives, trims, holds and send times.
pub fn plan_ai_followup_cadence(
    last_outbound_at: Option<Instant>,
    now: Instant,
    options: &PlanAiFollowupCadenceOptions,
) -> Vec<PlannedAiCadenceStep> {
    let pressure = options.contact_pressure.as_ref();
    // A terminal customer state produces no follow-ups at all.
    if is_terminal_customer_state(pressure.map(|p| p.latest_customer_state)) {
        return Vec::new();
    }
    let days_since = last_outbound_at.map(|last| days_between(now, last).max(0.0));
    let outbound_count = options
        .outbound_count
        .unwrap_or_else(|| if last_outbound_at.is_none() { 0.0 } else { 1.0 });
    let chase_days = options
        .first_outbound_at
        .map_or(days_since, |first| Some(days_between(now, first).max(0.0)));
    let (anchors, _) = conversation_plan(&ConversationPlanInputs {
        days_since,
        outbound_count,
        chase_days,
        channel: Some(options.channel.unwrap_or(FollowupChannel::Sms)),
        mix_sms_and_email: options.mix_sms_and_email,
        customer_state: options
            .customer_state
            .or_else(|| pressure.map(|p| p.customer_state)),
        unanswered_total: options
            .unanswered_total
            .or_else(|| pressure.map(|p| p.unanswered_total_since_last_reply)),
        contact_pressure: pressure,
        now: Some(now),
    });
    let with_delays = restrict_channels_to_available(with_step_delays(anchors), pressure);
    let with_phone = mark_requested_phone_as_manual_action(with_delays, pressure);
    let with_objectives = assign_semantic_objectives(with_phone);
    let with_timeline = apply_timeline_copy_purpose(with_objectives, pressure, now);
    let deduped = drop_consecutive_semantic_objectives(with_timeline, pressure);
    let trimmed = trim_truly_new_known_scope_sequence(
        trim_unknown_low_information_sequence(
            trim_low_information_sequence(deduped, pressure),
            pressure,
        ),
        pressure,
    );
    let rewritten = rewrite_close_loop_for_planned_events(
        suppress_close_loop_before_expected_window(trimmed, pressure, now),
        pressure,
        now,
    );
    let trimmed_after_close = trim_sequence_after_close_loop(rewritten, pressure);
    let with_minimum_length = expand_compressed_gaps_after_long_first(
        ensure_minimum_followup_sequence(trimmed_after_close),
    );
    let with_contact_window = apply_customer_contact_window_to_planned_steps(
        with_minimum_length,
        pressure,
        now,
        options.hold_until_at,
    );
    let requested_followup_at = options
        .customer_requested_followup_at
        .or_else(|| pressure.and_then(|p| p.customer_requested_followup_at));
    let explicit_hold = pressure.and_then(|p| p.hard_contact_hold_until);
    let held = apply_hold_until_to_planned_steps(
        with_contact_window,
        options.hold_until_at,
        now,
        requested_followup_at,
        explicit_hold,
    );
    let hard_hold =
        latest_hard_hold_until(options.hold_until_at, requested_followup_at, explicit_hold);
    let finalized = finalize_planned_send_times(held, now, hard_hold, requested_followup_at);
    apply_hold_to_planned_send_times(finalized, now, hard_hold, requested_followup_at)
}

fn apply_timeline_copy_purpose(
    planned: Vec<PlannedAiCadenceStep>,
    pressure: Option<&FollowupContactPressure>,
    now: Instant,
) -> Vec<PlannedAiCadenceStep> {
    let Some(p) = pressure
        .filter(|p| p.estimated_ready_at.is_some() || p.estimated_ready_window_start.is_some())
    else {
        return planned;
    };
    planned
        .into_iter()
        .map(|mut step| {
            let send_at = offset_at(now, step.offset_days);
            let Some(position) = timeline_position_at(send_at, p.estimated_ready_at, p.estimated_ready_window_start, p.estimated_ready_window_end) else {
                return step;
            };
            let extra = match position {
                super::super::facts::TimelinePosition::BeforeExpectedDate => {
                    "The customer timeline is still ahead of this send. You may say that date is getting closer. Do not treat it as already past."
                }
                super::super::facts::TimelinePosition::NearExpectedDate => {
                    "This send is near the customer timeline. Ask whether that timing is still the plan."
                }
                super::super::facts::TimelinePosition::AfterExpectedDate => {
                    "This send is after the customer timeline. Ask whether the timing changed. Do not say they are still on track, that the date is getting closer, or that it is coming up."
                }
            };
            step.purpose = format!("{} Timeline position for this send: {position}. {extra}", step.purpose);
            step
        })
        .collect()
}

fn mark_requested_phone_as_manual_action(
    mut planned: Vec<PlannedAiCadenceStep>,
    pressure: Option<&FollowupContactPressure>,
) -> Vec<PlannedAiCadenceStep> {
    let Some(p) = pressure else {
        return planned;
    };
    if planned.is_empty()
        || (p.customer_requested_contact_method != Some(ContactMethod::Phone)
            && p.manual_action_required != Some(ManualActionRequired::PhoneCall))
        || !p.phone_available
    {
        return planned;
    }
    let first = &mut planned[0];
    first.goal = "Call the customer as requested".into();
    first.purpose = "They asked for a phone call. This is a manual phone action, not an SMS or email. Do not treat a text as equivalent.".into();
    first.angle = A::PreferredNextStep;
    first.subangle = Some("requested_phone_call".into());
    first.semantic_objective = Some(S::ScheduleNextStep);
    first.manual_action = Some("PHONE_CALL");
    planned
}

/// Moves later steps by the same bump as the first, keeping their gaps.
fn bump_steps(planned: &[PlannedAiCadenceStep], first_delay: f64) -> Vec<PlannedAiCadenceStep> {
    let bump_days = (first_delay - planned[0].delay_minutes) / MINUTES_PER_DAY;
    planned
        .iter()
        .enumerate()
        .map(|(index, step)| {
            let mut step = step.clone();
            if index == 0 {
                step.delay_minutes = first_delay;
                step.delay_label = delay_label_from_minutes(first_delay, &step.delay_label);
                step.offset_days = first_delay / MINUTES_PER_DAY;
                return step;
            }
            let offset_days = step.offset_days + bump_days;
            let previous_offset = if index == 1 {
                first_delay / MINUTES_PER_DAY
            } else {
                planned[index - 1].offset_days + bump_days
            };
            let gap_days = (offset_days - previous_offset).max(0.0);
            let delay = js_round(gap_days * MINUTES_PER_DAY).min(AI_MAX_DELAY_MINUTES);
            step.offset_days = offset_days;
            step.delay_minutes = delay;
            step.delay_label = delay_label_from_minutes(delay, &step.delay_label);
            step
        })
        .collect()
}

/// The first send is the latest of the CRM hold, the customer's requested
/// date and the cadence minimum: a hold may only make it later.
pub fn apply_hold_until_to_planned_steps(
    planned: Vec<PlannedAiCadenceStep>,
    hold_until_at: Option<Instant>,
    now: Instant,
    customer_requested_followup_at: Option<Instant>,
    explicit_do_not_contact_until: Option<Instant>,
) -> Vec<PlannedAiCadenceStep> {
    let Some(first) = planned.first() else {
        return planned;
    };
    let cadence_minimum_at = at_ms(ms(now) + first.delay_minutes * 60_000.0);
    let earliest = earliest_followup_send_at(
        cadence_minimum_at,
        &[
            hold_until_at,
            None,
            customer_requested_followup_at,
            explicit_do_not_contact_until,
        ],
    );
    let windowed = clamp_followup_send_at(earliest, first.channel);
    let hard_hold = latest_hard_hold_until(
        hold_until_at,
        customer_requested_followup_at,
        explicit_do_not_contact_until,
    );
    if hard_hold.is_some_and(|hold| windowed < hold) {
        return Vec::new();
    }
    let minutes_needed = minutes_between_ceil(windowed, now);
    if minutes_needed > HOLD_MAX_MINUTES {
        return Vec::new();
    }
    let delay_minutes = minutes_needed.max(0.0);
    let base = if delay_minutes <= first.delay_minutes {
        planned
    } else {
        bump_steps(&planned, delay_minutes)
    };
    shift_planned_steps_to_respect_hold(
        apply_send_window_to_later_steps(base, now, hard_hold, customer_requested_followup_at),
        hard_hold,
        now,
    )
}

fn latest_hard_hold_until(
    hold_until_at: Option<Instant>,
    customer_requested_followup_at: Option<Instant>,
    explicit_do_not_contact_until: Option<Instant>,
) -> Option<Instant> {
    [
        hold_until_at,
        customer_requested_followup_at,
        explicit_do_not_contact_until,
    ]
    .into_iter()
    .flatten()
    .max()
}

/// When a step goes out counting from now (the later of offset and delay).
pub fn planned_send_at_from_now(step: &PlannedAiCadenceStep, now: Instant) -> Instant {
    let from_offset = ms(now) + step.offset_days * MINUTES_PER_DAY * 60_000.0;
    let from_delay = ms(now) + step.delay_minutes * 60_000.0;
    at_ms(from_offset.max(from_delay))
}

fn step_timings(planned: &[PlannedAiCadenceStep]) -> Vec<StepTiming> {
    planned
        .iter()
        .map(|step| StepTiming {
            delay_minutes: step.delay_minutes,
            channel: step.channel,
        })
        .collect()
}

pub fn planned_steps_violate_hard_hold(
    planned: &[PlannedAiCadenceStep],
    hold_until_at: Option<Instant>,
    now: Instant,
) -> bool {
    let Some(hold) = hold_until_at else {
        return false;
    };
    !planned.is_empty()
        && expected_followup_send_ats(&step_timings(planned), now)
            .into_iter()
            .any(|send| send < hold)
}

fn shift_planned_steps_to_respect_hold(
    planned: Vec<PlannedAiCadenceStep>,
    hold_until_at: Option<Instant>,
    now: Instant,
) -> Vec<PlannedAiCadenceStep> {
    let Some(hold) = hold_until_at else {
        return planned;
    };
    if planned.is_empty() || !planned_steps_violate_hard_hold(&planned, Some(hold), now) {
        return planned;
    }
    let minutes_needed = minutes_between_ceil(hold, now);
    if minutes_needed > HOLD_MAX_MINUTES {
        return Vec::new();
    }
    let delay_minutes = planned[0].delay_minutes.max(minutes_needed);
    let shifted = bump_steps(&planned, delay_minutes);
    if planned_steps_violate_hard_hold(&shifted, Some(hold), now) {
        return Vec::new();
    }
    shifted
}

/// Puts each step on a real send-window clock, keeping the absolute offset
/// as the source of truth and the order moving forward.
fn apply_send_window_to_later_steps(
    planned: Vec<PlannedAiCadenceStep>,
    now: Instant,
    hard_hold: Option<Instant>,
    customer_requested_followup_at: Option<Instant>,
) -> Vec<PlannedAiCadenceStep> {
    let mut previous_send = now;
    planned
        .into_iter()
        .enumerate()
        .map(|(index, step)| {
            let clamp = |at: Instant| {
                clamp_ai_cadence_send_at(at, step.channel, customer_requested_followup_at)
            };
            let held = |at: Instant| match hard_hold {
                Some(hold) if at < hold => clamp(hold),
                _ => at,
            };
            let mut send_at = held(clamp(offset_at(now, step.offset_days)));
            if send_at <= previous_send {
                let remaining_gap = js_round(
                    step.offset_days * MINUTES_PER_DAY - (ms(previous_send) - ms(now)) / 60_000.0,
                )
                .max(0.0);
                let fallback_gap = if index == 0 {
                    step.delay_minutes.max(0.0)
                } else {
                    (if remaining_gap > 0.0 {
                        remaining_gap
                    } else {
                        step.delay_minutes
                    })
                    .max(1.0)
                };
                send_at = held(clamp(add_cadence_gap_from_previous(
                    previous_send,
                    fallback_gap,
                    step.channel,
                )));
            }
            let delay_minutes = minutes_between_ceil(send_at, previous_send)
                .max(0.0)
                .min(HOLD_MAX_MINUTES);
            previous_send = send_at;
            let offset_days = (ms(send_at) - ms(now)) / (MINUTES_PER_DAY * 60_000.0);
            if (delay_minutes - step.delay_minutes).abs() < f64::EPSILON
                && (offset_days - step.offset_days).abs() == 0.0
            {
                return step;
            }
            let mut step = step;
            step.delay_minutes = delay_minutes;
            step.offset_days = offset_days;
            step.delay_label = delay_label_from_minutes(delay_minutes, &step.delay_label);
            step
        })
        .collect()
}

/// Final per-message send-window normalization: always runs, forward-only.
pub fn finalize_planned_send_times(
    planned: Vec<PlannedAiCadenceStep>,
    now: Instant,
    hard_hold: Option<Instant>,
    customer_requested_followup_at: Option<Instant>,
) -> Vec<PlannedAiCadenceStep> {
    apply_send_window_to_later_steps(planned, now, hard_hold, customer_requested_followup_at)
}

/// The first send moves into the window the customer asked for ("text me
/// in 3-5 days"), unless a hold keeps it later.
fn apply_customer_contact_window_to_planned_steps(
    planned: Vec<PlannedAiCadenceStep>,
    pressure: Option<&FollowupContactPressure>,
    now: Instant,
    hold_until_at: Option<Instant>,
) -> Vec<PlannedAiCadenceStep> {
    let Some(p) = pressure else {
        return planned;
    };
    let (window_start, window_end) = (
        p.customer_requested_contact_window_start,
        p.customer_requested_contact_window_end,
    );
    let Some(first) = planned.first() else {
        return planned;
    };
    if window_start.is_none() && window_end.is_none() {
        return planned;
    }
    let hard_hold = latest_hard_hold_until(
        hold_until_at,
        p.customer_requested_followup_at,
        p.hard_contact_hold_until,
    );
    let day_ms = 24.0 * 60.0 * 60.0 * 1000.0;
    let window_end_inclusive = window_end.map(|end| ms(end) + day_ms - 1.0);
    let raw_cadence_at = at_ms(ms(now) + first.delay_minutes * 60_000.0);
    let mut candidate = raw_cadence_at;
    if let Some(start) = window_start.filter(|start| candidate < *start) {
        candidate = start;
    }
    if let Some(end_inclusive) = window_end_inclusive {
        if ms(candidate) > end_inclusive
            && hard_hold.is_none_or(|hold| ms(hold) <= end_inclusive + day_ms)
        {
            let mut target = window_start.or(window_end).unwrap_or(candidate);
            if let Some(hold) = hard_hold.filter(|hold| *hold > target) {
                target = hold;
            }
            candidate = clamp_followup_send_at(target, first.channel);
            if ms(candidate) > end_inclusive {
                candidate = clamp_followup_send_at(window_end.unwrap_or(target), first.channel);
            }
        }
    }
    candidate = clamp_followup_send_at(candidate, first.channel);
    if let Some(hold) = hard_hold.filter(|hold| candidate < *hold) {
        candidate = clamp_followup_send_at(hold, first.channel);
    }
    if (ms(candidate) - ms(raw_cadence_at)).abs() < 60_000.0 {
        return planned;
    }
    let minutes_needed = minutes_between_ceil(candidate, now)
        .max(0.0)
        .min(AI_MAX_DELAY_MINUTES);
    bump_steps(&planned, minutes_needed)
}

fn is_close_loop_step(step: &PlannedAiCadenceStep) -> bool {
    step.angle == A::CloseLoop || step.semantic_objective == Some(S::CloseLoop)
}

fn is_phone_only_cadence(planned: &[PlannedAiCadenceStep]) -> bool {
    !planned.is_empty()
        && planned
            .iter()
            .all(|step| step.manual_action == Some("PHONE_CALL"))
}

struct Pad {
    goal: &'static str,
    angle: A,
    subangle: &'static str,
    purpose: &'static str,
    semantic_objective: S,
    phase: FollowupPhase,
}

const MINIMUM_SEQUENCE_PADS: [Pad; 5] = [
    Pad {
        goal: "Change angle",
        angle: A::HelpfulInformation,
        subangle: "different_hook",
        purpose: "Different hook. Give a new reason to reply. Do not repeat an earlier question.",
        semantic_objective: S::EducateMaterial,
        phase: FollowupPhase::Decay,
    },
    Pad {
        goal: "Check project status",
        angle: A::ProjectStatus,
        subangle: "still_active",
        purpose: "Ask whether the project is still happening. One easy question.",
        semantic_objective: S::ConfirmProjectActive,
        phase: FollowupPhase::Reactivation,
    },
    Pad {
        goal: "Check whether the project is still relevant",
        angle: A::LongTermNurture,
        subangle: "project_still_relevant",
        purpose: NURTURE_PURPOSE,
        semantic_objective: S::ProjectStillRelevant,
        phase: FollowupPhase::LongTermNurture,
    },
    Pad {
        goal: "Offer an easy restart",
        angle: A::LongTermNurture,
        subangle: "easy_restart",
        purpose: NURTURE_PURPOSE,
        semantic_objective: S::EasyRestart,
        phase: FollowupPhase::LongTermNurture,
    },
    Pad {
        goal: "Light nurture",
        angle: A::Reactivation,
        subangle: "light_checkin",
        purpose: NURTURE_PURPOSE,
        semantic_objective: S::RestartProject,
        phase: FollowupPhase::LongTermNurture,
    },
];

/// Never a flat weekly cadence: a weekly chain, or compressed gaps after a
/// long first wait, get spread out.
pub fn expand_compressed_gaps_after_long_first(
    planned: Vec<PlannedAiCadenceStep>,
) -> Vec<PlannedAiCadenceStep> {
    let Some(first) = planned.first() else {
        return planned;
    };
    if planned.len() < 2 {
        return planned;
    }
    let long_first = first.offset_days >= MIN_AI_FOLLOWUP_SPAN_DAYS;
    let mut consecutive_weekly = 0;
    let mut max_consecutive_weekly = 0;
    let mut compressed_after_long_first = false;
    for pair in planned.windows(2) {
        let gap_days = pair[1].offset_days - pair[0].offset_days;
        if (6.0..=8.5).contains(&gap_days) {
            consecutive_weekly += 1;
            max_consecutive_weekly = max_consecutive_weekly.max(consecutive_weekly);
        } else {
            consecutive_weekly = 0;
        }
        if long_first && gap_days < MAX_CADENCE_GAP_DAYS - 0.5 {
            compressed_after_long_first = true;
        }
    }
    let flat_weekly_chain =
        max_consecutive_weekly >= 3 || (first.offset_days >= 6.0 && max_consecutive_weekly >= 2);
    if compressed_after_long_first || flat_weekly_chain {
        apply_minimum_sequence_offsets(planned)
    } else {
        planned
    }
}

/// Padding gaps: widen, never stuck at weekly.
const PROGRESSIVE_PAD_GAPS: [f64; 7] = [7.0, 14.0, 21.0, 30.0, 30.0, 30.0, 30.0];

/// Offsets for a padded plan: progressive 7→14→21→30 gaps (30 after a long
/// first wait), the last step at least 30 days out.
pub fn apply_minimum_sequence_offsets(
    planned: Vec<PlannedAiCadenceStep>,
) -> Vec<PlannedAiCadenceStep> {
    let Some(first) = planned.first().cloned() else {
        return planned;
    };
    let mut previous_offset = first.offset_days;
    let min_last_offset = MIN_AI_FOLLOWUP_SPAN_DAYS;
    let progressive_start = usize::from(first.offset_days >= 6.0);
    let count = planned.len();
    let mut out = vec![first];
    for (index, mut step) in planned.into_iter().enumerate().skip(1) {
        let is_last = index == count - 1;
        let progressive_gap = PROGRESSIVE_PAD_GAPS
            [(progressive_start + index - 1).min(PROGRESSIVE_PAD_GAPS.len() - 1)];
        let min_pad_gap_days = if previous_offset >= min_last_offset {
            MAX_CADENCE_GAP_DAYS
        } else {
            progressive_gap
        };
        let mut offset_days = previous_offset + cap_cadence_gap_days(min_pad_gap_days);
        if is_last && offset_days < min_last_offset {
            offset_days = min_last_offset;
        }
        let mut gap_days = cap_cadence_gap_days(offset_days - previous_offset);
        offset_days = previous_offset + gap_days;
        if is_last && offset_days < min_last_offset && gap_days < MAX_CADENCE_GAP_DAYS {
            offset_days = min_last_offset.min(previous_offset + MAX_CADENCE_GAP_DAYS);
            gap_days = offset_days - previous_offset;
        }
        let delay_minutes = js_round(gap_days * MINUTES_PER_DAY).min(AI_MAX_DELAY_MINUTES);
        step.offset_days = offset_days;
        step.delay_minutes = delay_minutes;
        step.delay_label = delay_label_from_minutes(delay_minutes, &step.delay_label);
        out.push(step);
        previous_offset = offset_days;
    }
    out
}

/// An automated follow-up has at least 5 messages over at least 30 days,
/// and close-the-loop is never the second (or only) message.
fn ensure_minimum_followup_sequence(
    planned: Vec<PlannedAiCadenceStep>,
) -> Vec<PlannedAiCadenceStep> {
    if planned.is_empty() || is_phone_only_cadence(&planned) {
        return planned;
    }
    if planned.len() >= MIN_AI_FOLLOWUP_MESSAGES
        && planned.last().map_or(0.0, |s| s.offset_days) >= MIN_AI_FOLLOWUP_SPAN_DAYS
    {
        return expand_compressed_gaps_after_long_first(planned);
    }
    let close_steps: Vec<&PlannedAiCadenceStep> =
        planned.iter().filter(|s| is_close_loop_step(s)).collect();
    let body: Vec<PlannedAiCadenceStep> = planned
        .iter()
        .filter(|s| !is_close_loop_step(s))
        .cloned()
        .collect();
    let Some(template) = body.first().or_else(|| planned.first()).cloned() else {
        return planned;
    };
    let mut used_keys: Vec<String> = body
        .iter()
        .map(|s| format!("{}:{}", s.angle, s.subangle.as_deref().unwrap_or("")))
        .collect();
    let needed_body = body
        .len()
        .max(MIN_AI_FOLLOWUP_MESSAGES - usize::from(!close_steps.is_empty()));
    let mut padded = body;
    let pad_step = |pad: &Pad, goal: String, subangle: String| {
        let mut step = template.clone();
        step.goal = goal;
        step.angle = pad.angle;
        step.subangle = Some(subangle);
        step.purpose = pad.purpose.into();
        step.semantic_objective = Some(pad.semantic_objective);
        step.phase = Some(pad.phase);
        step.situation = FollowupSituation::StalledConversation;
        step.manual_action = None;
        step
    };
    for pad in &MINIMUM_SEQUENCE_PADS {
        if padded.len() >= needed_body {
            break;
        }
        let key = format!("{}:{}", pad.angle, pad.subangle);
        if used_keys.contains(&key) {
            continue;
        }
        used_keys.push(key);
        padded.push(pad_step(pad, pad.goal.into(), pad.subangle.into()));
    }
    while padded.len() < needed_body {
        let pad = &MINIMUM_SEQUENCE_PADS[padded.len() % MINIMUM_SEQUENCE_PADS.len()];
        let n = padded.len();
        padded.push(pad_step(
            pad,
            format!("{} (continued)", pad.goal),
            format!("{}_{n}", pad.subangle),
        ));
    }
    if let Some(close) = close_steps.first() {
        padded.push((*close).clone());
    }
    apply_minimum_sequence_offsets(padded)
}

fn trim_sequence_after_close_loop(
    planned: Vec<PlannedAiCadenceStep>,
    pressure: Option<&FollowupContactPressure>,
) -> Vec<PlannedAiCadenceStep> {
    let Some(close_index) = planned.iter().position(is_close_loop_step) else {
        return planned;
    };
    if pressure.map_or(0.0, |p| p.unanswered_total_since_last_reply) < 1.0
        && pressure.and_then(|p| p.days_since_last_outbound).is_none()
    {
        return planned;
    }
    let terminate = pressure.is_some_and(|p| {
        p.information_density == InformationDensity::Low
            || p.deal_product_scope == super::super::facts::DealProductScope::Unknown
            || p.customer_state == CustomerFollowupState::RepeatedNonResponder
    });
    if !terminate {
        return planned;
    }
    let mut planned = planned;
    planned.truncate(close_index + 1);
    planned
}

/// Every send at or after the hard hold and not in the past; delays from
/// the previous send.
pub fn apply_hold_to_planned_send_times(
    planned: Vec<PlannedAiCadenceStep>,
    now: Instant,
    hard_hold: Option<Instant>,
    customer_requested_followup_at: Option<Instant>,
) -> Vec<PlannedAiCadenceStep> {
    let mut previous_send = now;
    planned
        .into_iter()
        .enumerate()
        .map(|(index, step)| {
            let mut send_at = offset_at(now, step.offset_days);
            if let Some(hold) = hard_hold.filter(|hold| send_at < *hold) {
                send_at = hold;
            }
            if send_at < now {
                send_at = now;
            }
            send_at =
                clamp_ai_cadence_send_at(send_at, step.channel, customer_requested_followup_at);
            if let Some(hold) = hard_hold.filter(|hold| send_at < *hold) {
                send_at =
                    clamp_ai_cadence_send_at(hold, step.channel, customer_requested_followup_at);
            }
            let delay_from = if index == 0 { now } else { previous_send };
            let delay_minutes = minutes_between_ceil(send_at, delay_from)
                .max(0.0)
                .min(HOLD_MAX_MINUTES);
            let offset_days = (ms(send_at) - ms(now)) / (MINUTES_PER_DAY * 60_000.0);
            previous_send = send_at;
            if (delay_minutes - step.delay_minutes).abs() < f64::EPSILON
                && (offset_days - step.offset_days).abs() == 0.0
            {
                return step;
            }
            let mut step = step;
            step.delay_minutes = delay_minutes;
            step.offset_days = offset_days;
            step.delay_label = delay_label_from_minutes(delay_minutes, &step.delay_label);
            step
        })
        .collect()
}

/// A message of the edited preview.
#[derive(Debug, Clone, Default, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AiCadenceFlowPreviewStep {
    pub skip: bool,
    pub body: String,
    pub subject: Option<String>,
    /// Set when edited in the preview: overrides the planned wait.
    #[serde(default)]
    pub delay_minutes: Option<f64>,
    /// The channel this message was generated for; it wins over the flow's.
    #[serde(default)]
    pub channel: Option<FollowupChannel>,
}

/// An SMS flow step (template or custom text) for the flow tables.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SmsFlowStepInput {
    pub channel: FollowupChannel,
    pub template_id: Option<i64>,
    pub custom_text: Option<String>,
    pub subject: Option<String>,
    pub delay_minutes: f64,
}

fn measurement_facts(items: &[String]) -> bool {
    items
        .iter()
        .any(|item| re!(r"measurement|dimension|square footage", "i").is_match(item))
}

/// Missing measurements block the project.
pub fn measurements_block_project_progress(
    pressure: Option<&FollowupContactPressure>,
    facts: Option<&FollowupFacts>,
) -> bool {
    let (status, kind, unresolved) = match (facts, pressure) {
        (Some(f), _) => (
            f.countertop_measurements_status,
            f.unfinished_next_step_kind,
            f.active_unresolved_facts.as_slice(),
        ),
        (None, Some(p)) => (
            p.countertop_measurements_status,
            p.unfinished_next_step_kind,
            p.active_unresolved_facts.as_slice(),
        ),
        (None, None) => return false,
    };
    status == Some(MeasurementsStatus::Needed)
        || kind == Some(UnfinishedNextStepKind::CustomerMeasurements)
        || measurement_facts(unresolved)
}

pub fn first_cadence_step_ignores_blocking_measurements(
    pressure: Option<&FollowupContactPressure>,
    planned: &[PlannedAiCadenceStep],
) -> bool {
    if !measurements_block_project_progress(pressure, None) {
        return false;
    }
    planned.first().is_some_and(|first| {
        first.angle == A::MaterialPreference
            || first.semantic_objective == Some(S::ConfirmMaterial)
            || first.subangle.as_deref() == Some("favorite_option")
    })
}

fn preview_delay(delay: Option<f64>) -> Option<f64> {
    delay
        .filter(|d| d.is_finite())
        .map(|d| js_round(d).max(0.0).min(HOLD_MAX_MINUTES))
}

/// The flow steps to save from the plan and the generated (or edited) preview.
pub fn build_ai_cadence_flow_steps(
    planned: &[PlannedAiCadenceStep],
    preview: &[AiCadenceFlowPreviewStep],
    channel: Option<FollowupChannel>,
) -> Vec<SmsFlowStepInput> {
    let executable: Vec<&AiCadenceFlowPreviewStep> = preview
        .iter()
        .filter(|g| !g.skip && !super::super::js::trim(&g.body).is_empty())
        .collect();
    let compact_from_preview_delays = executable
        .iter()
        .any(|g| g.delay_minutes.is_some_and(f64::is_finite));
    let custom = |channel: FollowupChannel, generated: &AiCadenceFlowPreviewStep, delay: f64| {
        SmsFlowStepInput {
            channel,
            template_id: None,
            custom_text: Some(generated.body.clone()),
            subject: if channel == FollowupChannel::Email {
                generated.subject.clone()
            } else {
                None
            },
            delay_minutes: delay,
        }
    };
    if compact_from_preview_delays && !executable.is_empty() && executable.len() != planned.len() {
        return executable
            .into_iter()
            .map(|generated| {
                let ch = generated
                    .channel
                    .or(channel)
                    .unwrap_or(FollowupChannel::Sms);
                custom(
                    ch,
                    generated,
                    preview_delay(generated.delay_minutes).unwrap_or(0.0),
                )
            })
            .collect();
    }
    let mut steps = Vec::new();
    let mut accumulated = 0.0;
    for (index, step) in planned.iter().enumerate() {
        let generated = preview.get(index);
        accumulated += generated
            .and_then(|g| preview_delay(g.delay_minutes))
            .unwrap_or(step.delay_minutes);
        let ch = step.channel;
        match generated {
            Some(generated)
                if generated.skip || super::super::js::trim(&generated.body).is_empty() => {}
            Some(generated) => {
                steps.push(custom(ch, generated, accumulated));
                accumulated = 0.0;
            }
            None => {
                steps.push(SmsFlowStepInput {
                    channel: ch,
                    template_id: None,
                    custom_text: Some(SMS_FLOW_AI_SENTINEL.into()),
                    subject: None,
                    delay_minutes: accumulated,
                });
                accumulated = 0.0;
            }
        }
    }
    steps
}
