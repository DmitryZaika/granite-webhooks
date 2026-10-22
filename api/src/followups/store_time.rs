//! Store-local (Indianapolis) wall clock, computed like the CRM does
//! (`lib/dateHelpers.ts`): a wall time maps to UTC by a few correction
//! steps, so DST gaps resolve the same way.

use super::dates::{Instant, utc};
use chrono::{Datelike, Duration, Timelike, Weekday};
use chrono_tz::America::Indiana::Indianapolis;

pub const STORE_BUSINESS_TIMEZONE: &str = "America/Indiana/Indianapolis";

/// A store-local calendar date and clock time (month 1-12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZonedParts {
    pub year: i32,
    pub month: i32,
    pub day: i32,
    pub hour: i32,
    pub minute: i32,
}

#[allow(clippy::cast_possible_wrap)]
pub fn store_zoned_parts(at: Instant) -> ZonedParts {
    let local = at.with_timezone(&Indianapolis);
    ZonedParts {
        year: local.year(),
        month: local.month() as i32,
        day: local.day() as i32,
        hour: local.hour() as i32,
        minute: local.minute() as i32,
    }
}

/// The store calendar date `days` after `from`'s store date.
pub fn add_store_calendar_days(from: Instant, days: i32) -> (i32, i32, i32) {
    let parts = store_zoned_parts(from);
    let shifted = utc(parts.year, parts.month - 1, parts.day + days, 0, 0, 0);
    (
        shifted.year(),
        super::dates::month_index(shifted) + 1,
        super::dates::day(shifted),
    )
}

/// The UTC instant of a store wall time (month 1-12).
pub fn store_wall_time_to_utc(year: i32, month: i32, day: i32, hour: i32, minute: i32) -> Instant {
    let mut at = utc(year, month - 1, day, i64::from(hour), i64::from(minute), 0);
    for _ in 0..5 {
        let actual = store_zoned_parts(at);
        let diff_minutes = i64::from(hour - actual.hour) * 60
            + i64::from(minute - actual.minute)
            + i64::from(day - actual.day) * 24 * 60
            + i64::from(month - actual.month) * 31 * 24 * 60
            + i64::from(year - actual.year) * 366 * 24 * 60;
        if diff_minutes == 0 {
            break;
        }
        at += Duration::minutes(diff_minutes);
    }
    at
}

pub fn is_store_sunday(at: Instant) -> bool {
    at.with_timezone(&Indianapolis).weekday() == Weekday::Sun
}

pub fn same_store_calendar_day(left: Instant, right: Instant) -> bool {
    let (a, b) = (store_zoned_parts(left), store_zoned_parts(right));
    (a.year, a.month, a.day) == (b.year, b.month, b.day)
}

/// `YYYY-MM-DD` of the store date.
pub fn store_date_input_value(at: Instant) -> String {
    let parts = store_zoned_parts(at);
    format!("{}-{:02}-{:02}", parts.year, parts.month, parts.day)
}

/// `HH:MM` (24h) of the store clock.
pub fn store_time_input_value(at: Instant) -> String {
    let parts = store_zoned_parts(at);
    format!("{:02}:{:02}", parts.hour, parts.minute)
}
