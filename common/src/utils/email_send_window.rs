use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Utc};
use chrono_tz::America::Indiana::Indianapolis;

const START_HOUR: u32 = 8;
const START_MINUTE: u32 = 30;
const END_HOUR: u32 = 19;
const END_MINUTE: u32 = 0;

fn store_wall_to_utc(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
    let naive = NaiveDate::from_ymd_opt(year, month, day)
        .and_then(|date| date.and_hms_opt(hour, minute, 0))
        .expect("valid store wall time");
    match Indianapolis.from_local_datetime(&naive) {
        chrono::LocalResult::Single(local) => local.with_timezone(&Utc),
        chrono::LocalResult::Ambiguous(earliest, _) => earliest.with_timezone(&Utc),
        chrono::LocalResult::None => Indianapolis
            .from_utc_datetime(&naive)
            .with_timezone(&Utc),
    }
}

fn thanksgiving_month_day(year: i32) -> (u32, u32) {
    let first = NaiveDate::from_ymd_opt(year, 11, 1).expect("valid November 1");
    let weekday = first.weekday().num_days_from_sunday();
    let first_thursday = if weekday <= 4 {
        1 + (4 - weekday)
    } else {
        1 + (11 - weekday)
    };
    (11, first_thursday + 21)
}

fn is_major_holiday_store_day(when: DateTime<Utc>) -> bool {
    let local = when.with_timezone(&Indianapolis);
    let month = local.month();
    let day = local.day();
    if month == 1 && day == 1 {
        return true;
    }
    if month == 7 && day == 4 {
        return true;
    }
    if month == 12 && (day == 24 || day == 25) {
        return true;
    }
    let (thanks_month, thanks_day) = thanksgiving_month_day(local.year());
    month == thanks_month && day == thanks_day
}

fn window_start_on(when: DateTime<Utc>) -> DateTime<Utc> {
    let local = when.with_timezone(&Indianapolis);
    store_wall_to_utc(
        local.year(),
        local.month(),
        local.day(),
        START_HOUR,
        START_MINUTE,
    )
}

fn window_end_on(when: DateTime<Utc>) -> DateTime<Utc> {
    let local = when.with_timezone(&Indianapolis);
    store_wall_to_utc(
        local.year(),
        local.month(),
        local.day(),
        END_HOUR,
        END_MINUTE,
    )
}

pub fn is_automated_email_send_window_open(when: DateTime<Utc>) -> bool {
    let start = window_start_on(when);
    let end = window_end_on(when);
    when >= start && when <= end && !is_major_holiday_store_day(when)
}

pub fn automated_email_send_window_is_open_now() -> bool {
    is_automated_email_send_window_open(Utc::now())
}

pub fn clamp_automated_email_send_at(when: DateTime<Utc>) -> DateTime<Utc> {
    let mut cursor = when;
    for _ in 0..10 {
        let start = window_start_on(cursor);
        let end = window_end_on(cursor);
        let candidate = if cursor >= start && cursor <= end {
            cursor
        } else if cursor < start {
            start
        } else {
            let next = cursor.with_timezone(&Indianapolis).date_naive() + Duration::days(1);
            store_wall_to_utc(next.year(), next.month(), next.day(), START_HOUR, START_MINUTE)
        };
        if !is_major_holiday_store_day(candidate) {
            return candidate;
        }
        let after_holiday =
            candidate.with_timezone(&Indianapolis).date_naive() + Duration::days(1);
        cursor = store_wall_to_utc(
            after_holiday.year(),
            after_holiday.month(),
            after_holiday.day(),
            START_HOUR,
            START_MINUTE,
        );
    }
    cursor
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(iso: &str) -> DateTime<Utc> {
        iso.parse().expect("valid rfc3339")
    }

    #[test]
    fn seven_am_list_join_waits_until_eight_thirty() {
        assert_eq!(
            clamp_automated_email_send_at(utc("2026-07-28T11:00:00Z")),
            utc("2026-07-28T12:30:00Z")
        );
    }

    #[test]
    fn eight_am_waits_until_eight_thirty() {
        assert!(!is_automated_email_send_window_open(utc(
            "2026-07-28T12:00:00Z"
        )));
        assert_eq!(
            clamp_automated_email_send_at(utc("2026-07-28T12:00:00Z")),
            utc("2026-07-28T12:30:00Z")
        );
    }

    #[test]
    fn eight_thirty_is_open() {
        let at = utc("2026-07-28T12:30:00Z");
        assert!(is_automated_email_send_window_open(at));
        assert_eq!(clamp_automated_email_send_at(at), at);
    }

    #[test]
    fn ten_am_stays() {
        let at = utc("2026-07-28T14:00:00Z");
        assert_eq!(clamp_automated_email_send_at(at), at);
    }

    #[test]
    fn after_seven_pm_waits_until_next_morning() {
        assert_eq!(
            clamp_automated_email_send_at(utc("2026-07-27T23:01:00Z")),
            utc("2026-07-28T12:30:00Z")
        );
    }
}
