//! Cadence timing: the SMS ramp, gap tables, old-customer spacing and delay
//! labels (part of `smsFlowAiCadence.ts`).

use super::super::crm_hold::AI_HOLD_MAX_DELAY_MINUTES;
use super::super::dates::Instant;
use super::super::js::{js_number, js_round, js_to_fixed};
use super::super::pressure::timeline::count_outbound_touches_by_store_day;
use super::super::pressure::{
    CommunicationHistoryStatus, FollowupContactPressure, FollowupPhase, RelationshipAge,
    RelationshipState,
};
use super::super::send_window::{FollowupChannel, next_send_at_avoiding_sunday};
use super::super::timeline::{Channel, Direction, TimelineMessage};
use super::types::AiCadenceMode;
use crate::re;

pub const MINUTES_PER_DAY: f64 = 24.0 * 60.0;
/// The monthly follow-up rhythm, in days: no wait between two messages is
/// longer (only a hold may push the first send further out).
pub const MAX_CADENCE_GAP_DAYS: f64 = 30.0;
/// The second message goes 2–4 days after the first: replies come after the
/// first, so a customer who did not answer hears again while it is fresh.
pub const SECOND_MESSAGE_MIN_GAP_DAYS: f64 = 2.0;
pub const SECOND_MESSAGE_MAX_GAP_DAYS: f64 = 4.0;
/// A spread-out plan's last send is at least this many days from now.
pub const MIN_AI_FOLLOWUP_SPAN_DAYS: f64 = 30.0;
/// The longest regular AI cadence gap (`AI_MAX_DELAY_MINUTES`).
pub const AI_MAX_DELAY_MINUTES: f64 = 90.0 * 24.0 * 60.0;

/// Days before each SMS, from the previous one: now, +1, +2, +4, +7, +14, +30.
pub const SMS_RAMP_GAP_DAYS: [f64; 8] = [0.0, 1.0, 2.0, 4.0, 7.0, 14.0, 30.0, 30.0];
const LONG_TERM_GAP_DAYS: [f64; 2] = [30.0, 30.0];
/// Existing customers with a real conversation: not the new-lead ramp.
pub const ESTABLISHED_CONVERSATION_GAPS: [f64; 5] = [14.0, 21.0, 30.0, 30.0, 30.0];
/// First index whose gap is the monthly-or-wider rhythm.
pub const MONTHLY_RAMP_POSITION: f64 = 6.0;
const PLANNED_SMS_COUNT: usize = 8;

pub const UNTESTED_SMS_GAPS: [f64; 8] = [4.0, 8.0, 18.0, 30.0, 45.0, 60.0, 90.0, 90.0];
/// One recent unanswered SMS: do not pile 1–2 day touches.
pub const RECENT_UNANSWERED_GAPS: [f64; 8] = [0.0, 4.0, 9.0, 18.0, 30.0, 45.0, 60.0, 90.0];
pub const UNTESTED_SMS_URGENT_GAPS: [f64; 8] = [2.0, 7.0, 14.0, 21.0, 30.0, 45.0, 60.0, 90.0];
const UNTESTED_SMS_CHANNELS: [FollowupChannel; 8] = {
    use FollowupChannel::{Email, Sms};
    [Sms, Email, Sms, Email, Sms, Email, Sms, Email]
};
/// After a hold: reconnect, then widen.
pub const POST_HOLD_GAPS: [f64; 7] = [0.0, 6.0, 12.0, 25.0, 45.0, 60.0, 90.0];
pub const POST_HOLD_CHANNELS: [FollowupChannel; 6] = {
    use FollowupChannel::{Email, Sms};
    [Sms, Email, Sms, Email, Sms, Email]
};
pub const WAITING_REACTIVATION_GAPS: [f64; 5] = [12.0, 24.0, 45.0, 60.0, 90.0];
pub const DELAYED_REACTIVATION_GAPS: [f64; 5] = [21.0, 30.0, 45.0, 60.0, 90.0];
pub const DECAY_GAPS: [f64; 7] = [7.0, 14.0, 21.0, 30.0, 45.0, 60.0, 90.0];
pub const LONG_TERM_NURTURE_GAPS: [f64; 7] = [45.0, 60.0, 90.0, 90.0, 90.0, 90.0, 90.0];

/// A place in the sequence: when it goes out and over which channel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CadenceSlot {
    pub offset_days: f64,
    pub channel: FollowupChannel,
}

/// Old leads (On Hold, dormant, no digital history) get wider spacing.
pub fn is_old_followup_customer(pressure: Option<&FollowupContactPressure>) -> bool {
    pressure.is_some_and(|p| {
        p.relationship_age == RelationshipAge::Old
            || p.relationship_state == RelationshipState::ExistingOldLead
            || p.deal_stage_hold
    })
}

/// Double an old customer's gap, up to the 30-day rhythm.
pub fn widen_old_customer_followup_gap_minutes(delay_minutes: f64) -> f64 {
    if !delay_minutes.is_finite() || delay_minutes <= 0.0 {
        return 0.0;
    }
    js_round(delay_minutes * 2.0).min(MAX_CADENCE_GAP_DAYS * MINUTES_PER_DAY)
}

#[allow(clippy::cast_precision_loss)]
fn touch_is_old(at: Option<Instant>, now: Instant) -> bool {
    at.is_some_and(|at| {
        (now - at).num_milliseconds() as f64 / (MINUTES_PER_DAY * 60_000.0) >= MAX_CADENCE_GAP_DAYS
    })
}

fn parse_profile_timestamp(value: &str) -> Option<Instant> {
    let iso = format!("{}:00Z", value.replacen(' ', "T", 1));
    super::super::dates::js_date_parse(&iso).and_then(super::super::dates::from_millis)
}

/// An open CRM task created at least 30 days ago, or already overdue.
pub fn profile_has_old_open_task(profile: &str, now: Instant) -> bool {
    profile
        .split('\n')
        .filter(|line| line.starts_with("Activity:") && !re!(r"\bcompleted\b").is_match(line))
        .any(|line| {
            if re!(r"deadline \d+d overdue").is_match(line) {
                return true;
            }
            let old = |pattern: &regex::Regex| {
                pattern
                    .captures(line)
                    .is_some_and(|caps| touch_is_old(parse_profile_timestamp(&caps[1]), now))
            };
            old(re!(r"created (\d{4}-\d{2}-\d{2} \d{2}:\d{2})"))
                || old(re!(r"deadline (\d{4}-\d{2}-\d{2} \d{2}:\d{2})"))
        })
}

/// An old lead who already has a message or task at least 30 days old waits
/// 30 days between follow-ups, not a weekly ramp.
pub fn old_customer_uses_thirty_day_gap(
    pressure: Option<&FollowupContactPressure>,
    profile: Option<&str>,
    now: Instant,
) -> bool {
    if !is_old_followup_customer(pressure) {
        return false;
    }
    let old_message = pressure.is_some_and(|p| {
        p.days_since_last_outbound.unwrap_or(0.0) >= MAX_CADENCE_GAP_DAYS
            || p.days_since_last_customer_reply.unwrap_or(0.0) >= MAX_CADENCE_GAP_DAYS
            || touch_is_old(p.last_customer_message_at, now)
    });
    old_message || profile_has_old_open_task(profile.unwrap_or(""), now)
}

/// Last message at least this old: send now, +14 days, then every 30.
pub const STALE_CONVERSATION_DAYS: f64 = 30.0;
/// Last message older than this: send now, then every 30 days.
pub const LONG_STALE_CONVERSATION_DAYS: f64 = 45.0;

/// The gap schedule for an old conversation (the last gap repeats), or
/// `None` when the conversation is recent or there is none.
pub fn stale_conversation_gap_days(pressure: Option<&FollowupContactPressure>) -> Option<Vec<f64>> {
    let p = pressure?;
    let age = [p.days_since_last_outbound, p.days_since_last_customer_reply]
        .into_iter()
        .flatten()
        .filter(|days| days.is_finite())
        .fold(None, |min: Option<f64>, days| {
            Some(min.map_or(days, |m| m.min(days)))
        })?;
    if age > LONG_STALE_CONVERSATION_DAYS {
        return Some(vec![0.0, MAX_CADENCE_GAP_DAYS]);
    }
    (age >= STALE_CONVERSATION_DAYS).then(|| vec![0.0, 14.0, MAX_CADENCE_GAP_DAYS])
}

/// Old or existing customers with a real thread: weekly-plus spacing.
pub fn is_established_conversation_customer(pressure: Option<&FollowupContactPressure>) -> bool {
    let Some(p) = pressure else {
        return false;
    };
    if p.communication_history_status == CommunicationHistoryStatus::TrulyNew {
        return false;
    }
    let has_inbound = p.last_customer_message_at.is_some();
    let volume = p.outbound_last90_days >= 4.0 || p.unanswered_total_since_last_reply >= 4.0;
    if !has_inbound && !volume {
        return false;
    }
    matches!(
        p.relationship_age,
        RelationshipAge::Existing | RelationshipAge::Old
    ) || p.has_relationship_history
        || matches!(
            p.communication_history_status,
            CommunicationHistoryStatus::ActiveConversation
                | CommunicationHistoryStatus::PreviouslyContacted
                | CommunicationHistoryStatus::Dormant
        )
}

/// Inputs of [`resolve_cadence_preview_delay_minutes`].
#[derive(Debug, Clone, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct PreviewDelayInputs {
    pub generated_delay_minutes: Option<f64>,
    pub planned_delay_minutes: f64,
    pub floor_to_planned_delay: bool,
    /// Company instruction gap: replaces the planned and model waits.
    pub instruction_gap_minutes: Option<f64>,
    /// Old customers: double the chosen gap.
    pub widen_gap_for_old_customer: bool,
    /// Old customer with an old message or task: later gaps are 30 days.
    pub use_thirty_day_gap: bool,
    pub is_first_message: bool,
    /// Company instruction: send the first message today.
    pub send_first_message_today: bool,
    /// Old conversation schedule (first entry is the first message).
    pub stale_gap_days: Option<Vec<f64>>,
    /// Position among the messages that will be sent (0 = first).
    pub message_index: Option<f64>,
    /// Days since the last message either way; the first gap counts from then.
    pub days_since_last_contact: Option<f64>,
}

/// The wait before a preview message, within the bounds of
/// [`bound_cadence_gap_minutes`].
pub fn resolve_cadence_preview_delay_minutes(params: &PreviewDelayInputs) -> f64 {
    bound_cadence_gap_minutes(chosen_preview_delay_minutes(params), params)
}

/// No wait is over 30 days, and the second message goes 2–4 days after the
/// first unless the company instruction sets the gap.
pub fn bound_cadence_gap_minutes(delay_minutes: f64, params: &PreviewDelayInputs) -> f64 {
    let delay = delay_minutes.min(MAX_CADENCE_GAP_DAYS * MINUTES_PER_DAY);
    let second = !params.is_first_message
        && params
            .message_index
            .is_some_and(|index| (index - 1.0).abs() < f64::EPSILON);
    let instruction_gap = params
        .instruction_gap_minutes
        .is_some_and(|g| g.is_finite() && g >= 0.0);
    if !second || instruction_gap {
        return delay;
    }
    delay
        .max(SECOND_MESSAGE_MIN_GAP_DAYS * MINUTES_PER_DAY)
        .min(SECOND_MESSAGE_MAX_GAP_DAYS * MINUTES_PER_DAY)
}

#[allow(clippy::cast_precision_loss)]
fn chosen_preview_delay_minutes(params: &PreviewDelayInputs) -> f64 {
    let cap = AI_HOLD_MAX_DELAY_MINUTES as f64;
    let stale_gap = params
        .stale_gap_days
        .as_ref()
        .filter(|gaps| !gaps.is_empty());
    let first_today = params.is_first_message && params.send_first_message_today;
    if first_today
        || ((params.use_thirty_day_gap || stale_gap.is_some()) && params.is_first_message)
    {
        return 0.0;
    }
    if let Some(gap) = params
        .instruction_gap_minutes
        .filter(|g| g.is_finite() && *g >= 0.0)
    {
        return js_round(gap).max(0.0).min(cap);
    }
    if let Some(stale) = stale_gap {
        let index = params.message_index.unwrap_or(0.0).max(0.0);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let index = (index as usize).min(stale.len() - 1);
        return stale.get(index).copied().unwrap_or(MAX_CADENCE_GAP_DAYS) * MINUTES_PER_DAY;
    }
    if params.use_thirty_day_gap {
        return MAX_CADENCE_GAP_DAYS * MINUTES_PER_DAY;
    }
    let planned = js_round(params.planned_delay_minutes).max(0.0).min(cap);
    let chosen = match params.generated_delay_minutes.filter(|g| g.is_finite()) {
        None => planned,
        Some(generated) => {
            let generated = js_round(generated).max(0.0).min(cap);
            if params.floor_to_planned_delay {
                generated.max(planned)
            } else {
                generated
            }
        }
    };
    let gap = if params.widen_gap_for_old_customer {
        widen_old_customer_followup_gap_minutes(chosen)
    } else {
        chosen
    };
    if !params.is_first_message {
        return gap;
    }
    match params
        .days_since_last_contact
        .filter(|d| d.is_finite() && *d > 0.0)
    {
        Some(elapsed) => (gap - js_round(elapsed * MINUTES_PER_DAY)).max(0.0),
        None => gap,
    }
}

/// Whether the first preview message moves to today: the old-customer rule
/// must not undo a customer's hold; only an explicit instruction may.
pub fn should_force_first_message_today(
    instruction_sends_today: bool,
    old_customer_sends_today: bool,
    hold_until_at: Option<Instant>,
    now: Instant,
) -> bool {
    instruction_sends_today
        || (old_customer_sends_today && !hold_until_at.is_some_and(|hold| hold > now))
}

pub fn clamp_ai_cadence_send_at(
    when: Instant,
    channel: FollowupChannel,
    customer_requested_followup_at: Option<Instant>,
) -> Instant {
    next_send_at_avoiding_sunday(when, channel, customer_requested_followup_at)
}

pub fn cap_cadence_gap_days(gap: f64) -> f64 {
    if !gap.is_finite() || gap < 0.0 {
        0.0
    } else {
        gap.min(MAX_CADENCE_GAP_DAYS)
    }
}

fn ramp_gap_days(position: usize) -> f64 {
    let gap = SMS_RAMP_GAP_DAYS
        .get(position)
        .or_else(|| LONG_TERM_GAP_DAYS.get(position.saturating_sub(SMS_RAMP_GAP_DAYS.len())))
        .copied()
        .unwrap_or(MAX_CADENCE_GAP_DAYS);
    cap_cadence_gap_days(gap)
}

pub fn parse_followup_timeline_at(at: &str) -> Option<Instant> {
    super::super::crm_hold::parse_followup_timeline_at(at)
}

pub fn first_outbound_at_from_timeline(timeline: &[TimelineMessage]) -> Option<Instant> {
    timeline
        .iter()
        .filter(|m| m.direction == Direction::Outbound)
        .find_map(|m| parse_followup_timeline_at(&m.at))
}

pub fn last_outbound_at_from_timeline(timeline: &[TimelineMessage]) -> Option<Instant> {
    timeline
        .iter()
        .rev()
        .filter(|m| m.direction == Direction::Outbound)
        .find_map(|m| parse_followup_timeline_at(&m.at))
}

#[allow(clippy::cast_precision_loss)]
pub fn days_between(later: Instant, earlier: Instant) -> f64 {
    (later - earlier).num_milliseconds() as f64 / (MINUTES_PER_DAY * 60.0 * 1000.0)
}

pub fn days_since_last_outbound(timeline: &[TimelineMessage], now: Instant) -> Option<f64> {
    last_outbound_at_from_timeline(timeline).map(|last| days_between(now, last).floor().max(0.0))
}

/// How far along the SMS ramp the conversation sits. Any prior message uses
/// up the "send now" slot; a month of silence or a two-month chase jumps to
/// monthly — except when we never texted (emails only).
pub fn ramp_position(
    outbound_count: f64,
    days_since: Option<f64>,
    chase_days: Option<f64>,
    never_tried_sms: bool,
) -> f64 {
    let settled = !never_tried_sms
        && (days_since.unwrap_or(0.0) >= MAX_CADENCE_GAP_DAYS || chase_days.unwrap_or(0.0) >= 60.0);
    let mut position = if settled {
        MONTHLY_RAMP_POSITION
    } else {
        outbound_count.max(0.0).min(MONTHLY_RAMP_POSITION)
    };
    if days_since.is_some() && position < 1.0 {
        position = 1.0;
    }
    position
}

/// Ramp steps already used (SMS, or email when SMS is unavailable); sends on
/// the same store day are one step.
pub fn ramp_outbound_count(
    timeline: &[TimelineMessage],
    mix_sms_and_email: bool,
    channel: Option<FollowupChannel>,
) -> f64 {
    let ramp_channel = if mix_sms_and_email || channel != Some(FollowupChannel::Email) {
        Channel::Sms
    } else {
        Channel::Email
    };
    count_outbound_touches_by_store_day(
        timeline
            .iter()
            .filter(|m| m.direction == Direction::Outbound && m.channel == Some(ramp_channel))
            .map(|m| Some(m.at.as_str())),
    )
}

/// Outbound emails on file but no outbound SMS.
pub fn is_email_only_outbound_history(timeline: &[TimelineMessage]) -> bool {
    let outbound = || {
        timeline
            .iter()
            .filter(|m| m.direction == Direction::Outbound)
    };
    outbound().any(|m| m.channel == Some(Channel::Email))
        && !outbound().any(|m| m.channel == Some(Channel::Sms))
}

/// We can text, already contacted them somehow, but never texted.
pub fn never_tried_sms_yet(
    outbound_count: f64,
    mix_sms_and_email: bool,
    channel: Option<FollowupChannel>,
    days_since: Option<f64>,
) -> bool {
    days_since.is_some()
        && (mix_sms_and_email || channel != Some(FollowupChannel::Email))
        && outbound_count <= 0.0
}

pub fn classify_ai_cadence_mode(
    outbound_count: f64,
    chase_days: Option<f64>,
    days_since: Option<f64>,
    never_tried_sms: bool,
) -> AiCadenceMode {
    if days_since.is_none() && outbound_count <= 0.0 {
        return AiCadenceMode::New;
    }
    if ramp_position(outbound_count, days_since, chase_days, never_tried_sms)
        >= MONTHLY_RAMP_POSITION
    {
        AiCadenceMode::Monthly
    } else {
        AiCadenceMode::Unanswered
    }
}

/// Days from now for each upcoming SMS; the first honours what is left of
/// its gap after the time already waited.
pub fn sms_offsets_from_ramp(start_position: f64, days_since: f64) -> Vec<f64> {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let start = start_position.max(0.0) as usize;
    let mut offsets: Vec<f64> = Vec::with_capacity(PLANNED_SMS_COUNT);
    for index in 0..PLANNED_SMS_COUNT {
        let gap = ramp_gap_days(start + index);
        let next = offsets
            .last()
            .map_or_else(|| (gap - days_since).max(0.0), |previous| previous + gap);
        offsets.push(next);
    }
    offsets
}

/// An email halfway through early waits only; none after a 30-day gap.
pub fn interleave_emails(sms_offsets: &[f64]) -> Vec<CadenceSlot> {
    let mut slots = Vec::new();
    for (index, offset_days) in sms_offsets.iter().copied().enumerate() {
        slots.push(CadenceSlot {
            offset_days,
            channel: FollowupChannel::Sms,
        });
        let Some(next) = sms_offsets.get(index + 1).copied() else {
            continue;
        };
        if next - offset_days >= MAX_CADENCE_GAP_DAYS {
            continue;
        }
        let midpoint = (offset_days + next) / 2.0;
        if midpoint > offset_days && midpoint < next {
            slots.push(CadenceSlot {
                offset_days: midpoint,
                channel: FollowupChannel::Email,
            });
        }
    }
    slots
}

pub fn delay_label_from_days(offset_days: f64) -> String {
    if offset_days <= 0.0 {
        return "Immediately".into();
    }
    if offset_days < 1.0 {
        return format!("+{} hours", js_number(js_round(offset_days * 24.0)));
    }
    let count = if offset_days.fract() == 0.0 {
        js_number(offset_days)
    } else {
        js_to_fixed(offset_days, 1)
    };
    format!("+{count} days")
}

pub fn delay_label_from_minutes(delay_minutes: f64, fallback: &str) -> String {
    if delay_minutes <= 0.0 {
        return fallback.to_string();
    }
    let days = (delay_minutes / MINUTES_PER_DAY).floor();
    let hours = js_round((delay_minutes - days * MINUTES_PER_DAY) / 60.0);
    let mut parts = Vec::new();
    if days > 0.0 {
        parts.push(format!(
            "+{} day{}",
            js_number(days),
            if (days - 1.0).abs() < f64::EPSILON {
                ""
            } else {
                "s"
            }
        ));
    }
    if hours > 0.0 {
        parts.push(format!("+{}h", js_number(hours)));
    }
    if parts.is_empty() {
        fallback.to_string()
    } else {
        parts.join(" ")
    }
}

pub fn untested_sms_offsets(days_since: f64, urgent: bool) -> Vec<f64> {
    let gaps = if urgent {
        UNTESTED_SMS_URGENT_GAPS
    } else {
        UNTESTED_SMS_GAPS
    };
    let mut offsets: Vec<f64> = Vec::with_capacity(gaps.len());
    for gap in gaps {
        let next = offsets
            .last()
            .map_or_else(|| (gap - days_since).max(0.0), |previous| previous + gap);
        offsets.push(next);
    }
    offsets
}

/// Two SMS in a row at the end become SMS then email (when email works).
pub fn avoid_consecutive_same_channel(
    mut slots: Vec<CadenceSlot>,
    email_usable: bool,
) -> Vec<CadenceSlot> {
    let n = slots.len();
    if !email_usable || n < 2 {
        return slots;
    }
    if slots[n - 1].channel == slots[n - 2].channel && slots[n - 1].channel == FollowupChannel::Sms
    {
        slots[n - 1].channel = FollowupChannel::Email;
    }
    slots
}

pub fn untested_sms_slots(
    offsets: &[f64],
    mix_sms_and_email: bool,
    fallback: FollowupChannel,
    email_usable: bool,
) -> Vec<CadenceSlot> {
    let slots = offsets
        .iter()
        .enumerate()
        .map(|(index, offset_days)| CadenceSlot {
            offset_days: *offset_days,
            channel: if mix_sms_and_email {
                UNTESTED_SMS_CHANNELS
                    .get(index)
                    .copied()
                    .unwrap_or(FollowupChannel::Sms)
            } else {
                fallback
            },
        })
        .collect();
    avoid_consecutive_same_channel(slots, email_usable)
}

/// Gaps never shrink after a positive gap.
pub fn enforce_non_decreasing_gaps(gaps: &[f64]) -> Vec<f64> {
    let mut out: Vec<f64> = Vec::with_capacity(gaps.len());
    for gap in gaps {
        let gap = cap_cadence_gap_days(*gap);
        match out.last().copied() {
            Some(previous) if previous > 0.0 && gap < previous => out.push(previous),
            _ => out.push(gap),
        }
    }
    out
}

pub fn offsets_from_gaps(gaps: &[f64]) -> Vec<f64> {
    let mut offsets: Vec<f64> = Vec::with_capacity(gaps.len());
    for gap in enforce_non_decreasing_gaps(gaps)
        .into_iter()
        .map(cap_cadence_gap_days)
    {
        let next = offsets.last().map_or(gap, |previous| previous + gap);
        offsets.push(next);
    }
    offsets
}

/// The phase a step at `offset_days` is in, from the starting phase.
pub const fn phase_for_step(starting_phase: FollowupPhase, offset_days: f64) -> FollowupPhase {
    use FollowupPhase as P;
    match starting_phase {
        P::LongTermNurture => P::LongTermNurture,
        P::FirstFollowup | P::EarlyFollowup => {
            if offset_days >= 45.0 {
                P::LongTermNurture
            } else if offset_days >= 3.0 {
                P::EarlyFollowup
            } else {
                P::FirstFollowup
            }
        }
        P::Reactivation => {
            if offset_days >= 45.0 {
                P::LongTermNurture
            } else if offset_days >= 24.0 {
                P::Decay
            } else {
                P::Reactivation
            }
        }
        P::Decay => {
            if offset_days >= 45.0 {
                P::LongTermNurture
            } else if offset_days >= 30.0 {
                P::Reactivation
            } else {
                P::Decay
            }
        }
        P::ActivePursuit => {
            if offset_days >= 45.0 {
                P::LongTermNurture
            } else if offset_days >= 30.0 {
                P::Reactivation
            } else if offset_days >= 7.0 {
                P::Decay
            } else {
                P::ActivePursuit
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: f64 = MINUTES_PER_DAY;

    fn second() -> PreviewDelayInputs {
        PreviewDelayInputs {
            message_index: Some(1.0),
            ..PreviewDelayInputs::default()
        }
    }

    #[test]
    fn no_wait_is_over_thirty_days_and_the_second_message_comes_in_two_to_four() {
        // The model, the planner and the old-customer doubling are all capped.
        let later = PreviewDelayInputs {
            generated_delay_minutes: Some(60.0 * DAY),
            message_index: Some(3.0),
            ..PreviewDelayInputs::default()
        };
        assert_eq!(resolve_cadence_preview_delay_minutes(&later), 30.0 * DAY);
        let widened = PreviewDelayInputs {
            planned_delay_minutes: 21.0 * DAY,
            widen_gap_for_old_customer: true,
            message_index: Some(2.0),
            ..PreviewDelayInputs::default()
        };
        assert_eq!(resolve_cadence_preview_delay_minutes(&widened), 30.0 * DAY);
        assert_eq!(
            widen_old_customer_followup_gap_minutes(25.0 * DAY),
            30.0 * DAY
        );
        // A stale conversation's 14-day second gap, a 30-day old-customer gap
        // and a model's next-day touch all land in the window.
        let stale = PreviewDelayInputs {
            stale_gap_days: Some(vec![0.0, 14.0, 30.0]),
            ..second()
        };
        assert_eq!(resolve_cadence_preview_delay_minutes(&stale), 4.0 * DAY);
        let thirty = PreviewDelayInputs {
            use_thirty_day_gap: true,
            ..second()
        };
        assert_eq!(resolve_cadence_preview_delay_minutes(&thirty), 4.0 * DAY);
        let soon = PreviewDelayInputs {
            generated_delay_minutes: Some(DAY),
            ..second()
        };
        assert_eq!(resolve_cadence_preview_delay_minutes(&soon), 2.0 * DAY);
        let fits = PreviewDelayInputs {
            generated_delay_minutes: Some(3.0 * DAY),
            ..second()
        };
        assert_eq!(resolve_cadence_preview_delay_minutes(&fits), 3.0 * DAY);
        // The company's gap wins over the window, not over the cap.
        let instructed = PreviewDelayInputs {
            instruction_gap_minutes: Some(7.0 * DAY),
            ..second()
        };
        assert_eq!(
            resolve_cadence_preview_delay_minutes(&instructed),
            7.0 * DAY
        );
        let too_long = PreviewDelayInputs {
            instruction_gap_minutes: Some(45.0 * DAY),
            ..second()
        };
        assert_eq!(resolve_cadence_preview_delay_minutes(&too_long), 30.0 * DAY);
        // The planner's gaps are capped too.
        assert_eq!(cap_cadence_gap_days(90.0), 30.0);
        assert_eq!(cap_cadence_gap_days(-1.0), 0.0);
    }
}
