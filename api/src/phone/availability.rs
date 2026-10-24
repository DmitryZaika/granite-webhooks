//! Whether a user's phones ring now: Do not disturb (until a time, or until
//! turned off) and working hours in the user's own time zone.

use super::clock::time_zone;
use super::wire_enum;
use crate::serde_helpers::js_date;
use chrono::{DateTime, Datelike, Timelike, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Most time ranges working hours may list.
const MAX_RANGES: usize = 28;

/// When a user takes calls (null in the API: always). Outside these hours
/// their phones do not ring; calls go to the number's fallback and still
/// show in Recents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct WorkingHours {
    /// IANA time zone the hours are in, e.g. `America/Indiana/Indianapolis`.
    pub tz: String,
    /// The time ranges, any number per weekday (at most 28 in all, at least
    /// one); a weekday without a range is a day off.
    pub days: Vec<WorkingDay>,
}

/// One time range of a weekday.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct WorkingDay {
    /// The weekday: 0 = Sunday, 1 = Monday ... 6 = Saturday.
    pub day: u8,
    /// Local start, `HH:MM` (24-hour), e.g. `08:00`.
    pub start: String,
    /// Local end, `HH:MM`, after `start`; `24:00` = midnight at the end of
    /// the day.
    pub end: String,
}

/// Minutes after midnight of `HH:MM`; `24:00` only when `end` is set.
fn minutes(text: &str, end: bool) -> Option<u32> {
    let (hours, mins) = text.split_once(':')?;
    if hours.len() != 2 || mins.len() != 2 {
        return None;
    }
    let (hours, mins): (u32, u32) = (hours.parse().ok()?, mins.parse().ok()?);
    match (hours, mins) {
        (0..=23, 0..=59) => Some(hours * 60 + mins),
        (24, 0) if end => Some(24 * 60),
        _ => None,
    }
}

impl WorkingHours {
    /// Why these hours cannot be saved, as a message for the user.
    pub fn problem(&self) -> Option<String> {
        if time_zone(&self.tz).is_none() {
            return Some(format!("Unknown time zone: {}", self.tz.trim()));
        }
        if self.days.is_empty() {
            return Some("Add at least one day, or clear the working hours.".into());
        }
        if self.days.len() > MAX_RANGES {
            return Some(format!("At most {MAX_RANGES} time ranges."));
        }
        for range in &self.days {
            if range.day > 6 {
                return Some("Days are 0 (Sunday) to 6 (Saturday).".into());
            }
            match (minutes(&range.start, false), minutes(&range.end, true)) {
                (Some(start), Some(end)) if start < end => {}
                _ => {
                    return Some(format!(
                        "Each range needs a start before its end, as HH:MM (got {} to {}).",
                        range.start, range.end
                    ));
                }
            }
        }
        None
    }

    /// `at` falls in one of the ranges (in the hours' time zone). Unreadable
    /// hours never match.
    pub fn contains(&self, at: DateTime<Utc>) -> bool {
        let Some(tz) = time_zone(&self.tz) else {
            return false;
        };
        let local = at.with_timezone(&tz);
        let weekday = local.weekday().num_days_from_sunday();
        let now = local.hour() * 60 + local.minute();
        self.days.iter().any(|range| {
            u32::from(range.day) == weekday
                && matches!(
                    (minutes(&range.start, false), minutes(&range.end, true)),
                    (Some(start), Some(end)) if start <= now && now < end
                )
        })
    }

    /// Hours as stored (`working_hours`, `business_hours` JSON); `None` when
    /// unreadable.
    pub fn from_json(json: &str) -> Option<Self> {
        serde_json::from_str(json).ok()
    }

    /// The JSON stored in the database.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "null".into())
    }
}

wire_enum! {
    /// Whether the user's phones ring now: `available`, `dnd` (Do not
    /// disturb) or `outside_hours` (outside their working hours).
    pub enum AvailabilityStatus {
        /// Calls ring the user's phones.
        Available = "available",
        /// Do not disturb: calls go to the fallback.
        Dnd = "dnd",
        /// Outside the user's working hours: calls go to the fallback.
        OutsideHours = "outside_hours",
    }
}

/// The user's availability right now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Availability {
    /// `available`, `dnd` or `outside_hours`. Do not disturb wins over
    /// working hours.
    pub status: AvailabilityStatus,
    /// When a timed Do not disturb ends (RFC 3339, UTC); null when Do not
    /// disturb is off or lasts until turned off.
    #[serde(serialize_with = "js_date")]
    pub dnd_until: Option<DateTime<Utc>>,
    /// Do not disturb is on until the user turns it off.
    pub dnd_indefinite: bool,
}

impl Availability {
    /// The availability at `now` of a user with these settings.
    pub fn at(
        now: DateTime<Utc>,
        dnd_until: Option<DateTime<Utc>>,
        dnd_indefinite: bool,
        hours: Option<&WorkingHours>,
    ) -> Self {
        if dnd_indefinite {
            return Self {
                status: AvailabilityStatus::Dnd,
                dnd_until: None,
                dnd_indefinite: true,
            };
        }
        if let Some(until) = dnd_until.filter(|until| *until > now) {
            return Self {
                status: AvailabilityStatus::Dnd,
                dnd_until: Some(until),
                dnd_indefinite: false,
            };
        }
        let status = match hours {
            Some(hours) if !hours.contains(now) => AvailabilityStatus::OutsideHours,
            _ => AvailabilityStatus::Available,
        };
        Self {
            status,
            dnd_until: None,
            dnd_indefinite: false,
        }
    }

    /// The user's phones ring.
    pub fn rings(&self) -> bool {
        self.status == AvailabilityStatus::Available
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn hours(days: &[(u8, &str, &str)]) -> WorkingHours {
        WorkingHours {
            tz: "America/Indiana/Indianapolis".into(),
            days: days
                .iter()
                .map(|(day, start, end)| WorkingDay {
                    day: *day,
                    start: (*start).into(),
                    end: (*end).into(),
                })
                .collect(),
        }
    }

    #[test]
    fn hours_are_checked_in_their_time_zone() {
        // Monday to Friday 08:00-17:00, Saturday until midnight.
        let week = hours(&[
            (1, "08:00", "17:00"),
            (2, "08:00", "17:00"),
            (3, "08:00", "17:00"),
            (4, "08:00", "17:00"),
            (5, "08:00", "17:00"),
            (6, "20:00", "24:00"),
        ]);
        assert!(week.problem().is_none());
        // Wednesday 2026-10-14, 08:00 in Indianapolis (12:00 UTC).
        assert!(week.contains(utc("2026-10-14T12:00:00Z")));
        assert!(!week.contains(utc("2026-10-14T11:59:00Z")));
        assert!(week.contains(utc("2026-10-14T20:59:00Z")));
        assert!(!week.contains(utc("2026-10-14T21:00:00Z")));
        // Sunday: a day off.
        assert!(!week.contains(utc("2026-10-18T15:00:00Z")));
        // Saturday 23:59 local is Sunday 03:59 UTC.
        assert!(week.contains(utc("2026-10-18T03:59:00Z")));
    }

    #[test]
    fn bad_hours_are_refused() {
        assert!(hours(&[]).problem().is_some());
        assert!(hours(&[(7, "08:00", "17:00")]).problem().is_some());
        assert!(hours(&[(1, "17:00", "08:00")]).problem().is_some());
        assert!(hours(&[(1, "8:00", "17:00")]).problem().is_some());
        assert!(hours(&[(1, "24:00", "24:00")]).problem().is_some());
        assert!(hours(&[(1, "08:00", "17:60")]).problem().is_some());
        assert!(hours(&[(1, "00:00", "24:00")]).problem().is_none());
        let mut mars = hours(&[(1, "08:00", "17:00")]);
        mars.tz = "Mars/Olympus".into();
        assert_eq!(
            mars.problem().as_deref(),
            Some("Unknown time zone: Mars/Olympus")
        );
    }

    #[test]
    fn do_not_disturb_wins_over_hours() {
        let now = utc("2026-10-14T12:00:00Z");
        let office = hours(&[(3, "08:00", "17:00")]);
        let available = Availability::at(now, None, false, Some(&office));
        assert_eq!(available.status, AvailabilityStatus::Available);
        assert!(available.rings());
        let later = utc("2026-10-14T13:00:00Z");
        assert_eq!(
            Availability::at(now, Some(later), false, Some(&office)),
            Availability {
                status: AvailabilityStatus::Dnd,
                dnd_until: Some(later),
                dnd_indefinite: false
            }
        );
        // A Do not disturb that ended is off.
        let ended = Availability::at(now, Some(utc("2026-10-14T11:00:00Z")), false, None);
        assert_eq!(ended.status, AvailabilityStatus::Available);
        assert_eq!(ended.dnd_until, None);
        assert_eq!(
            Availability::at(now, None, true, None),
            Availability {
                status: AvailabilityStatus::Dnd,
                dnd_until: None,
                dnd_indefinite: true
            }
        );
        let evening = utc("2026-10-14T22:00:00Z");
        let outside = Availability::at(evening, None, false, Some(&office));
        assert_eq!(outside.status, AvailabilityStatus::OutsideHours);
        assert!(!outside.rings());
    }
}
