//! Calendar periods the dashboard counts in.
//!
//! "Today" is the UTC date, the date the Lambda runs in (Remix used the Node
//! process's local date, which is UTC in production as well).

use chrono::{Datelike, Months, NaiveDate, Utc};

/// The UTC date right now.
pub fn today() -> NaiveDate {
    Utc::now().date_naive()
}

/// The current month so far: from its 1st through `today`, both inclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonthToDate {
    pub start: NaiveDate,
    pub end: NaiveDate,
}

impl MonthToDate {
    pub fn containing(today: NaiveDate) -> Self {
        Self {
            start: first_of_month(today),
            end: today,
        }
    }
}

/// The 1st of the month `months_back` months before `today`'s month
/// (0 = this month).
pub fn month_start_back(today: NaiveDate, months_back: u32) -> NaiveDate {
    first_of_month(today)
        .checked_sub_months(Months::new(months_back))
        .expect("a month start is always representable")
}

fn first_of_month(date: NaiveDate) -> NaiveDate {
    date.with_day(1).expect("every month has a 1st")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn month_to_date_runs_from_the_first() {
        assert_eq!(
            MonthToDate::containing(date(2026, 10, 11)),
            MonthToDate {
                start: date(2026, 10, 1),
                end: date(2026, 10, 11)
            }
        );
    }

    #[test]
    fn month_start_back_crosses_years() {
        assert_eq!(month_start_back(date(2026, 1, 31), 0), date(2026, 1, 1));
        assert_eq!(month_start_back(date(2026, 1, 31), 2), date(2025, 11, 1));
    }
}
