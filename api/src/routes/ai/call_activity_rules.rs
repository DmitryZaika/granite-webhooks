//! The rules that turn the AI's call follow-up tasks into CRM activities.
//!
//! Names are cleaned up and merged; deadlines are read from the transcript
//! (call backs in N weeks, "next Friday", on-site estimate reminders, quotes
//! due before close, links sent "a little later").
//!
//! A line-by-line port of the helpers of the Remix
//! `api.aiSummarize.callActivity` route; `tests/call_activity_rules.rs`
//! compares it with the TypeScript on a corpus of cases.

use crate::openapi::ToSchema;
use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, TimeZone, Timelike, Utc};
use chrono_tz::America::Indiana::Indianapolis;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

macro_rules! re {
    ($pattern:expr) => {{
        static RE: std::sync::LazyLock<Regex> =
            std::sync::LazyLock::new(|| Regex::new($pattern).expect("valid pattern"));
        &*RE
    }};
}

/// A CRM activity from a call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CallActivity {
    /// What to do.
    pub name: String,
    /// `YYYY-MM-DD`, an ISO 8601 UTC time, or a store wall time
    /// `YYYY-MM-DD HH:MM:SS` (on-site estimate reminders); null for none.
    pub deadline: Option<String>,
}

pub const DEFAULT_ACTIVITY_NAME: &str = "Follow-up";
pub const VOICEMAIL_DEFAULT_ACTIVITY_NAME: &str = "Follow-up to confirm interest";
const STORE_CLOSE_HOUR: u32 = 18;
const DEADLINE_HOURS_BEFORE_STORE_CLOSE: u32 = 2;
const QUOTE_MINIMUM_LEAD_MINUTES: i64 = 60;
const ONSITE_MORNING_HOUR: u32 = 9;
const ONSITE_EVENING_HOUR: u32 = 17;
const ONSITE_NOON_HOUR: u32 = 12;
const CONFIRMED_ONSITE_ESTIMATE_NAME: &str = "Be on-site for estimate";
const REP_TWO_THREE_WEEK_CALLBACK_DAYS: i64 = 14;
const STONE_COLOR_PHOTO_ACTIVITY_NAME: &str = "Request picture of preferred stone";
const KITCHEN_PHOTO_ACTIVITY_NAME: &str = "Request kitchen photos";
const MERIDIEM: &str = r"(?:a\.?\s*m\.?|p\.?\s*m\.?)";

fn immediate() -> &'static Regex {
    re!(
        r"(?i)\b(right away|right now|immediately|asap|send (?:it |you |them |the )?(?:now|shortly)|(?:now|shortly|asap).{0,30}send|a little (?:bit )?later)\b"
    )
}
fn tomorrow() -> &'static Regex {
    re!(r"(?i)\btomorrow\b")
}
fn today() -> &'static Regex {
    re!(r"(?i)\btoday\b")
}
fn explicit_time() -> &'static Regex {
    re!(r"(?i)\b\d{1,2}(?::\d{2})?\s*(?:am|pm)\b")
}
fn callback_name() -> &'static Regex {
    re!(r"(?i)\b(?:call\s+back|follow[- ]?up|callback)\b")
}

// --- dates ------------------------------------------------------------

/// `Date.prototype.toISOString`.
pub fn iso(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// `new Date(text)` for the shapes deadlines take; the server's local time
/// is UTC.
pub fn parse_js_date(text: &str) -> Option<DateTime<Utc>> {
    let text = text.trim();
    if let Ok(date) = NaiveDate::parse_from_str(text, "%Y-%m-%d") {
        return date.and_hms_opt(0, 0, 0).map(|at| at.and_utc());
    }
    if let Ok(at) = DateTime::parse_from_rfc3339(text) {
        return Some(at.with_timezone(&Utc));
    }
    let bare = text.strip_suffix(['Z', 'z']).unwrap_or(text);
    for format in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(at) = NaiveDateTime::parse_from_str(bare, format) {
            return Some(at.and_utc());
        }
    }
    None
}

fn store_date(at: DateTime<Utc>) -> NaiveDate {
    at.with_timezone(&Indianapolis).date_naive()
}

fn store_date_plus(at: DateTime<Utc>, days: i64) -> NaiveDate {
    store_date(at) + Duration::days(days)
}

fn date_text(date: NaiveDate) -> String {
    date.format("%Y-%m-%d").to_string()
}

fn store_date_only_plus(at: DateTime<Utc>, days: i64) -> String {
    date_text(store_date_plus(at, days))
}

fn wall_to_utc(date: NaiveDate, hour: u32, minute: u32) -> DateTime<Utc> {
    let Some(local) = date.and_hms_opt(hour, minute, 0) else {
        return date.and_hms_opt(0, 0, 0).unwrap_or_default().and_utc();
    };
    match Indianapolis.from_local_datetime(&local) {
        chrono::LocalResult::Single(at) | chrono::LocalResult::Ambiguous(at, _) => {
            at.with_timezone(&Utc)
        }
        chrono::LocalResult::None => Indianapolis
            .from_local_datetime(&(local + Duration::hours(1)))
            .earliest()
            .map_or_else(|| local.and_utc(), |at| at.with_timezone(&Utc)),
    }
}

fn wall_mysql(date: NaiveDate, hour: u32, minute: u32) -> String {
    format!("{} {hour:02}:{minute:02}:00", date_text(date))
}

fn store_close_deadline(committed: DateTime<Utc>, days: i64) -> String {
    iso(wall_to_utc(
        store_date_plus(committed, days),
        STORE_CLOSE_HOUR - DEADLINE_HOURS_BEFORE_STORE_CLOSE,
        0,
    ))
}

fn weekday_index(name: &str) -> Option<i64> {
    Some(match name {
        "sunday" => 0,
        "monday" => 1,
        "tuesday" => 2,
        "wednesday" => 3,
        "thursday" => 4,
        "friday" => 5,
        "saturday" => 6,
        _ => return None,
    })
}

fn days_until_weekday(committed: DateTime<Utc>, weekday: &str, next_week: bool) -> i64 {
    let Some(target) = weekday_index(&weekday.to_lowercase()) else {
        return 0;
    };
    let current = i64::from(
        committed
            .with_timezone(&Indianapolis)
            .weekday()
            .num_days_from_sunday(),
    );
    let mut diff = target - current;
    if diff <= 0 {
        diff += 7;
    }
    if next_week {
        diff += 7;
    }
    diff
}

/// `text.split(/(?<=[.?!])\s+/)`, trimmed, empty pieces dropped.
fn sentences(text: &str) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut current = String::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        current.push(ch);
        if matches!(ch, '.' | '?' | '!') && chars.peek().is_some_and(|next| next.is_whitespace()) {
            while chars.peek().is_some_and(|next| next.is_whitespace()) {
                chars.next();
            }
            pieces.push(std::mem::take(&mut current));
        }
    }
    pieces.push(current);
    pieces
        .into_iter()
        .map(|piece| piece.trim().to_string())
        .filter(|piece| !piece.is_empty())
        .collect()
}

// --- names ----------------------------------------------------------------

fn strip_calendar_dates(name: &str) -> String {
    let name = re!(r"(?i)\b(?:on\s+)?(?:jan|feb|mar|apr|may|jun|jul|aug|sep|oct|nov|dec)[a-z]*\.?\s+\d{1,2}(?:,?\s*\d{4})?\b")
        .replace_all(name, "");
    let name = re!(r"\b\d{4}-\d{2}-\d{2}\b").replace_all(&name, "");
    let name = re!(r"\b\d{1,2}/\d{1,2}(?:/\d{2,4})?\b").replace_all(&name, "");
    let name = re!(r"\s{2,}").replace_all(&name, " ");
    let name = re!(r"^,\s*|\s*,\s*$").replace_all(name.trim(), "");
    name.trim().to_string()
}

fn is_call_back(name: &str) -> bool {
    callback_name().is_match(&name.to_lowercase())
}

fn capitalize(word: &str) -> String {
    let mut chars = word.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
    })
}

fn clean_place_name(raw: &str) -> String {
    let trimmed =
        re!(r"(?i)\s+(before|when|after|and|or|so|we|they|the|from)\s*$").replace(raw.trim(), "");
    trimmed
        .split_whitespace()
        .take(3)
        .map(capitalize)
        .collect::<Vec<_>>()
        .join(" ")
}

fn call_back_detail(transcript: &str) -> Option<String> {
    let lower = transcript.to_lowercase();
    if re!(r"\blitigation\b").is_match(&lower) || re!(r"\bmove forward\b").is_match(&lower) {
        return None;
    }
    let back_from = re!(r"\b(?:get|got|come|coming)\s+back\s+from\s+([a-z][a-z\s'-]{1,40}?)(?:\s+before|\s+when|\s+to|\s+and|\.|,|\?|!|$)")
        .captures(&lower)
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str().to_string());
    let vacation = re!(r"\bvacation(?:\s+on\s+\w+)?\s+to\s+([a-z][a-z\s'-]{1,30}?)(?:\s+|\.|,|$)")
        .captures(&lower)
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str().to_string());
    let trip = re!(r"\btrip\s+to\s+([a-z][a-z\s'-]{1,30}?)(?:\s+|\.|,|$)")
        .captures(&lower)
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str().to_string());
    let destination = clean_place_name(
        vacation
            .as_deref()
            .or(trip.as_deref())
            .or(back_from.as_deref())
            .unwrap_or(""),
    );
    if re!(r"\bvacation\b").is_match(&lower) {
        return Some(if destination.chars().count() > 1 {
            format!("after vacation to {destination}")
        } else {
            "after vacation".into()
        });
    }
    if let Some(from) = &back_from
        && re!(r"\b(?:trip|travel|decision|check back|make a final)\b").is_match(&lower)
    {
        let place = clean_place_name(from);
        if place.chars().count() > 1 {
            return Some(format!("after trip to {place}"));
        }
    }
    if re!(r"\b(?:holiday|holidays)\b").is_match(&lower)
        && re!(r"\b(?:get back|when|after|before|decision)\b").is_match(&lower)
    {
        return Some("after holiday".into());
    }
    if re!(r"\b(?:make a final decision|final decision|not ready yet)\b").is_match(&lower)
        && re!(r"\b(?:vacation|trip|travel|get back|few weeks)\b").is_match(&lower)
    {
        return Some("after they decide".into());
    }
    None
}

fn call_back_base_name(transcript: &str) -> &'static str {
    let lower = transcript.to_lowercase();
    if re!(r"\bquote\b").is_match(&lower)
        && re!(r"\b(?:questions?|check|follow\s+up|saw the|sent you|home depot|lowe)")
            .is_match(&lower)
    {
        return "Call back, follow up on quote";
    }
    if re!(r"\blitigation\b").is_match(&lower) {
        return "Call back, check if litigation is finished";
    }
    if re!(r"\bmove forward\b").is_match(&lower) {
        return "Call back, check if ready to move forward";
    }
    if re!(r"\bconfirm\b").is_match(&lower) && re!(r"\binterest\b").is_match(&lower) {
        return "Call back, confirm still interested";
    }
    if re!(r"\bkitchen\b").is_match(&lower) {
        return "Call back, follow up on kitchen project";
    }
    if re!(r"\b(?:countertop|counter top|counter)\b").is_match(&lower) {
        return "Call back, follow up on countertop project";
    }
    "Call back, follow up"
}

fn call_back_name(transcript: &str) -> String {
    let base = call_back_base_name(transcript);
    let Some(detail) = call_back_detail(transcript) else {
        return base.to_string();
    };
    let core = detail.strip_prefix("after ").unwrap_or(&detail);
    if base.to_lowercase().contains(core) {
        return base.to_string();
    }
    format!("{base} {detail}")
}

// --- call backs -----------------------------------------------------------

fn call_back_context() -> &'static Regex {
    re!(
        r"\b(?:would you like (?:me|us) to )?(?:call|follow\s+(?:you\s+)?up)\b|\bfollow\s+(?:you\s+)?up\b|\bcall\s+(?:you|me|back)\b|\bcall\s+me\s+back\b|\bfollow\s+up\b|\bwould you like\b"
    )
}

fn parse_week_count(raw: &str) -> Option<i64> {
    let lower = raw.trim().to_lowercase();
    if lower == "a" || lower == "an" {
        return Some(1);
    }
    let digits: String = lower.chars().take_while(char::is_ascii_digit).collect();
    if let Ok(number) = digits.parse::<i64>()
        && (1..=52).contains(&number)
    {
        return Some(number);
    }
    Some(match lower.as_str() {
        "one" => 1,
        "two" => 2,
        "three" => 3,
        "four" => 4,
        "five" => 5,
        "six" => 6,
        "seven" => 7,
        "eight" => 8,
        "nine" => 9,
        "ten" => 10,
        _ => return None,
    })
}

fn agreed_call_back_weeks(transcript: &str) -> Option<i64> {
    let lower = transcript.to_lowercase();
    if !call_back_context().is_match(&lower) {
        return None;
    }
    let weeks = re!(
        r"(?i)\b(?:about\s+)?(?:in\s+)?(a|an|one|two|three|four|five|six|seven|eight|nine|ten|\d{1,2})\s*weeks?\b"
    );
    let mut context = false;
    let mut last = None;
    for sentence in sentences(&lower) {
        if call_back_context().is_match(&sentence) {
            context = true;
        }
        let ambiguous =
            re!(r"\b(?:two|2)\s*(?:or|to|-)\s*(?:three|3)\s*weeks?\b").is_match(&sentence);
        let confirmed = re!(r"\b(?:yeah|yes|yep|sure|ok|good|will do)\b").is_match(&sentence);
        if ambiguous && !confirmed {
            last = None;
            continue;
        }
        for caps in weeks.captures_iter(&sentence) {
            if (context || re!(r"\bcall\b").is_match(&sentence))
                && let Some(parsed) = caps.get(1).and_then(|m| parse_week_count(m.as_str()))
            {
                last = Some(parsed);
            }
        }
    }
    last
}

fn agreed_call_back_deadline(transcript: &str, committed: DateTime<Utc>) -> Option<String> {
    let lower = transcript.to_lowercase();
    if !call_back_context().is_match(&lower) {
        return None;
    }
    if let Some(weeks) = agreed_call_back_weeks(transcript) {
        return Some(store_date_only_plus(committed, weeks * 7));
    }
    let weekday_after = |pattern: &Regex, next: bool| {
        pattern
            .captures(&lower)
            .and_then(|caps| caps.get(1))
            .map(|m| {
                store_date_only_plus(committed, days_until_weekday(committed, m.as_str(), next))
            })
    };
    if let Some(date) = weekday_after(
        re!(
            r"\b(?:you\s+can\s+call\s+me|call\s+me)\s+next\s+(monday|tuesday|wednesday|thursday|friday|saturday|sunday)\b"
        ),
        true,
    ) {
        return Some(date);
    }
    if let Some(date) = weekday_after(
        re!(
            r"\b(?:would you like\s+(?:me\s+)?to\s+)?call\s+you\s+next\s+(monday|tuesday|wednesday|thursday|friday|saturday|sunday)\b"
        ),
        true,
    ) {
        return Some(date);
    }
    let next_weekday =
        re!(r"\bnext\s+(monday|tuesday|wednesday|thursday|friday|saturday|sunday)\b");
    if next_weekday.is_match(&lower)
        && re!(r"\b(?:call|check|quote|questions?|will do|okay|good)\b").is_match(&lower)
        && let Some(date) = weekday_after(next_weekday, true)
    {
        return Some(date);
    }
    let plain = re!(
        r"\b(?:call\s+(?:you|me)\s+)?(?:on\s+)?(monday|tuesday|wednesday|thursday|friday|saturday|sunday)\b"
    );
    if !next_weekday.is_match(&lower) {
        return weekday_after(plain, false);
    }
    None
}

fn resolve_meridiem(meridiem: Option<&str>, matched: &str) -> Option<&'static str> {
    let compact: String = meridiem
        .unwrap_or("")
        .chars()
        .filter(|ch| *ch != '.' && !ch.is_whitespace())
        .collect::<String>()
        .to_lowercase();
    if compact == "pm" {
        return Some("pm");
    }
    if compact == "am" {
        return Some("am");
    }
    if re!(r"(?i)\bp\.?\s*m\.?\b").is_match(matched) {
        return Some("pm");
    }
    if re!(r"(?i)\ba\.?\s*m\.?\b").is_match(matched) {
        return Some("am");
    }
    None
}

fn to_24_hour(hour12: u32, meridiem: Option<&str>, matched: &str) -> u32 {
    let resolved = resolve_meridiem(meridiem, matched);
    let mut hour = hour12;
    if resolved == Some("pm") && hour < 12 {
        hour += 12;
    }
    if resolved == Some("am") && hour == 12 {
        hour = 0;
    }
    if resolved.is_none() && (1..=6).contains(&hour) {
        hour += 12;
    }
    hour
}

fn is_timed(deadline: &str) -> bool {
    let trimmed = deadline.trim();
    if re!(r"^\d{4}-\d{2}-\d{2}$").is_match(trimmed) {
        return false;
    }
    parse_js_date(trimmed).is_some()
}

fn timed_call_back_deadline(transcript: &str, committed: DateTime<Utc>) -> Option<String> {
    let lower = transcript.to_lowercase();
    if !call_back_context().is_match(&lower) {
        return None;
    }
    let chunks: Vec<String> = sentences(transcript)
        .into_iter()
        .filter(|chunk| {
            re!(
                r"(?i)\b(?:call\s+(?:me\s+)?back|call\s+(?:you|me)|we(?:'ll| will)\s+(?:do|call))\b"
            )
            .is_match(chunk)
        })
        .collect();
    let texts = if chunks.is_empty() {
        vec![transcript.to_string()]
    } else {
        chunks
    };
    for text in &texts {
        if let Some(parsed) = explicit_deadline(text, committed).filter(|parsed| is_timed(parsed)) {
            return Some(parsed);
        }
    }
    explicit_deadline(transcript, committed).filter(|parsed| is_timed(parsed))
}

fn apply_agreed_call_back(
    activity: CallActivity,
    committed: DateTime<Utc>,
    transcript: &str,
) -> CallActivity {
    if !is_call_back(&activity.name) {
        return activity;
    }
    if let Some(timed) = timed_call_back_deadline(transcript, committed) {
        return CallActivity {
            name: call_back_name(transcript),
            deadline: Some(timed),
        };
    }
    CallActivity {
        name: call_back_name(transcript),
        deadline: agreed_call_back_deadline(transcript, committed).or(activity.deadline),
    }
}

fn rep_two_three_week_task(name: &str, transcript: &str) -> bool {
    let lower = transcript.to_lowercase();
    let pattern = re!(
        r"(?i)\b(?:would you like (?:me|us) to )?call(?: you| back)?\b[^.]{0,120}\b(?:in\s+)?(?:about\s+)?(?:(?:two|2)\s*(?:or|to|-)\s*(?:three|3)|(?:one|two|three|four|five|six|\d{1,2}))\s*weeks?\b|\b(?:i(?:'ll| will)|we will)\s+call(?: you| back)?\b[^.]{0,80}\b(?:in\s+)?(?:(?:two|2)\s*(?:or|to|-)\s*(?:three|3)|(?:one|two|three|four|five|six|\d{1,2}))\s*weeks?\b"
    );
    if !pattern.is_match(&lower) {
        return false;
    }
    let name = name.to_lowercase();
    callback_name().is_match(&name)
        || re!(r"(?i)\b(?:(?:two|2)\s*(?:or|to|-)\s*(?:three|3)|(?:one|two|three|four|five|six|\d{1,2}))\s*weeks?\b").is_match(&name)
}

fn rep_two_three_week_deadline(
    name: &str,
    transcript: &str,
    committed: DateTime<Utc>,
) -> Option<String> {
    if !rep_two_three_week_task(name, transcript) {
        return None;
    }
    let days = agreed_call_back_weeks(transcript)
        .map_or(REP_TWO_THREE_WEEK_CALLBACK_DAYS, |weeks| weeks * 7);
    Some(store_date_only_plus(committed, days))
}

fn apply_rep_two_three_week(
    activity: CallActivity,
    committed: DateTime<Utc>,
    transcript: &str,
) -> CallActivity {
    rep_two_three_week_deadline(&activity.name, transcript, committed).map_or(
        activity,
        |deadline| CallActivity {
            name: call_back_name(transcript),
            deadline: Some(deadline),
        },
    )
}

// --- on-site estimates ------------------------------------------------------

fn is_onsite_estimate(name: &str) -> bool {
    let lower = name.to_lowercase();
    if re!(r"\bsend\b").is_match(&lower) && re!(r"\b(?:quote|estimate|pricing)\b").is_match(&lower)
    {
        return false;
    }
    if re!(r"\bschedule\b").is_match(&lower)
        && re!(r"\b(?:estimate|measurement|on[- ]?site|visit)\b").is_match(&lower)
    {
        return true;
    }
    if re!(r"\b(?:on[- ]?site|be on[- ]?site)\b").is_match(&lower) {
        return true;
    }
    re!(r"\b(?:estimate|measurement)\b").is_match(&lower)
        && re!(r"\b(?:visit|measure|appointment)\b").is_match(&lower)
}

#[derive(Debug, Clone, Copy)]
struct Appointment {
    at: DateTime<Utc>,
    explicit_time: bool,
}

fn transcript_confirms_visit(transcript: &str) -> bool {
    let lower = transcript.to_lowercase();
    let visit = re!(r"\b(?:estimate|measurement|measure|showroom|appointment|visit)\b")
        .is_match(&lower)
        || re!(r"\b(?:stop by|come over|be here|see you)\b").is_match(&lower);
    if !visit {
        return false;
    }
    re!(r"\b(?:saturday|sunday|monday|tuesday|wednesday|thursday|friday)\b").is_match(&lower)
        || re!(r"(?i)\b(?:see you|i(?:'ll| will) be there|be here|stop by)\b[^.]{0,60}\b(?:at\s+)?\d{1,2}").is_match(transcript)
        || re!(r"(?i)\bif you can be here at\s+\d").is_match(&lower)
        || (re!(r"\b(?:at\s+)?\d{1,2}(?::\d{2})?\s*(?:pm|am)\b").is_match(&lower)
            && re!(r"\b(?:awesome|works|good|yes|today)\b").is_match(&lower))
        || re!(r"(?i)\b\d{1,2}\s*o['']?clock\b").is_match(&lower)
        || re!(r"\baround\s+\d{1,2}\b").is_match(&lower)
        || re!(r"\b\d{1,2}\s+is\s+good\b").is_match(&lower)
}

fn scheduled_weekday_offset(text: &str, committed: DateTime<Utc>) -> Option<i64> {
    let lower = text.to_lowercase();
    let first = |pattern: &Regex| {
        pattern
            .captures(&lower)
            .and_then(|caps| caps.get(1))
            .map(|m| m.as_str().to_string())
    };
    if let Some(day) = first(re!(
        r"\b(monday|tuesday|wednesday|thursday|friday|saturday|sunday)\b[^.]{0,40}\b(?:of\s+)?next\s+week\b"
    )) {
        return Some(days_until_weekday(committed, &day, true));
    }
    if let Some(day) = first(re!(
        r"\bnext\s+week\b[^.]{0,40}\b(?:on\s+)?(monday|tuesday|wednesday|thursday|friday|saturday|sunday)\b"
    )) {
        return Some(days_until_weekday(committed, &day, true));
    }
    if let Some(day) = first(re!(
        r"\bnext\s+(monday|tuesday|wednesday|thursday|friday|saturday|sunday)\b"
    )) {
        return Some(days_until_weekday(committed, &day, true));
    }
    first(re!(
        r"\b(?:on\s+(?:a\s+)?)?(monday|tuesday|wednesday|thursday|friday|saturday|sunday)\b"
    ))
    .map(|day| days_until_weekday(committed, &day, false))
}

const fn hour_without_meridiem(hour12: u32) -> u32 {
    if hour12 >= 1 && hour12 <= 7 {
        hour12 + 12
    } else {
        hour12
    }
}

fn chunk_confirms_appointment(chunk: &str) -> bool {
    let lower = chunk.to_lowercase();
    if re!(r"\b(?:let'?s do|see you|we(?:'ll| will) do|booked|confirmed|okay, great|sounds good)\b")
        .is_match(&lower)
    {
        return true;
    }
    re!(r"\b(?:yeah|yes|okay|good|great)\b").is_match(&lower)
        && (re!(r"\b\d{1,2}:\d{2}\b").is_match(&lower)
            || explicit_time().is_match(&lower)
            || re!(r"\b(?:monday|tuesday|wednesday|thursday|friday|saturday|sunday)\b")
                .is_match(&lower))
}

fn starts_with_meridiem_letter(text: &str) -> bool {
    text.chars()
        .next()
        .is_some_and(|ch| matches!(ch, 'a' | 'p' | 'A' | 'P'))
}

fn appointment_from_text(
    text: &str,
    committed: DateTime<Utc>,
    transcript: Option<&str>,
) -> Option<Appointment> {
    let lower = text.to_lowercase();
    let mut weekday_offset = scheduled_weekday_offset(text, committed);
    if weekday_offset.is_some()
        && let Some(transcript) = transcript
        && let Some(weekday) =
            re!(r"\b(monday|tuesday|wednesday|thursday|friday|saturday|sunday)\b")
                .captures(&lower)
                .and_then(|caps| caps.get(1))
                .map(|m| m.as_str().to_string())
        && !re!(r"\bnext\s+week\b").is_match(&lower)
    {
        let next_week = Regex::new(&format!(
            r"\b{weekday}\b[^.]{{0,40}}\b(?:of\s+)?next\s+week\b"
        ))
        .is_ok_and(|pattern| pattern.is_match(&transcript.to_lowercase()));
        if next_week {
            weekday_offset = Some(days_until_weekday(committed, &weekday, true));
        }
    }
    let day_offset = weekday_offset.or_else(|| {
        if re!(r"\btomorrow\b").is_match(&lower) {
            Some(1)
        } else if re!(r"\btoday\b").is_match(&lower) {
            Some(0)
        } else {
            None
        }
    });
    let patterns: [&Regex; 4] = [
        re!(&format!(
            r"(?i)\bat\s+(\d{{1,2}})(?::(\d{{2}}))?\s*({MERIDIEM})\b"
        )),
        re!(&format!(
            r"(?i)\b(\d{{1,2}})(?::(\d{{2}}))?\s*({MERIDIEM})\b"
        )),
        re!(&format!(r"(?i)\b(\d{{1,2}})\s*({MERIDIEM})\b")),
        re!(r"\b(\d{1,2}):(\d{2})\b"),
    ];
    for pattern in patterns {
        let Some(caps) = pattern.captures(text) else {
            continue;
        };
        let Some(hour_raw) = caps.get(1).and_then(|m| m.as_str().parse::<u32>().ok()) else {
            continue;
        };
        let group2 = caps.get(2).map(|m| m.as_str());
        let group3 = caps.get(3).map(|m| m.as_str());
        let minute = match group2 {
            Some(text) if !starts_with_meridiem_letter(text) => text.parse::<u32>().unwrap_or(0),
            _ => 0,
        };
        if re!(r"\b(?:shop|store|business)\s+close").is_match(&lower) {
            continue;
        }
        let matched = caps.get(0).map_or("", |m| m.as_str());
        let meridiem = match group3 {
            Some(text) if starts_with_meridiem_letter(text) => Some(text),
            _ => group2,
        };
        let hour = match meridiem {
            Some(text) if starts_with_meridiem_letter(text) => {
                to_24_hour(hour_raw, Some(text), matched)
            }
            _ => hour_without_meridiem(hour_raw),
        };
        let date = store_date_plus(committed, day_offset.unwrap_or(0));
        return Some(Appointment {
            at: wall_to_utc(date, hour, minute),
            explicit_time: true,
        });
    }
    weekday_offset.map(|offset| Appointment {
        at: wall_to_utc(store_date_plus(committed, offset), ONSITE_MORNING_HOUR, 0),
        explicit_time: false,
    })
}

fn scheduled_appointment(transcript: &str, committed: DateTime<Utc>) -> Option<Appointment> {
    let chunks = sentences(transcript);
    for chunk in chunks.iter().rev() {
        if !chunk_confirms_appointment(chunk) {
            continue;
        }
        if let Some(found) = appointment_from_text(chunk, committed, Some(transcript)) {
            return Some(found);
        }
    }
    let timed: Vec<&String> = chunks
        .iter()
        .filter(|chunk| {
            let lower = chunk.to_lowercase();
            if !explicit_time().is_match(&lower)
                && !re!(r"\b\d{1,2}:\d{2}\b").is_match(&lower)
                && !re!(r"(?i)\b\d{1,2}\s*o['']?clock\b").is_match(&lower)
            {
                return false;
            }
            if re!(r"\b(?:shop|store|business)\s+close").is_match(&lower) {
                return false;
            }
            re!(r"\b(?:estimate|measurement|measure|showroom|appointment|be here|stop by|see you|there at|come|visit)\b").is_match(&lower)
        })
        .collect();
    for chunk in timed.iter().rev() {
        if let Some(found) = appointment_from_text(chunk, committed, Some(transcript)) {
            return Some(found);
        }
    }
    if re!(r"(?i)\b(?:estimate|measurement|measure|showroom|appointment|visit)\b")
        .is_match(transcript)
        && let Some(found) = appointment_from_text(transcript, committed, Some(transcript))
    {
        return Some(found);
    }
    scheduled_weekday_offset(transcript, committed).map(|offset| Appointment {
        at: wall_to_utc(store_date_plus(committed, offset), ONSITE_MORNING_HOUR, 0),
        explicit_time: false,
    })
}

fn onsite_name(appointment: Appointment) -> String {
    let local = appointment.at.with_timezone(&Indianapolis);
    let weekday = local.format("%a");
    if !appointment.explicit_time {
        return format!("{CONFIRMED_ONSITE_ESTIMATE_NAME} on {weekday}");
    }
    let hour12 = match local.hour() % 12 {
        0 => 12,
        hour => hour,
    };
    let meridiem = if local.hour() >= 12 { "PM" } else { "AM" };
    let minute = if local.minute() == 0 {
        String::new()
    } else {
        format!(":{:02}", local.minute())
    };
    format!("{CONFIRMED_ONSITE_ESTIMATE_NAME} at {weekday} {hour12}{minute} {meridiem}")
}

fn onsite_reminder(appointment: Appointment) -> String {
    let local = appointment.at.with_timezone(&Indianapolis);
    let day = local.date_naive();
    if !appointment.explicit_time || local.hour() >= ONSITE_NOON_HOUR {
        return wall_mysql(day, ONSITE_MORNING_HOUR, 0);
    }
    wall_mysql(store_date_plus(appointment.at, -1), ONSITE_EVENING_HOUR, 0)
}

fn onsite_reminder_deadline(
    name: &str,
    transcript: &str,
    committed: DateTime<Utc>,
) -> Option<String> {
    if !is_onsite_estimate(name) || !transcript_confirms_visit(transcript) {
        return None;
    }
    scheduled_appointment(transcript, committed).map(onsite_reminder)
}

fn apply_onsite_estimate(
    activity: CallActivity,
    committed: DateTime<Utc>,
    transcript: &str,
) -> CallActivity {
    if !is_onsite_estimate(&activity.name) {
        return activity;
    }
    scheduled_appointment(transcript, committed).map_or(activity, |appointment| CallActivity {
        name: onsite_name(appointment),
        deadline: Some(onsite_reminder(appointment)),
    })
}

// --- voicemail ----------------------------------------------------------------

fn is_voicemail_follow_up(name: &str) -> bool {
    let lower = name.to_lowercase();
    re!(r"\bfollow[- ]?up\b").is_match(&lower)
        || re!(r"\bconfirm\s+interest\b").is_match(&lower)
        || re!(r"\bcall\s+back\b").is_match(&lower)
}

fn on_committed_store_day(deadline: &str, committed: DateTime<Utc>, days: i64) -> bool {
    let expected = store_date_only_plus(committed, days);
    let trimmed = deadline.trim();
    if re!(r"^\d{4}-\d{2}-\d{2}$").is_match(trimmed) {
        return trimmed == expected;
    }
    parse_js_date(trimmed).is_some_and(|at| store_date_only_plus(at, 0) == expected)
}

/// A voicemail follow-up keeps only a deadline today or tomorrow.
pub fn voicemail_follow_up_deadline(
    deadline: Option<&str>,
    name: &str,
    committed: DateTime<Utc>,
) -> Option<String> {
    if !is_voicemail_follow_up(name) {
        return deadline.map(str::to_string);
    }
    let normalized = normalize_timed(deadline?)?;
    (on_committed_store_day(&normalized, committed, 0)
        || on_committed_store_day(&normalized, committed, 1))
    .then_some(normalized)
}

// --- sends, quotes, photos ------------------------------------------------------

fn is_quote_or_estimate(name: &str) -> bool {
    let lower = name.to_lowercase();
    if is_call_back(&lower) {
        return false;
    }
    let quote = re!(r"\b(quote|estimate|pricing|bid)\b");
    if re!(r"\bsend\b").is_match(&lower) && quote.is_match(&lower) {
        return true;
    }
    quote.is_match(&lower) && re!(r"\b(?:send|deliver|email)\b").is_match(&lower)
}

fn is_send_link_style(name: &str) -> bool {
    re!(r"\bsend\b").is_match(name)
        && re!(r"\b(link|option|inventory|website|granite|stone|sample|pricing)\b").is_match(name)
}

fn is_salesperson_send(name: &str) -> bool {
    let lower = name.to_lowercase();
    re!(r"\bsend\b").is_match(&lower) && !is_customer_photo_request(&lower)
}

fn tomorrow_is_customer_commitment(transcript: &str) -> bool {
    let lower = transcript.to_lowercase();
    re!(r"\b(?:pictures?|photos?|pics?|measurements?|kitchen)\b[^.]{0,140}\btomorrow\b")
        .is_match(&lower)
        || re!(r"\btomorrow\b[^.]{0,140}\b(?:pictures?|photos?|pics?|measurements?)")
            .is_match(&lower)
        || re!(r"\bhusband\b[^.]{0,100}\btomorrow\b").is_match(&lower)
        || (re!(r"\b(?:send|get)\b[^.]{0,100}\b(?:pictures?|photos?|measurements?)\b")
            .is_match(&lower)
            && re!(r"\btomorrow\b").is_match(&lower)
            && re!(r"\b(?:we would|i could|can have|from him|my husband)\b").is_match(&lower))
}

fn window_is_rep_send(window: &str) -> bool {
    let lower = window.to_lowercase();
    if !re!(r"\bsend\b").is_match(&lower) {
        return false;
    }
    let customer_sending = re!(r"\b(?:husband|she|he|they|my husband|we would send you)\b")
        .is_match(&lower)
        && !re!(r"\b(?:i(?:'ll| will)|we will)\s+send\b").is_match(&lower);
    if customer_sending {
        return false;
    }
    re!(r"\b(?:i(?:'ll| will)|we will|i can|we can)\s+send\b").is_match(&lower)
        || re!(r"\b(?:i(?:'ll| will)|we can)\s+send you\b").is_match(&lower)
        || re!(r"\bsend you (?:the |our )?(?:address|link|inventory|live|pictures?|photos?)\b")
            .is_match(&lower)
}

fn rep_committed_to_send(name: &str, transcript: &str) -> bool {
    let name = name.to_lowercase();
    let lower = transcript.to_lowercase();
    let offers = re!(r"\b(?:i(?:'ll| will)|i can|we can|we will)\s+send\b").is_match(&lower)
        || re!(r"\b(?:i(?:'ll| will)|we can)\s+send you\b").is_match(&lower);
    if !offers {
        return false;
    }
    if re!(r"\b(?:inventory|link)\b").is_match(&name) {
        return re!(r"\b(?:inventory|live inventory|link)\b").is_match(&lower);
    }
    if re!(r"\bsink\b").is_match(&name) {
        return re!(r"\bsink").is_match(&lower);
    }
    if re!(r"\b(?:address|showroom)\b").is_match(&name) {
        return re!(r"\b(?:address|showroom)\b").is_match(&lower);
    }
    if is_edge_photo_send(&name) {
        return re!(r"\b(?:sink|edge|profile)\b").is_match(&lower);
    }
    re!(r"\bsend\b").is_match(&name)
}

fn rep_send_says_tomorrow(name: &str, transcript: &str) -> bool {
    let lower = transcript.to_lowercase();
    let says = re!(r"\b(?:i(?:'ll| will)|we will|i can|we can)\s+send\b[^.]{0,140}\btomorrow\b")
        .is_match(&lower)
        || re!(r"\bsend\b[^.]{0,80}\btomorrow\b[^.]{0,40}\b(?:you|the )").is_match(&lower);
    says && rep_committed_to_send(name, transcript)
}

fn rep_send_default_deadline(
    name: &str,
    transcript: &str,
    committed: DateTime<Utc>,
) -> Option<String> {
    let lower_name = name.to_lowercase();
    if !is_salesperson_send(&lower_name) || is_customer_photo_request(&lower_name) {
        return None;
    }
    if is_quote_or_estimate(&lower_name) {
        let lower = transcript.to_lowercase();
        if tomorrow_is_customer_commitment(transcript)
            && re!(r"\b(?:measurements?|measure|pictures?|photos?)\b").is_match(&lower)
        {
            return Some(store_date_only_plus(committed, 1));
        }
        if re!(r"\b(?:whenever|when|after|once)\b[^.]{0,80}\bmeasurements?\b").is_match(&lower)
            && tomorrow_is_customer_commitment(transcript)
        {
            return Some(store_date_only_plus(committed, 1));
        }
        return None;
    }
    if rep_send_says_tomorrow(name, transcript) {
        return Some(store_date_only_plus(committed, 1));
    }
    rep_committed_to_send(name, transcript).then(|| store_date_only_plus(committed, 0))
}

fn is_soon_send(name: &str, transcript_lower: &str) -> bool {
    let lower = name.to_lowercase();
    if !is_salesperson_send(&lower) {
        return false;
    }
    if immediate().is_match(&lower) {
        return true;
    }
    re!(r"\b(?:i(?:'ll| will)|we will)\s+send\b[^.]{0,180}\b(?:a little (?:bit )?later|shortly|right away|right now|immediately|asap|now)\b").is_match(transcript_lower)
        || re!(r"\bsend you the link\b[^.]{0,80}\b(?:a little (?:bit )?later|shortly)\b").is_match(transcript_lower)
        || re!(r"\bsend\b[^.]{0,120}\b(?:a little (?:bit )?later|shortly)\b").is_match(transcript_lower)
        || re!(r"\b(?:a little (?:bit )?later|shortly)\b[^.]{0,100}\b(?:send|sending)\b[^.]{0,100}\b(?:the )?(?:link|options?|inventory|website)\b").is_match(transcript_lower)
}

fn quote_by_tomorrow(name: &str, deadline: &str, transcript_lower: &str) -> bool {
    if !is_quote_or_estimate(name) {
        return false;
    }
    if re!(r"\b(?:quote|estimate|pricing).{0,80}\bby tomorrow\b").is_match(transcript_lower)
        || re!(r"\bby tomorrow.{0,80}\b(?:quote|estimate|pricing)\b").is_match(transcript_lower)
        || (re!(r"\b(?:i(?:'ll| will)|we will)\b").is_match(transcript_lower)
            && re!(r"\b(?:quote|estimate|pricing)\b").is_match(transcript_lower)
            && re!(r"\bby tomorrow\b").is_match(transcript_lower))
    {
        return true;
    }
    tomorrow().is_match(name) || tomorrow().is_match(deadline)
}

fn mentions_explicit_time(haystacks: &[&str]) -> bool {
    haystacks
        .iter()
        .any(|text| explicit_time().is_match(text) || re!(r"T\d{2}:\d{2}").is_match(text))
}

fn coerce_day_only(normalized: &str, name: &str, transcript_lower: &str) -> bool {
    if !re!(r"^\d{4}-\d{2}-\d{2}").is_match(normalized) || is_quote_or_estimate(name) {
        return false;
    }
    let name_lower = name.to_lowercase();
    let deadline_lower = normalized.to_lowercase();
    let today_mentioned = today().is_match(&name_lower)
        || today().is_match(&deadline_lower)
        || (re!(r"\b(?:i(?:'ll| will)|we will)\b").is_match(transcript_lower)
            && re!(r"\btoday\b").is_match(transcript_lower));
    today_mentioned && !mentions_explicit_time(&[&name_lower, &deadline_lower, transcript_lower])
}

fn normalize_timed(deadline: &str) -> Option<String> {
    let trimmed = deadline.trim();
    if trimmed.is_empty() {
        return None;
    }
    if re!(r"^\d{4}-\d{2}-\d{2}$").is_match(trimmed) {
        return Some(trimmed.to_string());
    }
    let has_zone = re!(r"[zZ]$|[+-]\d{2}:\d{2}$").is_match(trimmed);
    let candidate = if has_zone {
        trimmed.to_string()
    } else {
        format!("{trimmed}Z")
    };
    parse_js_date(&candidate).map(iso)
}

fn transcript_windows(name: &str, transcript: &str) -> Vec<String> {
    let lower = name.to_lowercase();
    let chunks = sentences(transcript);
    if chunks.is_empty() {
        return vec![transcript.to_lowercase()];
    }
    let mut terms: Vec<String> = Vec::new();
    let mut add = |words: &[&str]| terms.extend(words.iter().map(|word| (*word).to_string()));
    if is_send_link_style(&lower)
        || (re!(r"\bsend\b").is_match(&lower) && re!(r"\blink\b").is_match(&lower))
    {
        add(&["send", "link", "text", "options", "inventory", "website"]);
    }
    if is_edge_photo_send(&lower) {
        add(&["edge", "picture", "photo", "pic"]);
    }
    if is_call_back(&lower) {
        add(&[
            "call",
            "follow",
            "week",
            "friday",
            "monday",
            "next",
            "check",
            "quote",
            "questions",
        ]);
    }
    if is_quote_or_estimate(&lower) {
        add(&["quote", "estimate", "pricing", "bid"]);
    }
    if is_onsite_estimate(&lower) {
        add(&[
            "estimate",
            "measurement",
            "measure",
            "showroom",
            "appointment",
            "saturday",
            "be here",
            "stop by",
            "see you",
            "today",
        ]);
    }
    if is_stone_color_photo_request(&lower) {
        add(&[
            "photo", "picture", "pic", "color", "quartz", "stone", "material", "menards",
        ]);
    }
    if is_kitchen_photo_request(&lower) {
        add(&["photo", "picture", "pic", "kitchen", "layout", "countertop"]);
    }
    if is_long_lead_request(&lower) || is_customer_photo_request(&lower) {
        add(&["photo", "picture", "pic", "send", "afternoon", "today"]);
    }
    if terms.is_empty() {
        terms.extend(
            lower
                .split_whitespace()
                .filter(|word| word.chars().count() > 3)
                .map(str::to_string),
        );
    }
    let hits: Vec<String> = chunks
        .iter()
        .map(|chunk| chunk.to_lowercase())
        .filter(|chunk| terms.iter().any(|term| chunk.contains(term.as_str())))
        .collect();
    if hits.is_empty() {
        vec![transcript.to_lowercase()]
    } else {
        hits
    }
}

fn explicit_deadline(text: &str, committed: DateTime<Utc>) -> Option<String> {
    let lower = text.to_lowercase();
    let weekday = |pattern: &Regex| {
        pattern
            .captures(&lower)
            .and_then(|caps| caps.get(1))
            .map(|m| m.as_str().to_string())
    };
    if let Some(day) = weekday(re!(
        r"\b(?:you\s+can\s+call\s+me|call\s+me)\s+next\s+(monday|tuesday|wednesday|thursday|friday|saturday|sunday)\b"
    )) {
        return Some(store_date_only_plus(
            committed,
            days_until_weekday(committed, &day, true),
        ));
    }
    let next = weekday(re!(
        r"\bnext\s+(monday|tuesday|wednesday|thursday|friday|saturday|sunday)\b"
    ));
    if let Some(day) = &next {
        return Some(store_date_only_plus(
            committed,
            days_until_weekday(committed, day, true),
        ));
    }
    if let Some(day) = weekday(re!(
        r"\b(?:on\s+)?(monday|tuesday|wednesday|thursday|friday|saturday|sunday)\b"
    )) {
        return Some(store_date_only_plus(
            committed,
            days_until_weekday(committed, &day, false),
        ));
    }
    let day_offset = if re!(r"\btomorrow\b").is_match(&lower) {
        Some(1)
    } else if re!(r"\btoday\b").is_match(&lower)
        || re!(r"\b(?:this\s+afternoon|this\s+evening|tonight)\b").is_match(&lower)
    {
        Some(0)
    } else {
        None
    };
    let patterns: [&Regex; 3] = [
        re!(&format!(
            r"(?i)\bat\s+(\d{{1,2}})(?::(\d{{2}}))?\s*({MERIDIEM})\b"
        )),
        re!(&format!(
            r"(?i)\b(\d{{1,2}})(?::(\d{{2}}))?\s*({MERIDIEM})\b"
        )),
        re!(&format!(r"(?i)\b(\d{{1,2}})\s*({MERIDIEM})\b")),
    ];
    for pattern in patterns {
        let Some(caps) = pattern.captures(text) else {
            continue;
        };
        let Some(hour_raw) = caps.get(1).and_then(|m| m.as_str().parse::<u32>().ok()) else {
            continue;
        };
        // The third pattern has no minutes: its meridiem is group 2.
        let (minute_text, meridiem) = if caps.len() == 3 {
            (None, caps.get(2).map(|m| m.as_str()))
        } else {
            (
                caps.get(2).map(|m| m.as_str()),
                caps.get(3).map(|m| m.as_str()),
            )
        };
        let minute = match minute_text {
            Some(text) => match text.parse::<u32>() {
                Ok(minute) => minute,
                Err(_) => continue,
            },
            None => 0,
        };
        let matched = caps.get(0).map_or("", |m| m.as_str());
        let hour = to_24_hour(hour_raw, meridiem, matched);
        let date = store_date_plus(committed, day_offset.unwrap_or(0));
        return Some(iso(wall_to_utc(date, hour, minute)));
    }
    day_offset.map(|offset| store_date_only_plus(committed, offset))
}

fn explicit_deadline_for(name: &str, transcript: &str, committed: DateTime<Utc>) -> Option<String> {
    let windows = transcript_windows(name, transcript);
    if is_salesperson_send(name) && !is_customer_photo_request(name) {
        return windows
            .iter()
            .filter(|window| window_is_rep_send(window))
            .find_map(|window| explicit_deadline(window, committed));
    }
    windows
        .iter()
        .find_map(|window| explicit_deadline(window, committed))
}

fn is_photo_request(name: &str) -> bool {
    re!(r"\b(request|wait for|receive)\b").is_match(name)
        && re!(r"\b(photo|picture|pic|image)").is_match(name)
}

fn is_stone_color_photo_request(name: &str) -> bool {
    let lower = name.to_lowercase();
    (re!(r"\b(?:request|wait for|receive)\b").is_match(&lower)
        && re!(r"\b(?:color|stone|quartz|granite|material)\b").is_match(&lower)
        && re!(r"\b(?:photo|picture|pic)\b").is_match(&lower))
        || re!(r"\b(?:preferred|exact)\s+(?:stone|color|quartz)\b").is_match(&lower)
}

fn is_kitchen_photo_request(name: &str) -> bool {
    let lower = name.to_lowercase();
    if is_stone_color_photo_request(&lower) {
        return false;
    }
    is_photo_request(&lower)
        || (re!(r"\b(photo|picture|pic)").is_match(&lower)
            && re!(r"\b(kitchen|countertop|counter|layout)\b").is_match(&lower))
}

fn is_customer_photo_request(name: &str) -> bool {
    is_stone_color_photo_request(name) || is_kitchen_photo_request(name)
}

fn sanitize_name(name: &str) -> String {
    strip_calendar_dates(re!(r"(?i)\s+from\s+[A-Za-z]+\s*$").replace(name, "").trim())
}

fn is_edge_photo_send(name: &str) -> bool {
    let lower = name.to_lowercase();
    re!(r"\bsend\b").is_match(&lower)
        && re!(r"\b(edge|profile)\b").is_match(&lower)
        && re!(r"\b(photo|picture|pic)s?\b").is_match(&lower)
}

fn canonical_edge_photo_name(lower: &str) -> &'static str {
    if re!(r"\bsquare(d)?\b").is_match(lower) {
        "Send square edge profile photos"
    } else if re!(r"\bflat\b").is_match(lower) {
        "Send flat edge profile photos"
    } else if re!(r"\b(?:slightly )?rounded\b").is_match(lower) {
        "Send rounded edge profile photos"
    } else if re!(r"\bbullnose\b").is_match(lower) {
        "Send bullnose edge profile photos"
    } else if re!(r"\bogee\b").is_match(lower) {
        "Send ogee edge profile photos"
    } else if re!(r"\b1/4\s*bevel\b").is_match(lower) {
        "Send 1/4 bevel edge profile photos"
    } else if re!(r"\b1/2\s*bevel\b").is_match(lower) {
        "Send 1/2 bevel edge profile photos"
    } else if re!(r"\bbevel\b").is_match(lower) {
        "Send bevel edge profile photos"
    } else {
        "Send edge profile photos"
    }
}

fn canonical_name(name: &str) -> String {
    let sanitized = sanitize_name(name);
    let lower = sanitized.to_lowercase();
    if is_stone_color_photo_request(&lower) {
        return STONE_COLOR_PHOTO_ACTIVITY_NAME.into();
    }
    if is_kitchen_photo_request(&lower) {
        return KITCHEN_PHOTO_ACTIVITY_NAME.into();
    }
    if is_edge_photo_send(&lower) {
        return canonical_edge_photo_name(&lower).into();
    }
    sanitized
}

fn dedup_key(name: &str) -> String {
    let lower = canonical_name(name).to_lowercase();
    if is_stone_color_photo_request(&lower) {
        return "request-stone-color-photo".into();
    }
    if is_kitchen_photo_request(&lower) {
        return "request-kitchen-photos".into();
    }
    if re!(r"\bsend\b").is_match(&lower)
        && re!(r"\b(link|option|inventory|website)\b").is_match(&lower)
    {
        return "send-options-link".into();
    }
    if is_edge_photo_send(&lower) {
        return "send-edge-profile-photos".into();
    }
    re!(r"[^a-z0-9]+")
        .replace_all(&lower, " ")
        .trim()
        .to_string()
}

fn rep_edge_photo_offer(transcript_lower: &str) -> bool {
    let discussed = re!(r"\b(?:flat|square|rounded|bevel|bullnose|ogee)\b[^.]{0,100}\bedges?\b")
        .is_match(transcript_lower)
        || re!(r"\bedges?\b[^.]{0,100}\b(?:flat|square|rounded|bevel|bullnose|ogee)\b")
            .is_match(transcript_lower)
        || re!(r"\bprefer\b[^.]{0,140}\bedges?\b").is_match(transcript_lower);
    discussed
        && (re!(r"\bedges?\b[\s\S]{0,280}\b(?:i(?:'ll| will)|we will|can also)\s+send\b[\s\S]{0,140}\b(?:picture|photo|pic)s?\b").is_match(transcript_lower)
            || re!(r"\b(?:i(?:'ll| will)|we will|can also)\s+send\b[\s\S]{0,140}\b(?:picture|photo|pic)s?\b[\s\S]{0,280}\bedges?\b").is_match(transcript_lower))
}

fn edge_profile_label(transcript_lower: &str) -> &'static str {
    if !re!(r"\bedges?\b").is_match(transcript_lower) {
        return "";
    }
    for (pattern, label) in [
        (re!(r"\bsquare(d)?\b"), "square"),
        (re!(r"\bflat\b"), "flat"),
        (re!(r"\b(?:slightly )?rounded\b"), "rounded"),
        (re!(r"\bbullnose\b"), "bullnose"),
        (re!(r"\bogee\b"), "ogee"),
        (re!(r"\b1/4\s*bevel\b"), "1/4 bevel"),
        (re!(r"\b1/2\s*bevel\b"), "1/2 bevel"),
        (re!(r"\bbevel\b"), "bevel"),
    ] {
        if pattern.is_match(transcript_lower) {
            return label;
        }
    }
    ""
}

fn supplement_edge_photos(
    mut activities: Vec<CallActivity>,
    committed: DateTime<Utc>,
    transcript: &str,
) -> Vec<CallActivity> {
    let lower = transcript.to_lowercase();
    if !rep_edge_photo_offer(&lower)
        || activities
            .iter()
            .any(|activity| is_edge_photo_send(&activity.name))
    {
        return activities;
    }
    let profile = edge_profile_label(&lower);
    let name = if profile.is_empty() {
        canonical_edge_photo_name("send edge profile photos")
    } else {
        canonical_edge_photo_name(&format!("send {profile} edge profile photos"))
    }
    .to_string();
    let deadline = deadline_rules(None, &name, committed, transcript);
    activities.push(CallActivity { name, deadline });
    activities
}

fn specific_color_selection(transcript: &str) -> bool {
    let lower = transcript.to_lowercase();
    re!(r"\b(?:menards|home depot|lowe'?s)\b").is_match(&lower)
        || re!(r"\b(?:riviera coast|exact color|specific color|color (?:we|they) (?:want|like|chose))\b").is_match(&lower)
        || re!(r"\bhave\s+(?:a\s+)?color\b").is_match(&lower)
}

fn customer_sends_stone_color_photo(transcript: &str) -> bool {
    let lower = transcript.to_lowercase();
    re!(r"\b(?:picture|photo|pic)\b[^.]{0,140}\b(?:color(?:ing)?|quartz|stone|granite|material)\b")
        .is_match(&lower)
        || re!(r"\b(?:color(?:ing)?|quartz|stone)\b[^.]{0,140}\b(?:picture|photo|pic)\b")
            .is_match(&lower)
        || (specific_color_selection(transcript)
            && re!(r"\b(?:i(?:'ll| will)|going to|can)\b[^.]{0,140}\b(?:picture|photo|pic|send)\b")
                .is_match(&lower))
}

fn supplement_stone_photo(
    mut activities: Vec<CallActivity>,
    committed: DateTime<Utc>,
    transcript: &str,
) -> Vec<CallActivity> {
    if !customer_sends_stone_color_photo(transcript)
        || activities
            .iter()
            .any(|activity| is_stone_color_photo_request(&activity.name))
    {
        return activities;
    }
    let deadline = deadline_rules(None, STONE_COLOR_PHOTO_ACTIVITY_NAME, committed, transcript);
    activities.push(CallActivity {
        name: STONE_COLOR_PHOTO_ACTIVITY_NAME.into(),
        deadline,
    });
    activities
}

fn deadline_from_transcript(
    name: &str,
    transcript: &str,
    committed: DateTime<Utc>,
) -> Option<String> {
    let name_lower = name.to_lowercase();
    let lower = transcript.to_lowercase();
    if let Some(explicit) = explicit_deadline_for(name, transcript, committed) {
        return Some(explicit);
    }
    if is_customer_photo_request(&name_lower) {
        if re!(r"\b(?:this\s+afternoon|by this afternoon|today|at the latest)\b").is_match(&lower)
            && re!(r"\b(?:picture|photo|pic|send)\b").is_match(&lower)
        {
            return Some(store_date_only_plus(committed, 0));
        }
        let tomorrow_send = re!(r"\b(?:she|he|they|customer)\b[^.]{0,120}\btomorrow\b[^.]{0,120}\b(?:photo|picture|pic|kitchen|countertop|color|quartz|stone)\b").is_match(&lower)
            || re!(r"\b(?:photo|picture|pic|kitchen|countertop|color|quartz|stone)\b[^.]{0,120}\btomorrow\b").is_match(&lower)
            || (re!(r"\bi(?:'ll| will) send (?:you )?(?:the )?(?:photo|picture|pic|kitchen)").is_match(&lower)
                && re!(r"\btomorrow\b").is_match(&lower));
        if tomorrow_send {
            return Some(store_date_only_plus(committed, 1));
        }
    }
    if is_quote_or_estimate(&name_lower)
        && re!(r"\bby tomorrow\b").is_match(&lower)
        && re!(r"\b(?:quote|estimate|pricing)\b").is_match(&lower)
    {
        return Some(store_close_deadline(committed, 0));
    }
    if is_send_link_style(&name_lower) {
        if is_soon_send(name, &lower) {
            return Some(iso(committed + Duration::minutes(30)));
        }
        if re!(r"\b(?:i(?:'ll| will)|we will)\s+send\b[^.]{0,120}\btoday\b").is_match(&lower)
            || re!(r"\bsend\b[^.]{0,80}\btoday\b").is_match(&lower)
        {
            return Some(store_date_only_plus(committed, 0));
        }
    }
    None
}

/// The CRM's `applyActivityDeadlineRules`.
fn deadline_rules(
    deadline: Option<&str>,
    name: &str,
    committed: DateTime<Utc>,
    transcript: &str,
) -> Option<String> {
    if is_voicemail_follow_up(name) {
        return voicemail_follow_up_deadline(deadline, name, committed);
    }
    if let Some(deadline) = rep_two_three_week_deadline(name, transcript, committed) {
        return Some(deadline);
    }
    if let Some(reminder) = onsite_reminder_deadline(name, transcript, committed) {
        return Some(reminder);
    }
    if is_call_back(name) {
        if let Some(timed) = timed_call_back_deadline(transcript, committed) {
            return Some(timed);
        }
        if let Some(agreed) = agreed_call_back_deadline(transcript, committed) {
            return Some(agreed);
        }
    }
    let name_lower = name.to_lowercase();
    let deadline_lower = deadline.unwrap_or("").to_lowercase();
    let lower = transcript.to_lowercase();
    if is_salesperson_send(name)
        && !is_customer_photo_request(name)
        && let Some(found) = rep_send_default_deadline(name, transcript, committed)
    {
        return Some(found);
    }
    if let Some(explicit) = explicit_deadline_for(name, transcript, committed) {
        return Some(explicit);
    }
    if is_soon_send(name, &lower) {
        return Some(iso(committed + Duration::minutes(30)));
    }
    if is_quote_or_estimate(&name_lower) {
        if quote_by_tomorrow(&name_lower, &deadline_lower, &lower) {
            return Some(store_close_deadline(committed, 0));
        }
        if today().is_match(&name_lower)
            || today().is_match(&deadline_lower)
            || re!(r"\b(?:before close|end of day|by close|closing at|close at|6\s*pm|6:00)\b")
                .is_match(&lower)
        {
            return Some(store_close_deadline(committed, 0));
        }
    }
    if let Some(normalized) = deadline.and_then(normalize_timed) {
        if coerce_day_only(&normalized, name, &lower) {
            return Some(store_date_only_plus(committed, 0));
        }
        if is_send_link_style(&name_lower) {
            let call_day = store_date_only_plus(committed, 0);
            if normalized == call_day
                || normalized.starts_with(&format!("{call_day}T"))
                || today().is_match(&name_lower)
                || today().is_match(&deadline_lower)
            {
                return Some(call_day);
            }
        }
        return Some(normalized);
    }
    if tomorrow().is_match(&name_lower) || tomorrow().is_match(&deadline_lower) {
        if is_customer_photo_request(name) || is_quote_or_estimate(&name_lower) {
            if tomorrow_is_customer_commitment(transcript) {
                return Some(store_date_only_plus(committed, 1));
            }
        } else if is_salesperson_send(name) {
            return Some(
                rep_send_default_deadline(name, transcript, committed)
                    .unwrap_or_else(|| store_date_only_plus(committed, 0)),
            );
        }
        return Some(store_date_only_plus(committed, 1));
    }
    if today().is_match(&name_lower) || today().is_match(&deadline_lower) {
        return Some(store_date_only_plus(committed, 0));
    }
    deadline_from_transcript(name, transcript, committed)
}

fn deduplicate(activities: Vec<CallActivity>) -> Vec<CallActivity> {
    let mut order: Vec<String> = Vec::new();
    let mut by_key: HashMap<String, CallActivity> = HashMap::new();
    for activity in activities {
        let name = canonical_name(&activity.name);
        let key = dedup_key(&name);
        if let Some(existing) = by_key.get_mut(&key) {
            if existing.deadline.is_none() {
                existing.deadline = activity.deadline;
            }
        } else {
            order.push(key.clone());
            by_key.insert(
                key,
                CallActivity {
                    name,
                    deadline: activity.deadline,
                },
            );
        }
    }
    order
        .into_iter()
        .filter_map(|key| by_key.remove(&key))
        .collect()
}

fn is_long_lead_request(name: &str) -> bool {
    let lower = name.to_lowercase();
    is_kitchen_photo_request(&lower)
        || is_stone_color_photo_request(&lower)
        || (re!(r"\brequest\b").is_match(&lower) && !re!(r"\bsend\b").is_match(&lower))
}

fn sends_photos(lower: &str) -> bool {
    is_edge_photo_send(lower)
        || (re!(r"\bsend\b").is_match(lower) && re!(r"\b(photo|picture|pic)s?\b").is_match(lower))
}

fn scheduling_order(name: &str) -> u8 {
    let lower = name.to_lowercase();
    if is_long_lead_request(&lower) {
        return 0;
    }
    if re!(r"\bsend\b").is_match(&lower)
        && re!(r"\b(link|option|inventory|website)\b").is_match(&lower)
    {
        return 1;
    }
    if sends_photos(&lower) {
        return 2;
    }
    if is_quote_or_estimate(&lower) {
        return 3;
    }
    2
}

fn quote_minimum_deadline(
    deadline: Option<String>,
    name: &str,
    committed: DateTime<Utc>,
) -> Option<String> {
    if !is_quote_or_estimate(name) {
        return deadline;
    }
    let minimum = committed + Duration::minutes(QUOTE_MINIMUM_LEAD_MINUTES);
    let Some(deadline) = deadline else {
        return Some(iso(minimum));
    };
    let trimmed = deadline.trim();
    if re!(r"^\d{4}-\d{2}-\d{2}$").is_match(trimmed) {
        return Some(trimmed.to_string());
    }
    match parse_js_date(trimmed) {
        Some(at) if is_timed(trimmed) && at >= minimum => Some(trimmed.to_string()),
        _ => Some(iso(minimum)),
    }
}

fn minutes_before_next(name: &str) -> i64 {
    if is_quote_or_estimate(&name.to_lowercase()) {
        QUOTE_MINIMUM_LEAD_MINUTES
    } else {
        2
    }
}

fn stagger(activities: Vec<CallActivity>) -> Vec<CallActivity> {
    let mut last: Option<(DateTime<Utc>, String)> = None;
    activities
        .into_iter()
        .map(|activity| {
            let Some(current) = activity
                .deadline
                .as_deref()
                .filter(|deadline| is_timed(deadline))
                .and_then(parse_js_date)
            else {
                last = None;
                return activity;
            };
            let Some((previous, previous_name)) = last.take() else {
                last = Some((current, activity.name.clone()));
                return activity;
            };
            let assigned =
                current.max(previous + Duration::minutes(minutes_before_next(&previous_name)));
            last = Some((assigned, activity.name.clone()));
            if assigned == current {
                activity
            } else {
                CallActivity {
                    deadline: Some(iso(assigned)),
                    ..activity
                }
            }
        })
        .collect()
}

fn filter_unaccepted_links(activities: Vec<CallActivity>, transcript: &str) -> Vec<CallActivity> {
    if !customer_will_send_material_photo(transcript) || !specific_color_selection(transcript) {
        return activities;
    }
    activities
        .into_iter()
        .filter(|activity| !is_send_link_style(&activity.name))
        .collect()
}

/// The CRM's `customerWillSendMaterialPhoto` (`callAiHelpers.ts`).
fn customer_will_send_material_photo(transcript: &str) -> bool {
    let lower = transcript.to_lowercase();
    re!(r"\b(?:i(?:'ll| will)|she|he|we will|can|going to)\s+(?:go ahead and\s+)?send\b[^.]{0,120}\b(?:picture|photo|pic|image)\b").is_match(&lower)
        || re!(r"\bsend\s+(?:you\s+)?(?:a\s+)?(?:picture|photo|pic|image)\b[^.]{0,120}\b(?:kitchen|layout|color|quartz|tile|countertop)\b").is_match(&lower)
        || re!(r"\b(?:picture|photo|pic)\b[^.]{0,120}\b(?:kitchen\s+layout|color(?:ing)?|quartz|tile)\b").is_match(&lower)
}

fn budget_amount(raw: &str) -> Option<f64> {
    raw.replace(',', "")
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())
}

fn format_budget(raw: &str) -> String {
    let Some(value) = budget_amount(raw) else {
        return format!("${raw}");
    };
    let grouped = |whole: u64| {
        let digits = whole.to_string();
        let mut out = String::new();
        for (index, ch) in digits.chars().enumerate() {
            if index > 0 && (digits.len() - index).is_multiple_of(3) {
                out.push(',');
            }
            out.push(ch);
        }
        out
    };
    if value.fract() == 0.0 {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        return format!("${}", grouped(value as u64));
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let cents = (value * 100.0).round() as u64;
    format!("${}.{:02}", grouped(cents / 100), cents % 100)
}

/// The CRM's `extractBudgetFromTranscript` (`callAiHelpers.ts`).
pub fn extract_budget(transcript: &str) -> Option<String> {
    let plausible =
        |raw: &str| budget_amount(raw).is_some_and(|value| (500.0..=500_000.0).contains(&value));
    if let Some(raw) = re!(r"(?i)\b(?:below|under|around|about|right about|stay(?:ing)?(?:\s+below|\s+under)?|target(?:ing)?|budget(?:\s+of)?|try to stay under)\s*\$?\s*([\d,]+(?:\.\d{2})?)\b")
        .captures(transcript)
        .and_then(|caps| caps.get(1))
        && plausible(raw.as_str())
    {
        return Some(format_budget(raw.as_str()));
    }
    for caps in re!(r"\$\s*([\d,]+(?:\.\d{2})?)\b").captures_iter(transcript) {
        let (Some(whole), Some(raw)) = (caps.get(0), caps.get(1)) else {
            continue;
        };
        if !plausible(raw.as_str()) {
            continue;
        }
        let start = transcript[..whole.start()]
            .char_indices()
            .rev()
            .nth(89)
            .map_or(0, |(index, _)| index);
        let end = transcript[whole.end()..]
            .char_indices()
            .nth(40)
            .map_or(transcript.len(), |(index, _)| whole.end() + index);
        let context = transcript[start..end].to_lowercase();
        if re!(r"\b(?:budget|target|below|under|about|around|stay|league|installation|quartz|granite|estimate|quote|trying to)\b").is_match(&context) {
            return Some(format_budget(raw.as_str()));
        }
    }
    None
}

fn inject_budget(activities: Vec<CallActivity>, transcript: &str) -> Vec<CallActivity> {
    let Some(budget) = extract_budget(transcript) else {
        return activities;
    };
    activities
        .into_iter()
        .map(|activity| {
            let lower = activity.name.to_lowercase();
            if !is_quote_or_estimate(&lower) || re!(r"\$\s*[\d,]+").is_match(&activity.name) {
                return activity;
            }
            if re!(r"\bunder\s+budget\b").is_match(&lower) {
                return CallActivity {
                    name: re!(r"(?i)\bunder\s+budget\b")
                        .replace(&activity.name, regex::NoExpand(&format!("under {budget}")))
                        .into_owned(),
                    ..activity
                };
            }
            if re!(r"\b(?:prepare|send)\b").is_match(&lower)
                && re!(r"\b(?:estimate|quote)\b").is_match(&lower)
            {
                return CallActivity {
                    name: format!("{} under {budget}", activity.name),
                    ..activity
                };
            }
            activity
        })
        .collect()
}

/// The CRM's `finalizeExtractedActivities`.
pub fn finalize(
    activities: Vec<CallActivity>,
    committed: DateTime<Utc>,
    transcript: &str,
) -> Vec<CallActivity> {
    let activities = deduplicate(activities);
    let activities = supplement_edge_photos(activities, committed, transcript);
    let activities = supplement_stone_photo(activities, committed, transcript);
    let activities = filter_unaccepted_links(activities, transcript);
    let activities = inject_budget(activities, transcript);
    let mut activities = deduplicate(activities);
    activities.sort_by_key(|activity| scheduling_order(&activity.name));
    stagger(activities)
        .into_iter()
        .map(|activity| CallActivity {
            deadline: quote_minimum_deadline(activity.deadline.clone(), &activity.name, committed),
            ..activity
        })
        .map(|activity| {
            apply_agreed_call_back(
                apply_onsite_estimate(
                    apply_rep_two_three_week(activity, committed, transcript),
                    committed,
                    transcript,
                ),
                committed,
                transcript,
            )
        })
        .collect()
}

fn text_field(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// The CRM's `extractActivitiesFromResponse`: the model's JSON to activities.
pub fn from_response(
    json: &Value,
    committed: DateTime<Utc>,
    transcript: &str,
) -> Vec<CallActivity> {
    let default = || {
        vec![CallActivity {
            name: DEFAULT_ACTIVITY_NAME.into(),
            deadline: None,
        }]
    };
    // zod: an object whose fields, when present, have the right types.
    let typed = |value: &Value, key: &str, nullable: bool| match value.get(key) {
        None | Some(Value::String(_)) => true,
        Some(Value::Null) => nullable,
        Some(_) => false,
    };
    let Some(object) = json.as_object() else {
        return default();
    };
    if !typed(json, "name", false) || !typed(json, "deadline", true) {
        return default();
    }
    let items: Vec<Value> = match object.get("activities") {
        None => Vec::new(),
        Some(Value::Array(items)) => {
            let valid = items.iter().all(|item| {
                item.is_object() && typed(item, "name", false) && typed(item, "deadline", true)
            });
            if !valid {
                return default();
            }
            items.clone()
        }
        Some(_) => return default(),
    };
    let items = if items.is_empty() {
        if text_field(&json["name"]).is_some() {
            vec![serde_json::json!({ "name": json["name"], "deadline": json["deadline"] })]
        } else {
            Vec::new()
        }
    } else {
        items
    };
    let activities: Vec<CallActivity> = items
        .iter()
        .filter_map(|item| {
            let name = canonical_name(&text_field(&item["name"])?);
            let raw = text_field(&item["deadline"]);
            let deadline = quote_minimum_deadline(
                deadline_rules(raw.as_deref(), &name, committed, transcript),
                &name,
                committed,
            );
            Some(CallActivity { name, deadline })
        })
        .collect();
    if activities.is_empty() {
        return default();
    }
    finalize(activities, committed, transcript)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentences_split_after_punctuation() {
        assert_eq!(
            sentences("Hi. How are you?  Fine!ok"),
            ["Hi.", "How are you?", "Fine!ok"]
        );
    }

    #[test]
    fn budgets_read_like_the_crm() {
        assert_eq!(
            extract_budget("we are trying to stay under $8,000 for quartz").as_deref(),
            Some("$8,000")
        );
        assert_eq!(
            extract_budget("budget of 12500.50").as_deref(),
            Some("$12,500.50")
        );
        assert_eq!(extract_budget("it costs $20"), None);
    }
}
