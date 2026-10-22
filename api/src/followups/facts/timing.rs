//! When the customer will be ready, and facts that expire (part of
//! `smsFlowAiFollowupFacts.ts`).

use super::super::dates::{
    Instant, add_months, day, last_day_of_month, month_index, parse_local_minutes, utc, utc_date,
    year,
};
use super::super::js::fancy_test;
use super::types::{
    EstimatedReadyPrecision, EvidenceConfidence, EvidenceSource, PlannedCustomerEventType,
    TimeSensitiveFact, VisualStyle,
};
use crate::{fre, re};

pub(crate) const MONTHS: [&str; 12] = [
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
];

/// `MONTH_INDEX[name]`.
pub(crate) fn month_index_of(name: &str) -> Option<i32> {
    MONTHS
        .iter()
        .position(|month| *month == name)
        .and_then(|index| i32::try_from(index).ok())
}

fn resolve_ready_year(now: Instant, month: i32, end_day: i32) -> i32 {
    let this_year = year(now);
    if utc_date(this_year, month, end_day) < now {
        this_year + 1
    } else {
        this_year
    }
}

/// The source must name where the evidence came from: a note that quotes
/// the customer stays a note.
fn source_from_ready_line(line: &str, from_customer_message: bool) -> EvidenceSource {
    if from_customer_message {
        return EvidenceSource::CustomerSms;
    }
    if re!(r"^Customer message:|^Lead details:", "i").is_match(line) {
        return EvidenceSource::CustomerMessage;
    }
    if re!(r"^Activity:", "i").is_match(line) {
        return EvidenceSource::CrmActivity;
    }
    EvidenceSource::SalespersonNote
}

/// When the customer expects to be ready.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EstimatedReady {
    pub at: Option<Instant>,
    pub source: EvidenceSource,
    pub confidence: EvidenceConfidence,
    pub precision: Option<EstimatedReadyPrecision>,
    pub window_start: Option<Instant>,
    pub window_end: Option<Instant>,
}

impl EstimatedReady {
    const EMPTY: Self = Self {
        at: None,
        source: EvidenceSource::None,
        confidence: EvidenceConfidence::Low,
        precision: None,
        window_start: None,
        window_end: None,
    };
}

/// "Ready in a year", "late March", "next spring"... from the customer's own
/// lines (`extra_text`) first, then the profile.
#[allow(clippy::too_many_lines)]
pub fn extract_estimated_ready_at(profile: &str, now: Instant, extra_text: &str) -> EstimatedReady {
    let customer_line_count = extra_text.split('\n').count();
    let joined = format!("{extra_text}\n{profile}");
    let lines: Vec<&str> = joined.split('\n').collect();
    for (index, line) in lines.iter().enumerate() {
        let line = *line;
        let from_customer_message = index < customer_line_count;
        if !re!(
            r"ready in|next spring|after christmas|in a year|in about a year|at least for a y[ea]*r|not ready .{0,48}y[ea]*r|approximat|beginning of (the |next )?year|start of (the |next )?year|late (january|february|march|april|may|june|july|august|september|october|november|december)|early (january|february|march|april|may|june|july|august|september|october|november|december)|middle of (january|february|march|april|may|june|july|august|september|october|november|december)|started in|be ready in march|ready in march",
            "i"
        )
        .is_match(line)
        {
            continue;
        }
        if fancy_test(
            fre!(
                r"\bnew build\b(?!er)|house (is )?being built|contractor delayed",
                "i"
            ),
            line,
        ) {
            continue;
        }
        if re!(r"within a couple\b", "i").is_match(line)
            && !re!(r"couple\s+(of\s+)?(days?|weeks?|months?)", "i").is_match(line)
        {
            continue;
        }
        let base = parse_profile_line_when_for_ready(line)
            .or_else(|| parse_nearby_created_date(&lines, line))
            .unwrap_or(now);
        let mut window_start = None;
        let mut window_end = None;
        let mut at = None;
        let month_match = re!(
            r"\b(late|early|middle of|mid)?\s*(january|february|march|april|may|june|july|august|september|october|november|december)\b",
            "i"
        )
        .captures(line);
        let month = month_match
            .as_ref()
            .and_then(|caps| caps.get(2))
            .and_then(|m| month_index_of(&m.as_str().to_lowercase()));
        if let Some(month) = month {
            let qualifier = month_match
                .as_ref()
                .and_then(|caps| caps.get(1))
                .map_or_else(String::new, |m| m.as_str().to_lowercase());
            let ready_year = resolve_ready_year(now, month, last_day_of_month(year(now), month));
            let (start, end, day) = match qualifier.as_str() {
                "late" => (20, last_day_of_month(ready_year, month), 25),
                "early" => (1, 10, 5),
                "middle of" | "mid" => (10, 20, 15),
                _ => (1, last_day_of_month(ready_year, month), 15),
            };
            window_start = Some(utc_date(ready_year, month, start));
            window_end = Some(utc_date(ready_year, month, end));
            at = Some(utc_date(ready_year, month, day));
        } else if re!(
            r"ready in (about |approximately |approx\.? )?(a |one )?year",
            "i"
        )
        .is_match(line)
            || re!(r"in a year approximat", "i").is_match(line)
            || re!(r"will be ready in a year", "i").is_match(line)
            || re!(
                r"at least for a y[ea]*r|not ready .{0,48}(a |one )?y[ea]*r",
                "i"
            )
            .is_match(line)
        {
            at = Some(add_months(base, 12));
            window_start = Some(add_months(base, 12));
            window_end = Some(add_months(base, 14));
        } else if re!(
            r"will be ready in (about |approximately |approx\.? )?(a|one)\s+month",
            "i"
        )
        .is_match(line)
            || re!(
                r"ready in (about |approximately |approx\.? )?(a|one)\s+month",
                "i"
            )
            .is_match(line)
        {
            at = Some(add_months(base, 1));
        } else if re!(r"ready in (about )?(six|6) months", "i").is_match(line) {
            at = Some(add_months(base, 6));
        } else if re!(r"ready in (about )?(three|3) months", "i").is_match(line) {
            at = Some(add_months(base, 3));
        } else if re!(r"next spring", "i").is_match(line) {
            let spring_year = year(base) + i32::from(month_index(base) >= 5);
            window_start = Some(utc_date(spring_year, 2, 1));
            window_end = Some(utc_date(spring_year, 4, 31));
            at = Some(utc_date(spring_year, 3, 15));
        } else if re!(r"after christmas", "i").is_match(line) {
            at = Some(utc_date(
                year(base) + i32::from(month_index(base) == 11),
                0,
                5,
            ));
        } else if re!(
            r"beginning of (the |next )?year|start of (the |next )?year",
            "i"
        )
        .is_match(line)
        {
            let next_year = re!(r"next year", "i").is_match(line) || month_index(base) >= 1;
            let start_year = year(base) + i32::from(next_year);
            window_start = Some(utc_date(start_year, 0, 1));
            window_end = Some(utc_date(start_year, 0, 31));
            at = Some(utc_date(start_year, 0, 1));
        }
        if at.is_none() {
            continue;
        }
        let customer_said = re!(
            r"we plan|i plan|she said|he said|they said|customer (said|says)|started in late|ready in|\bsaid (?:will be )?ready|\bsaid (?:they |she |he )?(?:will be )?ready",
            "i"
        )
        .is_match(line);
        return EstimatedReady {
            at,
            source: source_from_ready_line(line, from_customer_message),
            confidence: if customer_said || !re!(r"approximat|about", "i").is_match(line) {
                EvidenceConfidence::High
            } else {
                EvidenceConfidence::Medium
            },
            precision: Some(
                if re!(
                    r"at least for a y[ea]*r|not ready .{0,48}(a |one )?y[ea]*r",
                    "i"
                )
                .is_match(line)
                {
                    EstimatedReadyPrecision::Minimum
                } else {
                    EstimatedReadyPrecision::Approximate
                },
            ),
            window_start,
            window_end,
        };
    }
    EstimatedReady::EMPTY
}

/// The profile line's own time (`(2026-03-01 10:00` or `created 2026-03-01 10:00`).
pub(crate) fn parse_profile_line_when_for_ready(line: &str) -> Option<Instant> {
    let parse = |raw: &str| {
        let iso = if raw.contains('T') {
            raw.to_string()
        } else {
            raw.replacen(' ', "T", 1)
        };
        parse_local_minutes(&iso)
    };
    if let Some(at) = re!(r"\((\d{4}-\d{2}-\d{2}[ T]\d{2}:\d{2})")
        .captures(line)
        .and_then(|caps| caps.get(1))
        .and_then(|m| parse(m.as_str()))
    {
        return Some(at);
    }
    re!(r"created (\d{4}-\d{2}-\d{2}[ T]\d{2}:\d{2})")
        .captures(line)
        .and_then(|caps| caps.get(1))
        .and_then(|m| parse(m.as_str()))
}

fn parse_nearby_created_date(lines: &[&str], current: &str) -> Option<Instant> {
    // `lines.indexOf(current)`: the first equal line.
    let index = lines.iter().position(|line| *line == current)?;
    let window = [Some(index), index.checked_sub(1), Some(index + 1)];
    window
        .into_iter()
        .flatten()
        .filter_map(|i| lines.get(i))
        .filter(|line| !line.is_empty())
        .find_map(|line| parse_profile_line_when_for_ready(line))
}

/// A quartz price change on a known date ("price of quartz will change starting September 15").
pub fn extract_time_sensitive_facts(profile: &str, now: Instant) -> Vec<TimeSensitiveFact> {
    let mut facts = Vec::new();
    if let Some(caps) = re!(
        r"price of quartz will change starting (september|sep)\s+(\d{1,2})",
        "i"
    )
    .captures(profile)
    {
        let day: i32 = caps
            .get(2)
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(1);
        facts.push(TimeSensitiveFact {
            kind: "pricing_change",
            valid_from: None,
            valid_until: Some(utc(year(now), 8, day, 23, 59, 59)),
            evidence: caps[0].to_string(),
        });
    }
    facts
}

pub fn time_sensitive_fact_is_active(fact: &TimeSensitiveFact, now: Instant) -> bool {
    if fact.valid_from.is_some_and(|from| now < from) {
        return false;
    }
    fact.valid_until.is_none_or(|until| now <= until)
}

/// A day range the customer gave ("3-5 days", "10 days would be best").
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct DayRange {
    pub start: f64,
    pub end: f64,
}

pub fn extract_relative_day_range(text: &str) -> Option<DayRange> {
    let number = |m: Option<regex::Match<'_>>| m.and_then(|m| m.as_str().parse::<f64>().ok());
    if let Some(caps) = re!(r"(\d+)\s*[-–]\s*(\d+)\s+days", "i").captures(text)
        && let (Some(start), Some(end)) = (number(caps.get(1)), number(caps.get(2)))
    {
        return Some(DayRange { start, end });
    }
    let best = re!(r"\b(\d+)\s+days would be best\b", "i").captures(text)?;
    let amount = number(best.get(1))?;
    Some(DayRange {
        start: amount,
        end: amount,
    })
}

/// "Late next week or early the following week", "in a couple weeks": day
/// offsets from the day the customer wrote it. Weekday-agnostic on purpose.
pub fn extract_relative_week_range(text: &str) -> Option<DayRange> {
    let mut ranges: Vec<(f64, f64)> = Vec::new();
    if re!(r"\b(late|end of) next week\b", "i").is_match(text) {
        ranges.push((8.0, 11.0));
    } else if re!(r"\b(early|beginning of) next week\b", "i").is_match(text) {
        ranges.push((4.0, 8.0));
    } else if re!(r"\bnext week\b", "i").is_match(text) {
        ranges.push((5.0, 11.0));
    }
    if re!(r"\b(following week|week after next)\b", "i").is_match(text) {
        ranges.push((11.0, 16.0));
    }
    if re!(r"\bin a week\b", "i").is_match(text) {
        ranges.push((7.0, 7.0));
    }
    if re!(r"\bin (two|2|a couple(?: of)?) weeks\b", "i").is_match(text) {
        ranges.push((14.0, 14.0));
    }
    if ranges.is_empty() {
        return None;
    }
    Some(DayRange {
        start: ranges.iter().map(|r| r.0).fold(f64::INFINITY, f64::min),
        end: ranges.iter().map(|r| r.1).fold(f64::NEG_INFINITY, f64::max),
    })
}

/// `Date.UTC(y, m, d + days)` from an instant's UTC date.
pub(crate) fn add_utc_days(base: Instant, days: f64) -> Instant {
    #[allow(clippy::cast_possible_truncation)]
    let days = days.clamp(-1e8, 1e8) as i32;
    utc_date(
        year(base),
        month_index(base),
        day(base).saturating_add(days),
    )
}

/// When the customer asked to hear back ("text me in 3-5 days").
pub fn extract_customer_requested_contact_window(
    profile: &str,
    customer_corpus: &str,
    now: Instant,
) -> (Option<Instant>, Option<Instant>) {
    let joined = format!("{customer_corpus}\n{profile}");
    for line in joined.split('\n') {
        if !re!(
            r"would be best|text (me|you)|check back|follow up|days",
            "i"
        )
        .is_match(line)
            || re!(r"ready in|project (will be )?ready", "i").is_match(line)
        {
            continue;
        }
        let Some(range) = extract_relative_day_range(line) else {
            continue;
        };
        let at = parse_profile_line_when_for_ready(line).unwrap_or(now);
        return (
            Some(add_utc_days(at, range.start)),
            Some(add_utc_days(at, range.end)),
        );
    }
    (None, None)
}

pub fn extract_visual_style(text: &str) -> Option<VisualStyle> {
    re!(r"simple|not busy|low movement|isn'?t busy", "i")
        .is_match(text)
        .then_some(VisualStyle::SimpleLowMovement)
}

fn calendar_date_label(month_name: &str, day: i32) -> String {
    let lower = month_name.to_lowercase();
    let mut chars = lower.chars();
    let first = chars
        .next()
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_default();
    format!("{first}{} {day}", chars.as_str())
}

fn extract_showroom_date_from_profile(profile: &str, now: Instant) -> Option<(Instant, String)> {
    let around = re!(
        r"(?:showroom|come (?:in|by)|visit (?:the )?(?:showroom|store)|stop by)[\s\S]{0,320}",
        "i"
    )
    .find(profile)
    .map_or("", |m| m.as_str());
    let caps = re!(
        r"\b(january|february|march|april|may|june|july|august|september|october|november|december)\s+(\d{1,2})(?:st|nd|rd|th)?\b",
        "i"
    )
    .captures(around)?;
    let month_name = caps.get(1)?.as_str();
    let day: i32 = caps.get(2)?.as_str().parse().ok()?;
    let month = month_index_of(&month_name.to_lowercase())?;
    if !(1..=31).contains(&day) {
        return None;
    }
    let ready_year = resolve_ready_year(now, month, day);
    Some((
        utc_date(ready_year, month, day),
        calendar_date_label(month_name, day),
    ))
}

/// Whether a draft names the planned date ("Sept 5", "5th of September").
pub fn body_mentions_planned_event_date(body: &str, date_label: &str) -> bool {
    let Some(caps) = re!(r"^([A-Za-z]+)\s+(\d{1,2})$").captures(date_label) else {
        return body.to_lowercase().contains(&date_label.to_lowercase());
    };
    let month_name = &caps[1];
    let day = &caps[2];
    let abbrev = &month_name[..month_name.len().min(3)];
    crate::followups::js::js_regex(
        &format!(
            r"\b(?:{month_name}|{abbrev}\.?)\s+{day}(?:st|nd|rd|th)?\b|\b{day}(?:st|nd|rd|th)?\s+(?:of\s+)?{month_name}\b"
        ),
        "i",
    )
    .is_match(body)
}

/// Rep voice must not claim the showroom visit when CRM says the customer may visit.
pub fn body_attributes_planned_showroom_visit_to_rep(body: &str) -> bool {
    if !re!(r"\bshowroom\b", "i").is_match(body) {
        return false;
    }
    re!(
        r"\bI(?:'m| am) planning to (?:be )?(?:at )?(?:the )?showroom\b",
        "i"
    )
    .is_match(body)
        || re!(r"\bI(?:'ll| will) be at (?:the )?showroom\b", "i").is_match(body)
        || (re!(r"\bI(?:'m| am) planning to be\b", "i").is_match(body)
            && re!(r"\bshowroom\b", "i").is_match(body))
}

/// Something the customer plans that the follow-up should wait for.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlannedCustomerEvent {
    #[serde(rename = "type")]
    pub kind: Option<PlannedCustomerEventType>,
    pub window_start: Option<Instant>,
    pub window_end: Option<Instant>,
    pub confidence: Option<EvidenceConfidence>,
    pub date_label: Option<String>,
}

pub fn extract_planned_customer_event(profile: &str, now: Instant) -> PlannedCustomerEvent {
    let empty = PlannedCustomerEvent {
        kind: None,
        window_start: None,
        window_end: None,
        confidence: None,
        date_label: None,
    };
    if re!(
        r"waiting for (the )?floors|flooring (is )?on order|floors to arrive|until (the )?floors are in place",
        "i"
    )
    .is_match(profile)
    {
        return PlannedCustomerEvent {
            kind: Some(PlannedCustomerEventType::FloorInstallation),
            confidence: Some(EvidenceConfidence::Medium),
            ..empty
        };
    }
    if !re!(r"showroom|come (in|by)|visit (the )?(showroom|store)|stop by", "i").is_match(profile)
        || !re!(
            r"showroom|visit|come (in|by)|after (the )?business trip|beginning of september|september\s*5",
            "i"
        )
        .is_match(profile)
    {
        return empty;
    }
    let this_year = year(now);
    let mentioned = extract_showroom_date_from_profile(profile, now);
    let mut date_label = mentioned.as_ref().map(|(_, label)| label.clone());
    let (window_start, window_end) = if let Some((date, _)) = &mentioned {
        let (y, m, d) = (year(*date), month_index(*date), day(*date));
        (
            Some(utc_date(y, m, (d - 1).max(1))),
            Some(utc_date(y, m, d + 1)),
        )
    } else if re!(r"september\s*5|sep(?:t)?\.?\s*5\b", "i").is_match(profile) {
        date_label = Some("September 5".into());
        (
            Some(utc_date(this_year, 8, 4)),
            Some(utc_date(this_year, 8, 6)),
        )
    } else if re!(r"beginning of september|early september", "i").is_match(profile) {
        (
            Some(utc_date(this_year, 8, 1)),
            Some(utc_date(this_year, 8, 10)),
        )
    } else {
        (None, None)
    };
    PlannedCustomerEvent {
        kind: Some(PlannedCustomerEventType::ShowroomVisit),
        window_start,
        window_end,
        confidence: Some(if window_start.is_some() {
            EvidenceConfidence::Medium
        } else {
            EvidenceConfidence::Low
        }),
        date_label,
    }
}
