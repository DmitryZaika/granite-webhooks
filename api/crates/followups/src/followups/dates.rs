//! JS `Date` arithmetic the engine relies on: `Date.UTC` normalizes
//! overflowing months and days (Jan 31 + 1 month is Mar 3), and dates are
//! instants in UTC.

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, TimeZone, Utc};

pub type Instant = DateTime<Utc>;

/// `new Date(Date.UTC(year, month_index, day, h, m, s))`, with JS overflow rules.
pub fn utc(year: i32, month_index: i32, day: i32, hour: i64, minute: i64, second: i64) -> Instant {
    // `Date.UTC` reads years 0-99 as 1900-1999.
    let year = if (0..=99).contains(&year) {
        year + 1900
    } else {
        year
    };
    let year = year + month_index.div_euclid(12);
    let month = u32::try_from(month_index.rem_euclid(12)).unwrap_or(0) + 1;
    // Out-of-range dates (an Invalid Date in JS) clamp instead of panicking.
    let clamp = |forward: bool| {
        if forward {
            NaiveDateTime::MAX
        } else {
            NaiveDateTime::MIN
        }
    };
    let first = NaiveDate::from_ymd_opt(year, month, 1).unwrap_or_else(|| clamp(year > 0).date());
    let at = Duration::try_days(i64::from(day) - 1)
        .and_then(|days| first.checked_add_signed(days))
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .and_then(|at| at.checked_add_signed(Duration::try_hours(hour)?))
        .and_then(|at| at.checked_add_signed(Duration::try_minutes(minute)?))
        .and_then(|at| at.checked_add_signed(Duration::try_seconds(second)?))
        .unwrap_or_else(|| clamp(day > 0 || hour > 0 || minute > 0 || second > 0));
    Utc.from_utc_datetime(&at)
}

/// `new Date(Date.UTC(year, month_index, day))`.
pub fn utc_date(year: i32, month_index: i32, day: i32) -> Instant {
    utc(year, month_index, day, 0, 0, 0)
}

/// `getUTCFullYear()`.
pub fn year(at: Instant) -> i32 {
    at.year()
}

/// `getUTCMonth()` (0-11).
pub fn month_index(at: Instant) -> i32 {
    i32::try_from(at.month0()).unwrap_or(0)
}

/// `getUTCDate()`.
pub fn day(at: Instant) -> i32 {
    i32::try_from(at.day()).unwrap_or(1)
}

/// Days in a month (`new Date(Date.UTC(y, m + 1, 0)).getUTCDate()`).
pub fn last_day_of_month(year: i32, month_index: i32) -> i32 {
    day(utc_date(year, month_index + 1, 0))
}

/// `Date.UTC(y, m + months, d)` from an instant's UTC date.
pub fn add_months(base: Instant, months: i32) -> Instant {
    utc_date(year(base), month_index(base) + months, day(base))
}

/// JS `new Date(text)` for `YYYY-MM-DDTHH:MM` (no zone: the server's local
/// time, which is UTC on the CRM's servers).
pub fn parse_local_minutes(text: &str) -> Option<Instant> {
    let at = NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M").ok()?;
    Some(Utc.from_utc_datetime(&at))
}

/// JS `Date.parse` for ISO strings the engine stores (`2026-05-01T10:00:00.000Z`,
/// `2026-05-01T10:00:00Z`, `2026-05-01 10:00:00`, `2026-05-01`).
pub fn parse_iso(text: &str) -> Option<Instant> {
    let text = text.trim();
    if let Ok(at) = DateTime::parse_from_rfc3339(text) {
        return Some(at.with_timezone(&Utc));
    }
    for format in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(at) = NaiveDateTime::parse_from_str(text, format) {
            return Some(Utc.from_utc_datetime(&at));
        }
    }
    // A date alone is UTC midnight in JS.
    NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .ok()
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .map(|at| Utc.from_utc_datetime(&at))
}

/// `date.toISOString()`.
pub fn iso(at: Instant) -> String {
    at.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// JS `Date.parse` for the timestamps the engine sees, in milliseconds.
///
/// `YYYY-M-D`, then optional `T`/space `H:MM[:SS[.fff]]` and a zone (`Z`,
/// `±HH:MM`). Without a zone a date-time is the server's local time (UTC);
/// a date alone is UTC. Like V8, a day past the month's end rolls over
/// (Feb 30 is Mar 2) and 24:00 is the next midnight.
pub fn js_date_parse(text: &str) -> Option<i64> {
    let caps = crate::re!(
        r"^\s*([+-]?\d{4,6})-(\d{1,2})(?:-(\d{1,2}))?(?:[T ](\d{1,2}):(\d{2})(?::(\d{2})(?:[.,](\d+))?)?)?\s*(Z|[+-]\d{2}:?\d{2})?\s*$",
        "i"
    )
    .captures(text)?;
    let num = |i: usize| caps.get(i).map_or(Ok(0), |m| m.as_str().parse::<i64>());
    let year = i32::try_from(num(1).ok()?).ok()?;
    let month = num(2).ok()?;
    let day = caps
        .get(3)
        .map_or(Ok(1), |m| m.as_str().parse::<i64>())
        .ok()?;
    let (hour, minute, second) = (num(4).ok()?, num(5).ok()?, num(6).ok()?);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 24
        || minute > 59
        || second > 59
    {
        return None;
    }
    if hour == 24 && (minute != 0 || second != 0) {
        return None;
    }
    let millis = caps.get(7).map_or(0, |m| {
        let digits: String = m.as_str().chars().take(3).collect();
        format!("{digits:0<3}").parse::<i64>().unwrap_or(0)
    });
    let offset_minutes = caps.get(8).map_or(Some(0), |zone| {
        let zone = zone.as_str();
        if zone.eq_ignore_ascii_case("z") {
            return Some(0);
        }
        let sign = if zone.starts_with('-') { -1 } else { 1 };
        let digits: String = zone.chars().filter(char::is_ascii_digit).collect();
        let hours: i64 = digits.get(..2)?.parse().ok()?;
        let minutes: i64 = digits.get(2..)?.parse().ok()?;
        Some(sign * (hours * 60 + minutes))
    })?;
    let month_index = i32::try_from(month - 1).ok()?;
    let at = utc(
        year,
        month_index,
        i32::try_from(day).ok()?,
        hour,
        minute,
        second,
    );
    Some(at.timestamp_millis() + millis - offset_minutes * 60_000)
}

/// An instant from JS milliseconds.
pub const fn from_millis(ms: i64) -> Option<Instant> {
    DateTime::from_timestamp_millis(ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn months_and_days_overflow_like_js() {
        assert_eq!(iso(utc_date(2026, 0, 31)), "2026-01-31T00:00:00.000Z");
        assert_eq!(
            iso(add_months(utc_date(2026, 0, 31), 1)),
            "2026-03-03T00:00:00.000Z"
        );
        assert_eq!(iso(utc_date(2026, 12, 1)), "2027-01-01T00:00:00.000Z");
        assert_eq!(iso(utc_date(2026, -1, 1)), "2025-12-01T00:00:00.000Z");
        assert_eq!(iso(utc_date(2026, 4, 31)), "2026-05-31T00:00:00.000Z");
        assert_eq!(last_day_of_month(2028, 1), 29);
        assert_eq!(
            iso(utc(2026, 8, 15, 23, 59, 59)),
            "2026-09-15T23:59:59.000Z"
        );
    }

    #[test]
    fn iso_strings_parse() {
        assert_eq!(
            iso(parse_iso("2026-05-01T10:00:00.000Z").unwrap()),
            "2026-05-01T10:00:00.000Z"
        );
        assert_eq!(
            iso(parse_iso("2026-05-01 10:00:00").unwrap()),
            "2026-05-01T10:00:00.000Z"
        );
        assert_eq!(
            iso(parse_iso("2026-05-01").unwrap()),
            "2026-05-01T00:00:00.000Z"
        );
        assert!(parse_iso("soon").is_none());
    }
}
