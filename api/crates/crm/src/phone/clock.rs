//! Local days, weeks and months in a company's time zone, as UTC instants
//! (call stats, the daily call limit).

use chrono::{DateTime, Datelike, Days, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;

/// The time zone named `name` (IANA, e.g. `America/Indiana/Indianapolis`).
pub fn time_zone(name: &str) -> Option<Tz> {
    name.trim().parse().ok()
}

/// The first instant of local date `date` in `tz`. Where midnight does not
/// exist (a daylight-saving jump at 00:00), the first instant after it.
pub fn start_of(tz: Tz, date: NaiveDate) -> DateTime<Utc> {
    let midnight = date.and_hms_opt(0, 0, 0).unwrap_or(NaiveDateTime::MIN);
    (0..=3)
        .find_map(|hour| {
            tz.from_local_datetime(&(midnight + chrono::Duration::hours(hour)))
                .earliest()
        })
        .map_or_else(|| midnight.and_utc(), |local| local.with_timezone(&Utc))
}

/// When today began in `tz`.
pub fn day_start(tz: Tz, now: DateTime<Utc>) -> DateTime<Utc> {
    start_of(tz, now.with_timezone(&tz).date_naive())
}

/// When this week began in `tz` (weeks start on Monday).
pub fn week_start(tz: Tz, now: DateTime<Utc>) -> DateTime<Utc> {
    let today = now.with_timezone(&tz).date_naive();
    let monday = today
        .checked_sub_days(Days::new(u64::from(today.weekday().num_days_from_monday())))
        .unwrap_or(today);
    start_of(tz, monday)
}

/// When this month began in `tz`.
pub fn month_start(tz: Tz, now: DateTime<Utc>) -> DateTime<Utc> {
    let today = now.with_timezone(&tz).date_naive();
    start_of(tz, today.with_day(1).unwrap_or(today))
}

/// When tomorrow begins in `tz`.
pub fn next_day_start(tz: Tz, now: DateTime<Utc>) -> DateTime<Utc> {
    let today = now.with_timezone(&tz).date_naive();
    start_of(tz, today.checked_add_days(Days::new(1)).unwrap_or(today))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn periods_start_at_local_midnight() {
        let indy = time_zone("America/Indiana/Indianapolis").unwrap();
        // Wednesday 2026-10-14, 01:30 in Indianapolis (EDT, UTC-4).
        let now = utc("2026-10-14T05:30:00Z");
        assert_eq!(day_start(indy, now), utc("2026-10-14T04:00:00Z"));
        assert_eq!(week_start(indy, now), utc("2026-10-12T04:00:00Z"));
        assert_eq!(month_start(indy, now), utc("2026-10-01T04:00:00Z"));
        assert_eq!(next_day_start(indy, now), utc("2026-10-15T04:00:00Z"));
        // 00:30 UTC on the 14th is still the 13th there.
        assert_eq!(
            day_start(indy, utc("2026-10-14T00:30:00Z")),
            utc("2026-10-13T04:00:00Z")
        );
        // After the switch to EST (UTC-5).
        assert_eq!(
            day_start(indy, utc("2026-11-02T12:00:00Z")),
            utc("2026-11-02T05:00:00Z")
        );
        // A Monday is its own week start; a Sunday belongs to the week before.
        assert_eq!(
            week_start(indy, utc("2026-10-12T15:00:00Z")),
            utc("2026-10-12T04:00:00Z")
        );
        assert_eq!(
            week_start(indy, utc("2026-10-18T15:00:00Z")),
            utc("2026-10-12T04:00:00Z")
        );
        assert!(time_zone("Mars/Olympus").is_none());
    }

    #[test]
    fn a_missing_midnight_starts_the_day_after_the_jump() {
        // Havana springs forward at midnight: 2026-03-08 begins at 01:00.
        let havana = time_zone("America/Havana").unwrap();
        assert_eq!(
            start_of(havana, NaiveDate::from_ymd_opt(2026, 3, 8).unwrap()),
            utc("2026-03-08T05:00:00Z")
        );
    }
}
