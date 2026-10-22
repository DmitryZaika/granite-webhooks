//! When automated follow-ups may go out: SMS 9:00 AM to before 6:00 PM,
//! email 8:30 AM to 7:00 PM store time, never on major holidays, and AI
//! follow-ups not on Sunday. Port of `smsFlowSendWindow.ts`.

use super::dates::Instant;
use super::store_time::{
    add_store_calendar_days, is_store_sunday, same_store_calendar_day, store_wall_time_to_utc,
    store_zoned_parts,
};
use crate::str_enum;
use chrono::{Datelike, Duration, NaiveDate};

str_enum! { pub enum FollowupChannel { Sms = "sms", Email = "email" } }

/// A send window in store local time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FollowupSendWindow {
    pub start_hour: i32,
    pub start_minute: i32,
    pub end_hour: i32,
    pub end_minute: i32,
}

/// Email: 8:30 AM–7:00 PM store local time (both ends inclusive).
pub const EMAIL_FOLLOWUP_SEND_WINDOW: FollowupSendWindow = FollowupSendWindow {
    start_hour: 8,
    start_minute: 30,
    end_hour: 19,
    end_minute: 0,
};

/// SMS: 9:00 AM–before 6:00 PM store local time (start inclusive, end exclusive).
pub const SMS_FOLLOWUP_SEND_WINDOW: FollowupSendWindow = FollowupSendWindow {
    start_hour: 9,
    start_minute: 0,
    end_hour: 18,
    end_minute: 0,
};

/// The longest delay a flow step may have (`MAX_DELAY_MINUTES`).
pub const MAX_DELAY_MINUTES: i64 = 43_200;

const fn window_for(channel: FollowupChannel) -> FollowupSendWindow {
    match channel {
        FollowupChannel::Email => EMAIL_FOLLOWUP_SEND_WINDOW,
        FollowupChannel::Sms => SMS_FOLLOWUP_SEND_WINDOW,
    }
}

fn window_bounds_on_store_day(when: Instant, channel: FollowupChannel) -> (Instant, Instant) {
    let parts = store_zoned_parts(when);
    let window = window_for(channel);
    (
        store_wall_time_to_utc(
            parts.year,
            parts.month,
            parts.day,
            window.start_hour,
            window.start_minute,
        ),
        store_wall_time_to_utc(
            parts.year,
            parts.month,
            parts.day,
            window.end_hour,
            window.end_minute,
        ),
    )
}

fn in_window(channel: FollowupChannel, at: Instant, start: Instant, end: Instant) -> bool {
    match channel {
        FollowupChannel::Sms => at >= start && at < end,
        FollowupChannel::Email => at >= start && at <= end,
    }
}

pub fn is_followup_send_window_open(channel: FollowupChannel, at: Instant) -> bool {
    let (start, end) = window_bounds_on_store_day(at, channel);
    in_window(channel, at, start, end)
}

fn thanksgiving_day(year: i32) -> i32 {
    let weekday = NaiveDate::from_ymd_opt(year, 11, 1)
        .map_or(0, |first| first.weekday().num_days_from_sunday());
    #[allow(clippy::cast_possible_wrap)]
    let weekday = weekday as i32;
    let first_thursday = if weekday <= 4 {
        1 + (4 - weekday)
    } else {
        1 + (11 - weekday)
    };
    first_thursday + 21
}

/// New Year's Day, July 4th, Christmas Eve and Day, Thanksgiving.
pub fn is_major_holiday_store_day(when: Instant) -> bool {
    let parts = store_zoned_parts(when);
    let (month, day) = (parts.month, parts.day);
    (month == 1 && day == 1)
        || (month == 7 && day == 4)
        || (month == 12 && (day == 24 || day == 25))
        || (month == 11 && day == thanksgiving_day(parts.year))
}

fn window_start_on(date: (i32, i32, i32), window: FollowupSendWindow) -> Instant {
    store_wall_time_to_utc(
        date.0,
        date.1,
        date.2,
        window.start_hour,
        window.start_minute,
    )
}

fn next_open_send_window_forward(when: Instant, channel: FollowupChannel) -> Instant {
    let window = window_for(channel);
    let mut cursor = when;
    for _ in 0..10 {
        let (start, end) = window_bounds_on_store_day(cursor, channel);
        let candidate = if in_window(channel, cursor, start, end) {
            cursor
        } else if cursor < start {
            start
        } else {
            window_start_on(add_store_calendar_days(cursor, 1), window)
        };
        if !is_major_holiday_store_day(candidate) {
            return candidate;
        }
        cursor = window_start_on(add_store_calendar_days(candidate, 1), window);
    }
    cursor
}

/// Before the window, wait until it opens; after it, until the next open day.
pub fn clamp_followup_send_at(when: Instant, channel: FollowupChannel) -> Instant {
    next_open_send_window_forward(when, channel)
}

/// AI follow-ups do not send on Sunday, except on the store day the
/// customer asked to be contacted (`allow_on`).
pub fn next_send_at_avoiding_sunday(
    when: Instant,
    channel: FollowupChannel,
    allow_on: Option<Instant>,
) -> Instant {
    let opened = clamp_followup_send_at(when, channel);
    if allow_on.is_some_and(|allow| same_store_calendar_day(opened, allow))
        || !is_store_sunday(opened)
    {
        return opened;
    }
    clamp_followup_send_at(
        window_start_on(add_store_calendar_days(opened, 1), window_for(channel)),
        channel,
    )
}

fn preferred_local_clock_from_send(from: Instant, channel: FollowupChannel) -> (i32, i32) {
    if is_followup_send_window_open(channel, from) {
        let parts = store_zoned_parts(from);
        return (parts.hour, parts.minute);
    }
    let window = window_for(channel);
    (window.start_hour, window.start_minute)
}

/// Whole-day gaps move by store calendar days and keep the local clock
/// (DST-safe); shorter gaps add minutes, then the window applies.
pub fn add_cadence_gap_from_previous(
    from: Instant,
    gap_minutes: f64,
    channel: FollowupChannel,
) -> Instant {
    let whole_days = gap_minutes / 1440.0;
    if whole_days >= 1.0 && whole_days.fract() == 0.0 {
        #[allow(clippy::cast_possible_truncation)]
        let target = add_store_calendar_days(from, whole_days as i32);
        let (hour, minute) = preferred_local_clock_from_send(from, channel);
        let candidate = store_wall_time_to_utc(target.0, target.1, target.2, hour, minute);
        return normalize_followup_send_at_forward(candidate, channel);
    }
    #[allow(clippy::cast_possible_truncation)]
    let millis = (gap_minutes * 60_000.0) as i64;
    clamp_followup_send_at(from + Duration::milliseconds(millis), channel)
}

/// Later cadence steps keep their gap unless they land on a closed holiday.
pub fn clamp_followup_send_at_if_holiday(when: Instant, channel: FollowupChannel) -> Instant {
    if is_major_holiday_store_day(when) {
        next_open_send_window_forward(when, channel)
    } else {
        when
    }
}

pub fn followup_next_send_at(
    from: Instant,
    delay_minutes: f64,
    channel: FollowupChannel,
) -> Instant {
    add_cadence_gap_from_previous(from, delay_minutes, channel)
}

/// A step of a sequence: its gap and channel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StepTiming {
    pub delay_minutes: f64,
    pub channel: FollowupChannel,
}

fn ceil_minutes(later: Instant, earlier: Instant) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let ms = (later - earlier).num_milliseconds() as f64;
    (ms / 60_000.0).ceil()
}

/// Relative gaps for an AI sequence, moved off Sunday except `allow_on`.
pub fn followup_delay_minutes_avoiding_sunday(
    steps: &[StepTiming],
    from: Instant,
    allow_on: Option<Instant>,
) -> Vec<f64> {
    let mut delays = Vec::with_capacity(steps.len());
    let mut cursor = from;
    for step in steps {
        let landed = followup_next_send_at(cursor, step.delay_minutes, step.channel);
        let send_at = next_send_at_avoiding_sunday(landed, step.channel, allow_on);
        delays.push(ceil_minutes(send_at, cursor).max(0.0));
        cursor = send_at;
    }
    delays
}

/// List-drip / automated emails use the same store-local email window.
pub fn scheduled_automated_email_send_at(hour_delay: f64, now: Instant) -> Instant {
    #[allow(clippy::cast_possible_truncation)]
    let ms = (hour_delay * 3_600_000.0) as i64;
    clamp_followup_send_at(now + Duration::milliseconds(ms), FollowupChannel::Email)
}

/// List-drip / automated SMS use the store-local SMS window.
pub fn scheduled_automated_sms_send_at(hour_delay: f64, now: Instant) -> Instant {
    #[allow(clippy::cast_possible_truncation)]
    let ms = (hour_delay * 3_600_000.0) as i64;
    clamp_followup_send_at(now + Duration::milliseconds(ms), FollowupChannel::Sms)
}

pub fn should_send_followup_immediately(
    delay_minutes: f64,
    channel: FollowupChannel,
    now: Instant,
) -> bool {
    delay_minutes == 0.0 && is_followup_send_window_open(channel, now)
}

str_enum! { pub enum FollowupGreetingKind { GoodMorning = "Good morning", GoodEvening = "Good evening", Daytime = "daytime" } }

pub const fn followup_greeting_kind(hour: i32) -> FollowupGreetingKind {
    if hour < 12 {
        FollowupGreetingKind::GoodMorning
    } else if hour >= 18 {
        FollowupGreetingKind::GoodEvening
    } else {
        FollowupGreetingKind::Daytime
    }
}

/// `3:05 PM`.
pub fn format_store_clock(hour: i32, minute: i32) -> String {
    let hour12 = if hour % 12 == 0 { 12 } else { hour % 12 };
    format!(
        "{hour12}:{minute:02} {}",
        if hour < 12 { "AM" } else { "PM" }
    )
}

pub const fn greeting_instruction_for_hour(hour: i32) -> &'static str {
    match followup_greeting_kind(hour) {
        FollowupGreetingKind::GoodMorning => {
            "Open with Good morning, Hi, or Hey — pick one. Do not use Good evening."
        }
        FollowupGreetingKind::GoodEvening => {
            "Open with Good evening, Hi, or Hey — pick one. Do not use Good morning."
        }
        FollowupGreetingKind::Daytime => {
            "Daytime (noon–6pm): open with Hi or Hey. Do not use Good morning or Good evening."
        }
    }
}

str_enum! { pub enum FollowupOpener { GoodMorning = "Good morning", GoodEvening = "Good evening", Hi = "Hi", Hey = "Hey" } }

pub fn greetings_allowed_at_hour(hour: i32) -> Vec<FollowupOpener> {
    use FollowupOpener as O;
    if hour < 12 {
        vec![O::GoodMorning, O::Hey, O::Hi]
    } else if hour >= 18 {
        vec![O::GoodEvening, O::Hey, O::Hi]
    } else {
        vec![O::Hey, O::Hi]
    }
}

/// A different opener for each message where the hour allows it.
pub fn assign_unique_greetings(hours: &[i32]) -> Vec<FollowupOpener> {
    let mut assigned: Vec<FollowupOpener> = Vec::new();
    let mut used: Vec<FollowupOpener> = Vec::new();
    for hour in hours {
        let allowed = greetings_allowed_at_hour(*hour);
        let previous = assigned.last().copied();
        let pick = allowed
            .iter()
            .find(|opener| !used.contains(opener))
            .or_else(|| allowed.iter().find(|opener| Some(**opener) != previous))
            .or_else(|| allowed.first())
            .copied()
            .unwrap_or(FollowupOpener::Hi);
        assigned.push(pick);
        if !used.contains(&pick) {
            used.push(pick);
        }
    }
    assigned
}

pub fn greeting_instruction_for_opener(opener: FollowupOpener) -> String {
    format!(
        "Open with \"{opener}\" — do not use this same greeting on any other message in the sequence."
    )
}

pub fn expected_followup_send_ats(steps: &[StepTiming], from: Instant) -> Vec<Instant> {
    let mut cursor = from;
    steps
        .iter()
        .map(|step| {
            cursor = followup_next_send_at(cursor, step.delay_minutes, step.channel);
            cursor
        })
        .collect()
}

/// Final send-window normalization: only moves times forward.
pub fn normalize_followup_send_at_forward(when: Instant, channel: FollowupChannel) -> Instant {
    clamp_followup_send_at(when, channel)
}

pub fn assert_followup_send_schedule_in_window(
    steps: &[StepTiming],
    from: Instant,
) -> Vec<Instant> {
    expected_followup_send_ats(steps, from)
        .into_iter()
        .zip(steps)
        .map(|(at, step)| {
            if is_followup_send_window_open(step.channel, at) {
                at
            } else {
                normalize_followup_send_at_forward(at, step.channel)
            }
        })
        .collect()
}

/// Minutes from `from` so the next send lands on the chosen store day at the
/// preferred clock (clamped into the window), between 0 and the step maximum.
pub fn delay_minutes_for_store_send_day(
    from: Instant,
    channel: FollowupChannel,
    date_input: &str,
    hour: i32,
    minute: i32,
) -> i64 {
    let Some(caps) = crate::re!(r"^(\d{4})-(\d{2})-(\d{2})$").captures(super::js::trim(date_input))
    else {
        return 0;
    };
    let number = |i: usize| caps[i].parse::<i32>().unwrap_or(0);
    let desired = clamp_followup_send_at(
        store_wall_time_to_utc(number(1), number(2), number(3), hour, minute),
        channel,
    );
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    let minutes = super::js::js_round((desired - from).num_milliseconds() as f64 / 60_000.0) as i64;
    minutes.clamp(0, MAX_DELAY_MINUTES)
}

const MONTH_SHORT: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// "sends tomorrow ~8:30 AM": when the follow-up actually goes out.
pub fn format_followup_send_label(at: Instant, now: Instant) -> String {
    if (at - now).num_milliseconds() <= 60_000 {
        return "sends now".into();
    }
    let parts = store_zoned_parts(at);
    let now_parts = store_zoned_parts(now);
    let clock = format_store_clock(parts.hour, parts.minute);
    if (parts.year, parts.month, parts.day) == (now_parts.year, now_parts.month, now_parts.day) {
        return format!("sends today ~{clock}");
    }
    if (parts.year, parts.month, parts.day) == add_store_calendar_days(now, 1) {
        return format!("sends tomorrow ~{clock}");
    }
    let month = usize::try_from(parts.month - 1)
        .ok()
        .and_then(|index| MONTH_SHORT.get(index))
        .map_or_else(|| parts.month.to_string(), |m| (*m).to_string());
    format!("sends {month} {} ~{clock}", parts.day)
}

/// Time-only label when the date is shown elsewhere.
pub fn format_followup_send_time_label(at: Instant, now: Instant) -> String {
    if (at - now).num_milliseconds() <= 60_000 {
        return "now".into();
    }
    let parts = store_zoned_parts(at);
    format!("~{}", format_store_clock(parts.hour, parts.minute))
}
