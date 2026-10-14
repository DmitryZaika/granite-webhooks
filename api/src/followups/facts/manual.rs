//! Manual work for the salesperson (a call, a quote to send) and whether
//! later messages show it was done (part of `smsFlowAiFollowupFacts.ts`).

use super::super::dates::{Instant, utc_date};
use super::super::js::{js_slice, trim, utf16_index};
use super::materials::{activity_name, title_case_material};
use super::quotes::text_looks_like_quote_sent;
use super::timing::month_index_of;
use super::types::{
    AreaReadiness, ContactMethod, FollowupFacts, ManualActionRequired, ManualActionStatus,
    ProjectAreaKind, ProjectAreaState,
};
use crate::re;
use chrono::{DateTime, Duration, Utc};
use regex::Regex;

fn call_task_re() -> &'static Regex {
    re!(
        r"\bcall (him|her|them|back|the customer)\b|\bphone call\b|\bgive (him|her|them) a call\b|\bcall to discuss\b",
        "i"
    )
}

fn call_logged_re() -> &'static Regex {
    re!(
        r"call type:|left (him|her|them )?a voicemail|left a voicemail|spoke (with|to) (him|her|them)|talked to (him|her|them) on the phone|called (him|her|them)( back)?\b",
        "i"
    )
}

fn sales_deliverable_task_re() -> &'static Regex {
    re!(
        r"\b(send (?:him|her|them )?(?:a |the )?(?:updated |revised )?quote|send updated .{0,30}quote|send (?:him|her|them )?(?:a |the )?(?:quote|revised quote|updated quote|pictures|photos|pricing|estimate|measurements)|should send (?:the )?(?:measurements|pictures|photos)|check (?:the\s*)?(?:quote|backsplash)|check material availability|confirm pricing|going to order|order black mist|check with (?:her|him|them))\b",
        "i"
    )
}

fn parse_sales_manual_tasks(activity_text: &str) -> Vec<String> {
    let mut tasks = Vec::new();
    let lower = activity_text.to_lowercase();
    if re!(r"check (?:the\s*)?backsplash", "i").is_match(&lower) {
        tasks.push("CHECK_BACKSPLASH");
    }
    if re!(r"check (?:the\s*)?quote", "i").is_match(&lower) {
        tasks.push("SEND_UPDATED_QUOTE");
    }
    if re!(
        r"send (?:him|her|them )?(?:a |the )?(?:revised quote|updated quote)",
        "i"
    )
    .is_match(&lower)
    {
        tasks.push("SEND_REVISED_QUOTE");
    } else if re!(
        r"send updated .{0,30}quote|send (?:him|her|them ).{0,12}quote",
        "i"
    )
    .is_match(&lower)
    {
        tasks.push("SEND_QUOTE");
    }
    if re!(r"send (?:him|her|them )?(?:the )?(?:pictures|photos)", "i").is_match(&lower) {
        tasks.push("SEND_PICTURES");
    }
    if re!(r"send (?:the )?(?:measurements|pictures|photos)", "i").is_match(&lower) {
        tasks.push("SEND_MEASUREMENTS");
    }
    if re!(r"check material availability", "i").is_match(&lower) {
        tasks.push("CHECK_MATERIAL_AVAILABILITY");
    }
    if re!(r"confirm pricing", "i").is_match(&lower) {
        tasks.push("CONFIRM_PRICING");
    }
    if re!(r"going to order black mist|order black mist", "i").is_match(&lower) {
        tasks.push("CONFIRM_OR_ORDER_BLACK_MIST");
    }
    tasks.into_iter().map(String::from).collect()
}

/// An open CRM task for the salesperson to deliver something.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManualAction {
    pub required: Option<ManualActionRequired>,
    pub status: ManualActionStatus,
    pub completed_at: Option<Instant>,
    pub description: Option<String>,
    pub tasks: Vec<String>,
}

pub fn extract_manual_action_status(profile: &str) -> ManualAction {
    let first_task = profile.split('\n').find(|line| {
        line.starts_with("Activity:")
            && !re!(r"\bcompleted\b").is_match(line)
            && sales_deliverable_task_re().is_match(line)
    });
    if let Some(task) = first_task {
        let description = activity_name(task);
        let tasks = parse_sales_manual_tasks(&description);
        return ManualAction {
            required: Some(ManualActionRequired::SalesTask),
            status: ManualActionStatus::Pending,
            completed_at: None,
            description: Some(description),
            tasks: if tasks.is_empty() {
                vec!["SALES_TASK".into()]
            } else {
                tasks
            },
        };
    }
    ManualAction {
        required: None,
        status: ManualActionStatus::Pending,
        completed_at: None,
        description: None,
        tasks: Vec::new(),
    }
}

/// A requested phone call stays PENDING (and blocks the automated sequence)
/// until a completed call task or a logged call shows it happened.
pub fn extract_manual_phone_action_status(profile: &str) -> (ManualActionStatus, Option<Instant>) {
    let lines: Vec<&str> = profile.split('\n').collect();
    let call_tasks: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|line| line.starts_with("Activity:") && call_task_re().is_match(line))
        .collect();
    let completed = |line: &&str| re!(r"\bcompleted\b").is_match(line);
    if call_tasks.iter().any(|line| !completed(line)) {
        return (ManualActionStatus::Pending, None);
    }
    let evidence = call_tasks
        .iter()
        .find(|line| completed(line))
        .or_else(|| lines.iter().find(|line| call_logged_re().is_match(line)));
    evidence.map_or((ManualActionStatus::Pending, None), |line| {
        (ManualActionStatus::Completed, profile_line_date(line))
    })
}

/// A message as the manual-action reconciliation reads it.
#[derive(Debug, Clone, Default)]
pub struct ManualActionTimelineMessage {
    pub at: String,
    pub inbound: bool,
    pub text: Option<String>,
    pub is_mirrored_duplicate: bool,
}

fn parse_timeline_at(at: &str) -> Option<Instant> {
    if at.contains('T') {
        return super::super::dates::parse_iso(at);
    }
    let iso = format!("{}:00Z", at.replacen(' ', "T", 1));
    DateTime::parse_from_rfc3339(&iso)
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

fn timeline_text_after(
    timeline: &[ManualActionTimelineMessage],
    after_at: Option<Instant>,
) -> String {
    timeline
        .iter()
        .filter(|message| {
            let at = parse_timeline_at(&message.at);
            !matches!((after_at, at), (Some(after), Some(at)) if at <= after)
                && !message.is_mirrored_duplicate
        })
        .map(|message| message.text.clone().unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n")
}

fn substantive_progress_after_phone_request(text: &str) -> bool {
    if trim(text).is_empty() {
        return false;
    }
    if text_looks_like_quote_sent(text) || call_logged_re().is_match(text) {
        return true;
    }
    if re!(
        r"\b(showroom|maple point|address|ste [a-z]|suite|drive|street|lane|road)\b",
        "i"
    )
    .is_match(text)
        && re!(r"\d{2,5}\s+[A-Za-z]", "i").is_match(text)
    {
        return true;
    }
    if re!(
        r"in-?home estimate|came (by|in)|visited (the )?showroom|traveled|on my way|met (with|you)|measurement|templat",
        "i"
    )
    .is_match(text)
    {
        return true;
    }
    text.split('\n')
        .filter(|line| super::super::js::js_len(trim(line)) > 40)
        .count()
        >= 2
}

fn manual_task_satisfied_in_text(task: &str, text: &str) -> bool {
    if trim(text).is_empty() {
        return false;
    }
    match task {
        "SEND_QUOTE" | "SEND_REVISED_QUOTE" | "SEND_UPDATED_QUOTE" => {
            (text_looks_like_quote_sent(text)
                || re!(r"\bquote (was )?(sent|emailed|attached)\b", "i").is_match(text))
                && !re!(r"\bDeal:.*· Got a Quote\b", "i").is_match(text)
        }
        "SEND_PICTURES" => {
            re!(r"\bsent (you )?(the )?(pictures|photos)\b", "i").is_match(text)
                || re!(r"\b(received|got) (the )?(pictures|photos)\b", "i").is_match(text)
                || re!(r"\b(thanks for|love) (the )?(pictures|photos)\b", "i").is_match(text)
        }
        "CHECK_MATERIAL_AVAILABILITY" => re!(
            r"\b(in stock|out of stock|available|not available|we have|don't have|do not have)\b",
            "i"
        )
        .is_match(text),
        "CHECK_BACKSPLASH" => {
            re!(r"\bbacksplash\b", "i").is_match(text)
                && re!(r"\b(yes|no|4.?inch|full height|tile|stone)\b", "i").is_match(text)
        }
        "CONFIRM_PRICING" => {
            re!(r"\b(pricing|price) (is|are|would be|comes in at)\b", "i").is_match(text)
        }
        _ => false,
    }
}

/// What [`reconcile_manual_action_against_timeline`] reads.
pub struct ManualActionReconciliation<'a> {
    pub required: Option<ManualActionRequired>,
    pub status: ManualActionStatus,
    pub tasks: &'a [String],
    pub profile: &'a str,
    pub timeline: &'a [ManualActionTimelineMessage],
    pub quote_sent: bool,
    pub customer_requested_contact_method: Option<ContactMethod>,
}

/// A pending manual action is checked against what happened afterwards
/// before it may block automation: a call that took place completes it, a
/// quote or a visit since then supersedes it.
pub fn reconcile_manual_action_against_timeline(
    params: &ManualActionReconciliation<'_>,
) -> ManualActionStatus {
    let Some(required) = params.required else {
        return params.status;
    };
    if params.status != ManualActionStatus::Pending {
        return params.status;
    }
    let phone_request_at = params
        .timeline
        .iter()
        .find(|message| {
            let text = message.text.as_deref().unwrap_or("");
            message.inbound
                && re!(r"\b(call|phone)\b", "i").is_match(text)
                && re!(r"\b(give me|please|can you|could you|want|need)\b", "i").is_match(text)
        })
        .and_then(|message| parse_timeline_at(&message.at));
    let after_text = timeline_text_after(params.timeline, phone_request_at);
    if required == ManualActionRequired::PhoneCall {
        if re!(
            r"\b(spoke (with|to)|talked to (him|her|them)|connected on the phone|call type: connected)\b",
            "i"
        )
        .is_match(&format!("{after_text}\n{}", params.profile))
        {
            return ManualActionStatus::Completed;
        }
        if params.quote_sent
            || text_looks_like_quote_sent(&after_text)
            || (!trim(&after_text).is_empty()
                && substantive_progress_after_phone_request(&after_text))
        {
            return ManualActionStatus::Superseded;
        }
        return ManualActionStatus::Pending;
    }
    let full_later_text = format!("{after_text}\n{}", params.profile);
    if params.quote_sent
        && params
            .tasks
            .iter()
            .any(|task| re!(r"SEND_.*QUOTE|QUOTE").is_match(task))
    {
        return ManualActionStatus::Completed;
    }
    if params
        .tasks
        .iter()
        .any(|task| manual_task_satisfied_in_text(task, &full_later_text))
    {
        return ManualActionStatus::Completed;
    }
    if re!(
        r"updated your quote but never sent|never sent it to you|will send (you )?(the |your )?(updated )?quote",
        "i"
    )
    .is_match(&full_later_text)
        && !text_looks_like_quote_sent(&full_later_text)
    {
        return ManualActionStatus::Pending;
    }
    params.status
}

fn normalize_task_label(value: &str) -> String {
    re!(r"\s+").replace_all(trim(value), " ").to_lowercase()
}

pub fn unfinished_step_matches_manual_action(
    unfinished_next_step: &str,
    manual_action_description: Option<&str>,
) -> bool {
    let Some(description) = manual_action_description.filter(|d| !d.is_empty()) else {
        return false;
    };
    let unfinished = normalize_task_label(unfinished_next_step);
    let manual = normalize_task_label(description);
    if unfinished.is_empty() || manual.is_empty() {
        return false;
    }
    unfinished.contains(&manual) || manual.contains(&unfinished)
}

/// Drops the unfinished next step once the matching manual action (or a
/// completed CRM activity) shows it was done.
pub fn clear_unfinished_step_when_manual_action_finished(
    facts: &mut FollowupFacts,
    manual_action_status: ManualActionStatus,
    manual_action_description: Option<&str>,
    profile: &str,
) {
    let Some(unfinished_step) = facts.unfinished_next_step.clone().filter(|s| !s.is_empty()) else {
        return;
    };
    let unfinished = normalize_task_label(&unfinished_step);
    let completed_activity = profile.split('\n').any(|line| {
        if !line.starts_with("Activity:") || !re!(r"\bcompleted\b").is_match(line) {
            return false;
        }
        let stripped = re!(r"^Activity:\s*", "i").replace(line, "");
        let activity_text = stripped.split(" ·").next().unwrap_or("");
        let activity = normalize_task_label(activity_text);
        !unfinished.is_empty() && activity.contains(&js_slice(&unfinished, 0, 12))
    });
    let manual_finished = matches!(
        manual_action_status,
        ManualActionStatus::Completed | ManualActionStatus::Superseded
    );
    if !manual_finished && !completed_activity {
        return;
    }
    if unfinished_step_matches_manual_action(&unfinished_step, manual_action_description)
        || completed_activity
    {
        facts.unfinished_next_step = None;
        facts.unfinished_next_step_kind = None;
        facts.historical_task = facts.historical_task || completed_activity || manual_finished;
    }
}

/// The rooms the project covers (bathroom, kitchen), each still waiting.
pub fn extract_project_areas(text: &str) -> Vec<ProjectAreaState> {
    let mut areas = Vec::new();
    if re!(r"\bbathroom\b", "i").is_match(text) {
        areas.push(ProjectAreaState {
            area: ProjectAreaKind::Bathroom,
            readiness: AreaReadiness::Waiting,
        });
    }
    if re!(r"\bkitchen\b", "i").is_match(text) {
        areas.push(ProjectAreaState {
            area: ProjectAreaKind::Kitchen,
            readiness: AreaReadiness::Waiting,
        });
    }
    areas
}

/// Trimmed items, without repeats that differ only in spacing or case.
pub fn dedupe_normalized_facts(items: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();
    for item in items {
        let normalized = normalize_task_label(item);
        if normalized.is_empty() || !seen.insert(normalized) {
            continue;
        }
        result.push(trim(item).to_string());
    }
    result
}

/// The `(2026-03-01 10:00` or `created 2026-03-01 10:00` time of a profile line, in UTC.
pub(crate) fn profile_line_date(line: &str) -> Option<Instant> {
    let caps = re!(r"\((\d{4}-\d{2}-\d{2})[ T](\d{2}:\d{2})")
        .captures(line)
        .or_else(|| re!(r"created (\d{4}-\d{2}-\d{2})[ T](\d{2}:\d{2})").captures(line))?;
    DateTime::parse_from_rfc3339(&format!("{}T{}:00Z", &caps[1], &caps[2]))
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

/// A referenced date older than this is history, not a current next step.
const STALE_TASK_MARGIN_DAYS: i64 = 30;

fn referenced_dates_in_text(text: &str) -> Vec<Instant> {
    let number = |caps: &regex::Captures<'_>, i: usize| {
        caps.get(i)
            .and_then(|m| m.as_str().parse::<i32>().ok())
            .unwrap_or(0)
    };
    let mut dates = Vec::new();
    for caps in re!(r"\b(\d{1,2})\/(\d{1,2})\/(\d{4})\b").captures_iter(text) {
        dates.push(utc_date(
            number(&caps, 3),
            number(&caps, 1) - 1,
            number(&caps, 2),
        ));
    }
    for caps in re!(r"\b(\d{4})-(\d{2})-(\d{2})\b").captures_iter(text) {
        dates.push(utc_date(
            number(&caps, 1),
            number(&caps, 2) - 1,
            number(&caps, 3),
        ));
    }
    for caps in re!(
        r"\b(january|february|march|april|may|june|july|august|september|october|november|december)\s+(\d{1,2}),?\s+(\d{4})\b",
        "gi"
    )
    .captures_iter(text)
    {
        if let Some(month) = month_index_of(&caps[1].to_lowercase()) {
            dates.push(utc_date(number(&caps, 3), month, number(&caps, 2)));
        }
    }
    dates
}

/// A CRM task about a dated event that already passed is history, not the
/// current next step.
pub fn task_references_past_event(text: &str, now: Instant) -> bool {
    if trim(text).is_empty()
        || re!(
            r"(discuss|talk about|go over|review|explain).{0,24}(past|previous|old|expired)",
            "i"
        )
        .is_match(text)
    {
        return false;
    }
    referenced_dates_in_text(text)
        .into_iter()
        .max()
        .is_some_and(|newest| newest + Duration::days(STALE_TASK_MARGIN_DAYS) < now)
}

/// A stone the salesperson said is available as an alternative.
pub fn extract_alternative_material_offered(text: &str) -> Option<String> {
    for caps in re!(
        r"\b(bianco absoluto|frost white|calacatta|carrara|absolute black|white ice|black mist|taj mahal|izaro|unique)\b",
        "gi"
    )
    .captures_iter(text)
    {
        let raw = caps.get(1)?;
        let start = utf16_index(text, raw.start());
        let raw_len = super::super::js::js_len(raw.as_str());
        let around = js_slice(text, start, start + raw_len + 64).to_lowercase();
        let name_len = super::super::js::js_len(&raw.as_str().to_lowercase());
        let after_name = js_slice(&around, name_len, usize::MAX);
        if re!(r"unavailable|not available|out of stock").is_match(&after_name) {
            continue;
        }
        if re!(r"\bavailable\b").is_match(&after_name) {
            return Some(title_case_material(raw.as_str()));
        }
    }
    None
}
