//! CRM dates the follow-up must wait for: open activities' due dates and
//! notes like "said will be ready in a month". Port of `smsFlowAiCrmHold.ts`.

use super::dates::{Instant, from_millis, iso};
use super::intent::message_time_ms;
use super::js::{js_len, js_slice, trim};
use super::send_window::SMS_FOLLOWUP_SEND_WINDOW;
use super::store_time::store_wall_time_to_utc;
use crate::{re, str_enum};
use chrono::Duration;

const MINUTES_PER_DAY: f64 = 24.0 * 60.0;
/// The longest an AI step may wait for a hold (`AI_HOLD_MAX_DELAY_MINUTES`).
pub const AI_HOLD_MAX_DELAY_MINUTES: i64 = 400 * 24 * 60;

str_enum! { pub enum CrmHoldSource { Activity = "activity", Note = "note" } }

/// Do not contact before `until`, and why.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CrmHoldUntil {
    pub until: Instant,
    pub reason: String,
    pub source: CrmHoldSource,
}

/// A CRM activity as the hold reads it.
#[derive(Debug, Clone, Default)]
pub struct ActivityLike {
    pub name: Option<String>,
    pub deadline: Option<String>,
    pub is_completed: bool,
}

/// A deal note as the hold reads it.
#[derive(Debug, Clone, Default)]
pub struct NoteLike {
    pub content: Option<String>,
    pub created_at: Option<String>,
}

/// `parseFollowupTimelineAt`: zone-less times are UTC.
pub fn parse_followup_timeline_at(at: &str) -> Option<Instant> {
    message_time_ms(Some(at)).and_then(from_millis)
}

/// Rep to-dos like "Email" or "call him" are not a do-not-contact date.
fn is_salesperson_reminder_activity(name: Option<&str>) -> bool {
    let normalized = re!(r"\s+")
        .replace_all(trim(name.unwrap_or("")), " ")
        .to_lowercase();
    matches!(
        normalized.as_str(),
        "email"
            | "sms"
            | "text"
            | "follow-up"
            | "follow up"
            | "call him"
            | "call her"
            | "call them"
            | "call back"
            | "called, texted"
            | "called texted"
    )
}

/// `YYYY-MM-DD` at the start of a deadline.
pub fn calendar_date_from_deadline(deadline: &str) -> Option<(i32, i32, i32)> {
    let caps = re!(r"^(\d{4})-(\d{2})-(\d{2})").captures(trim(deadline))?;
    Some((
        caps[1].parse().ok()?,
        caps[2].parse().ok()?,
        caps[3].parse().ok()?,
    ))
}

/// Date-only holds mean that store day: the first send is 9:00 AM store time.
pub fn first_allowed_send_time_on_date(year: i32, month: i32, day: i32) -> Instant {
    store_wall_time_to_utc(
        year,
        month,
        day,
        SMS_FOLLOWUP_SEND_WINDOW.start_hour,
        SMS_FOLLOWUP_SEND_WINDOW.start_minute,
    )
}

pub fn hold_until_from_deadline_string(deadline: &str) -> Option<Instant> {
    calendar_date_from_deadline(deadline)
        .map(|(year, month, day)| first_allowed_send_time_on_date(year, month, day))
        .or_else(|| parse_followup_timeline_at(deadline))
}

fn later_hold(current: Option<CrmHoldUntil>, next: CrmHoldUntil) -> CrmHoldUntil {
    match current {
        Some(current) if next.until <= current.until => current,
        _ => next,
    }
}

/// Open activities: the latest future due date wins (never the soonest).
pub fn crm_hold_from_activities(activities: &[ActivityLike], now: Instant) -> Option<CrmHoldUntil> {
    let mut best = None;
    for activity in activities {
        if activity.is_completed || is_salesperson_reminder_activity(activity.name.as_deref()) {
            continue;
        }
        let Some(deadline) = activity.deadline.as_deref().filter(|d| !d.is_empty()) else {
            continue;
        };
        let Some(until) = hold_until_from_deadline_string(deadline).filter(|until| *until > now)
        else {
            continue;
        };
        let name = activity
            .name
            .as_deref()
            .map(trim)
            .filter(|n| !n.is_empty())
            .unwrap_or("follow-up");
        best = Some(later_hold(
            best,
            CrmHoldUntil {
                until,
                source: CrmHoldSource::Activity,
                reason: format!(
                    "Open activity \"{name}\" is due {} — wait until then before contacting",
                    js_slice(deadline, 0, 10)
                ),
            },
        ));
    }
    best
}

/// Notes like "said will be ready in a month": the note date plus that span.
#[allow(clippy::float_cmp)]
pub fn crm_hold_from_notes(notes: &[NoteLike], now: Instant) -> Option<CrmHoldUntil> {
    let mut best = None;
    for note in notes {
        let Some(content) = note.content.as_deref().map(trim).filter(|c| !c.is_empty()) else {
            continue;
        };
        let Some(created) = note
            .created_at
            .as_deref()
            .filter(|c| !c.is_empty())
            .and_then(parse_followup_timeline_at)
        else {
            continue;
        };
        let Some(days) = relative_wait_days_from_note(content).filter(|days| *days > 0.0) else {
            continue;
        };
        #[allow(clippy::cast_possible_truncation)]
        let until =
            created + Duration::milliseconds((days.min(1e8) * MINUTES_PER_DAY * 60_000.0) as i64);
        if until <= now {
            continue;
        }
        let days_text = super::js::js_number(days);
        best = Some(later_hold(
            best,
            CrmHoldUntil {
                until,
                source: CrmHoldSource::Note,
                reason: format!(
                    "Deal note ({}) says to wait ~{days_text} day{}: \"{}{}\"",
                    &iso(created)[..10],
                    if days == 1.0 { "" } else { "s" },
                    js_slice(content, 0, 120),
                    if js_len(content) > 120 { "…" } else { "" }
                ),
            },
        ));
    }
    best
}

/// The latest restriction among open activities and notes.
pub fn resolve_crm_hold_until(
    activities: &[ActivityLike],
    notes: &[NoteLike],
    now: Instant,
) -> Option<CrmHoldUntil> {
    later_crm_hold(
        crm_hold_from_activities(activities, now),
        crm_hold_from_notes(notes, now),
    )
}

/// "In a month", "3-5 days", "in 2 weeks": days to wait.
pub fn relative_wait_days_from_note(content: &str) -> Option<f64> {
    let text = trim(content);
    if text.is_empty() {
        return None;
    }
    if re!(r"\bin\s+a\s+month\b", "i").is_match(text)
        || re!(r"\bin\s+one\s+month\b", "i").is_match(text)
    {
        return Some(30.0);
    }
    if re!(r"\bin\s+a\s+week\b", "i").is_match(text)
        || re!(r"\bin\s+one\s+week\b", "i").is_match(text)
    {
        return Some(7.0);
    }
    let number = |m: Option<regex::Match<'_>>| m.and_then(|m| m.as_str().parse::<f64>().ok());
    if let Some(end) = number(
        re!(r"(\d+)\s*[-–]\s*(\d+)\s+days", "i")
            .captures(text)
            .and_then(|caps| caps.get(2)),
    ) && end.is_finite()
        && end > 0.0
    {
        return Some(end);
    }
    if let Some(amount) = number(
        re!(r"\b(\d+)\s+days would be best\b", "i")
            .captures(text)
            .and_then(|caps| caps.get(1)),
    ) && amount.is_finite()
        && amount > 0.0
    {
        return Some(amount);
    }
    let caps = re!(r"\bin\s+(\d+)\s+(day|days|week|weeks|month|months)\b", "i").captures(text)?;
    let amount = number(caps.get(1)).filter(|a| a.is_finite() && *a > 0.0)?;
    let unit = caps[2].to_lowercase();
    Some(if unit.starts_with("day") {
        amount
    } else if unit.starts_with("week") {
        amount * 7.0
    } else {
        amount * 30.0
    })
}

pub fn minutes_until_crm_hold(hold: &CrmHoldUntil, now: Instant) -> i64 {
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    let raw = ((hold.until - now).num_milliseconds() as f64 / 60_000.0).ceil() as i64;
    raw.clamp(0, AI_HOLD_MAX_DELAY_MINUTES)
}

pub fn format_crm_hold_prompt_line(hold: &CrmHoldUntil) -> String {
    format!(
        "CRM WAIT UNTIL: {} — {}. MESSAGE 1 is scheduled for that day, not today. Write the check-in that should go out then (e.g. whether they are ready / how the quote went). Do not write as if you are contacting them before they said they would be ready.",
        &iso(hold.until)[..10],
        hold.reason
    )
}

/// The latest date among the hold sources. Channel history never changes this.
pub fn effective_hold_until(dates: &[Option<Instant>]) -> Option<Instant> {
    dates.iter().flatten().copied().max()
}

pub fn later_crm_hold(
    crm_hold: Option<CrmHoldUntil>,
    crm_note_hold: Option<CrmHoldUntil>,
) -> Option<CrmHoldUntil> {
    match (crm_hold, crm_note_hold) {
        (None, note) => note,
        (hold, None) => hold,
        (Some(hold), Some(note)) => Some(if hold.until >= note.until { hold } else { note }),
    }
}

/// If a hold is later than the scheduled send, move the send to the hold.
pub fn next_send_at_if_held(scheduled: Instant, current_hold: Option<Instant>) -> Instant {
    match current_hold {
        Some(hold) if scheduled < hold => hold,
        _ => scheduled,
    }
}

pub fn must_reschedule_for_hold(scheduled: Instant, current_hold: Option<Instant>) -> bool {
    next_send_at_if_held(scheduled, current_hold) != scheduled
}
