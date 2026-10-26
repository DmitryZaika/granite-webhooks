//! Tidying an AI call note the way the CRM does before it saves one: a port
//! of `sanitizeCallNoteContent` (`app/lib/callAiHelpers.ts` in the CRM),
//! rule for rule, with its patterns as they are written there.
//!
//! [`js`] compiles those JavaScript patterns with their JavaScript meaning:
//! `\b`, `\w` and `\d` are ASCII, `\s` is JavaScript's white space and `.`
//! anything but a line break. Lengths and cuts count UTF-16 units and
//! [`js_trim`] trims what `String.prototype.trim` trims, as in JavaScript.
//! What is left: a case-insensitive `k` or `s` also matches the Kelvin sign
//! and the long s, a cut that would split a character keeps it whole, and a
//! greeting-only voicemail is `voicemail::greeting_only`.
//!
//! In the CRM's order: Customer and Next steps discussed bullets go, and
//! bullets saying nothing ("not specified", "N/A" ...); for a call,
//! misplaced Estimate, Site visit and Project timing bullets are moved or
//! dropped and sink negations ("not in the island") dropped; with a
//! transcript, callbacks and missed appointments the transcript puts on the
//! salesperson are corrected, and for a call the Location bullets merged
//! with an address read from the transcript, granite named in a tear-out it
//! confirms and a Budget bullet added from it; then a call loses its Call
//! type bullets and gets bold labels, and a voicemail keeps one Action line
//! (prior timing folded in) under a "- Voicemail" heading, or is "No answer"
//! when the transcript is only a greeting.

use crate::voicemail;
use regex::{NoExpand, Regex};
use std::collections::HashSet;
use std::sync::LazyLock;

/// The first line of a voicemail note.
const VOICEMAIL_HEADING_LINE: &str = "- Voicemail";
/// The note of a voicemail whose transcript is only the mailbox greeting.
pub const VOICEMAIL_NO_ANSWER_NOTE: &str = "- Voicemail\n- Action: No answer";

/// JavaScript's `\s` (its white space and line terminators), as the inside
/// of a character class.
const JS_SPACE: &str = r"\t\n\x0B\x0C\r \x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}";

/// A JavaScript regular expression (no `u` flag) as a Rust one with the same
/// meaning: `\b` an ASCII word boundary, `\w` and `\d` ASCII, `\s`
/// JavaScript's white space, `.` anything but a line break. `flags` is the
/// JavaScript flags: `i` matters (`g` is how the caller iterates).
fn js(pattern: &str, flags: &str) -> Result<Regex, regex::Error> {
    let mut rust = String::with_capacity(pattern.len() * 2);
    if flags.contains('i') {
        rust.push_str("(?i)");
    }
    let mut in_class = false;
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match (chars.next(), in_class) {
                (Some('b'), false) => rust.push_str(r"(?-u:\b)"),
                (Some('w'), false) => rust.push_str("[0-9A-Za-z_]"),
                (Some('w'), true) => rust.push_str("0-9A-Za-z_"),
                (Some('d'), false) => rust.push_str("[0-9]"),
                (Some('d'), true) => rust.push_str("0-9"),
                (Some('s'), false) => {
                    rust.push('[');
                    rust.push_str(JS_SPACE);
                    rust.push(']');
                }
                (Some('s'), true) => rust.push_str(JS_SPACE),
                (Some('/'), _) => rust.push('/'),
                (Some(escaped), _) => {
                    rust.push('\\');
                    rust.push(escaped);
                }
                (None, _) => rust.push_str(r"\\"),
            },
            '[' if !in_class => {
                in_class = true;
                rust.push('[');
            }
            ']' if in_class => {
                in_class = false;
                rust.push(']');
            }
            '.' if !in_class => rust.push_str(r"[^\n\r\x{2028}\x{2029}]"),
            _ => rust.push(c),
        }
    }
    Regex::new(&rust)
}

/// One of the CRM's patterns, compiled once where it is used:
/// `regex!(r"...")`, or `regex!(r"...", "i")` with the `i` flag. (The tests
/// compile every one of them.)
macro_rules! regex {
    ($pattern:literal) => {
        regex!($pattern, "")
    };
    ($pattern:literal, $flags:literal) => {{
        static PATTERN: LazyLock<Regex> =
            LazyLock::new(|| js($pattern, $flags).expect("the CRM's call note pattern compiles"));
        &*PATTERN
    }};
}

/// JavaScript's white space and line terminators.
const fn is_js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

/// `text.trim()`.
fn js_trim(text: &str) -> &str {
    text.trim_matches(is_js_space)
}

/// `text.length`: UTF-16 units.
fn js_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// The byte offset of UTF-16 unit `units` of `text` (the end when past it;
/// inside a character, that character's end).
fn byte_at(text: &str, units: usize) -> usize {
    let mut seen = 0;
    for (at, c) in text.char_indices() {
        if seen >= units {
            return at;
        }
        seen += c.len_utf16();
    }
    text.len()
}

/// `text.slice(start, end)`, in UTF-16 units.
fn js_slice(text: &str, start: usize, end: usize) -> &str {
    let start = byte_at(text, start);
    &text[start..byte_at(text, end).max(start)]
}

/// A count as a score.
fn score_of(count: usize) -> i64 {
    i64::try_from(count).unwrap_or(i64::MAX / 100)
}

/// `line.trim().match(BULLET_PARTS_PATTERN)`: a "- Label: value" line's
/// label and value, as matched.
fn bullet_parts(line: &str) -> Option<(&str, &str)> {
    let caps = regex!(r"^\s*-\s*([^:]+):\s*(.*)$").captures(js_trim(line))?;
    Some((caps.get(1)?.as_str(), caps.get(2)?.as_str()))
}

/// A bullet line's label, trimmed and lower case.
fn label_of(line: &str) -> Option<String> {
    bullet_parts(line).map(|(label, _)| js_trim(label).to_lowercase())
}

/// The lines of `note`, each made over by `change`, joined again.
fn map_lines(note: &str, change: impl FnMut(&str) -> String) -> String {
    note.split('\n').map(change).collect::<Vec<_>>().join("\n")
}

/// `stripNoteLines`: the lines whose trimmed text matches `pattern` go.
fn strip_note_lines(content: &str, pattern: &Regex) -> String {
    let kept: Vec<&str> = content
        .split('\n')
        .filter(|line| !pattern.is_match(js_trim(line)))
        .collect();
    js_trim(&kept.join("\n")).to_string()
}

/// `isUnknownOrEmptyBulletValue`: a bullet value that says nothing.
fn is_unknown_or_empty_bullet_value(value: &str) -> bool {
    let trimmed = js_trim(value);
    trimmed.is_empty()
        || regex!(
            r"^(?:not\s+specified|unknown|n\/a|na|none|not\s+discussed|no\s+details(?:\s+discussed)?|unspecified|—|-|\.)$",
            "i"
        )
        .is_match(trimmed)
        || regex!(
            r"\b(?:not\s+(?:specified|discussed|mentioned)|no\s+specific\b|nothing\s+(?:specific\s+)?(?:mentioned|discussed|stated)|no\s+preference(?:\s+stated)?)\b",
            "i"
        )
        .is_match(trimmed)
}

/// `stripUnknownNoteBullets`.
fn strip_unknown_note_bullets(note: &str) -> String {
    let kept: Vec<&str> = note
        .split('\n')
        .filter(|line| {
            bullet_parts(line).is_none_or(|(_, value)| !is_unknown_or_empty_bullet_value(value))
        })
        .collect();
    js_trim(&kept.join("\n")).to_string()
}

/// `transcriptShowsSalespersonMissedAppointment`.
fn transcript_shows_salesperson_missed_appointment(transcript: &str) -> bool {
    let lower = transcript.to_lowercase();
    regex!(r"\b(?:i|we)\s+(?:somehow\s+)?missed\b").is_match(&lower)
        || regex!(r"\bmy\s+mistake\b").is_match(&lower)
        || regex!(r"\bno\s+way\s+i\s+missed\b").is_match(&lower)
        || (regex!(r"\b(?:i'?m|i am)\s+sorry\b").is_match(&lower)
            && regex!(r"\bmissed\b").is_match(&lower))
}

/// `estimateBulletIsOnSiteScheduling`.
fn estimate_bullet_is_on_site_scheduling(value: &str) -> bool {
    let lower = value.to_lowercase();
    regex!(
        r"\b(?:on[- ]?site|be here|stop by|come out|measurement visit|will be there|see you at)\b"
    )
    .is_match(&lower)
        || (regex!(r"\b(?:appointment|missed|reschedul)\b").is_match(&lower)
            && regex!(r"\b(?:\d{1,2}(?::\d{2})?\s*(?:am|pm)|at\s+\d|today|tomorrow)\b")
                .is_match(&lower))
}

/// `estimateBulletIsPhoneCallbackScheduling`.
fn estimate_bullet_is_phone_callback_scheduling(value: &str) -> bool {
    let lower = value.to_lowercase();
    if estimate_bullet_is_on_site_scheduling(&lower) {
        return false;
    }
    regex!(r"\b(?:call\s+me|call\s+back|callback|follow[- ]?up)\b").is_match(&lower)
        || regex!(r"\bcustomer\s+will\s+call\b").is_match(&lower)
        || (regex!(r"\b(?:quote|pricing)\b").is_match(&lower)
            && regex!(r"\b(?:discuss|questions?|follow|check)\b").is_match(&lower)
            && regex!(
                r"\b(?:friday|monday|tuesday|wednesday|thursday|saturday|sunday|next|weekend)\b"
            )
            .is_match(&lower))
        || (regex!(r"\b(?:home depot|lowe)\b").is_match(&lower)
            && regex!(r"\b(?:quote|compare|shopping|questions?)\b").is_match(&lower))
}

/// `transcriptShowsCustomerAskedRepToCall`.
fn transcript_shows_customer_asked_rep_to_call(transcript: &str) -> bool {
    let lower = transcript.to_lowercase();
    regex!(r"\b(?:you\s+can\s+call\s+me|call\s+me)\b").is_match(&lower)
        || (regex!(r"\b(?:would you like\s+(?:me\s+)?to\s+)?call\s+you\b").is_match(&lower)
            && regex!(r"\b(?:yes|okay|good|will do|next\s+(?:monday|tuesday|wednesday|thursday|friday|saturday|sunday)|friday)\b")
                .is_match(&lower))
}

/// `customerWillCallIsRepCallbackMisattribution`.
fn customer_will_call_is_rep_callback_misattribution(value: &str) -> bool {
    if regex!(r"\bwhen\s+ready\b", "i").is_match(value) {
        return false;
    }
    if regex!(r"\bcontact(?:s)?\s+(?:the\s+)?(?:company|us)\s+when\b", "i").is_match(value) {
        return false;
    }
    regex!(r"\bcustomer\s+will\s+call\b", "i").is_match(value)
}

/// `normalizeEstimateNoteBullets`.
fn normalize_estimate_note_bullets(note: &str) -> String {
    let kept: Vec<&str> = note
        .split('\n')
        .filter(|line| match bullet_parts(line) {
            Some((label, value)) if js_trim(label).to_lowercase() == "estimate" => {
                !estimate_bullet_is_phone_callback_scheduling(js_trim(value))
            }
            _ => true,
        })
        .collect();
    map_lines(&kept.join("\n"), |line| {
        let Some((label, value)) = bullet_parts(line) else {
            return line.to_string();
        };
        let label = js_trim(label).to_lowercase();
        let value = js_trim(value);
        if label == "site visit" {
            return format!("- Estimate: {value}");
        }
        if label != "project timing"
            || estimate_bullet_is_phone_callback_scheduling(value)
            || !regex!(
                r"\b(?:appointment|estimate|measurement|missed|reschedul|site\s+visit)\b",
                "i"
            )
            .is_match(value)
        {
            return line.to_string();
        }
        format!("- Estimate: {value}")
    })
}

/// `correctRepCallBackAttribution`.
fn correct_rep_call_back_attribution(note: &str, transcript: &str) -> String {
    if !transcript_shows_customer_asked_rep_to_call(transcript) {
        return note.to_string();
    }
    map_lines(note, |line| {
        let Some((label, value)) = bullet_parts(line) else {
            return line.to_string();
        };
        let value = js_trim(value);
        if !customer_will_call_is_rep_callback_misattribution(value) {
            return line.to_string();
        }
        let value = regex!(r"\bcustomer\s+will\s+call\b", "i")
            .replace_all(value, NoExpand("salesperson will call customer"));
        format!("- {}: {value}", js_trim(label))
    })
}

/// `correctMissedAppointmentAttribution`.
fn correct_missed_appointment_attribution(note: &str, transcript: &str) -> String {
    if !transcript_shows_salesperson_missed_appointment(transcript) {
        return note.to_string();
    }
    map_lines(note, |line| {
        let Some((label, value)) = bullet_parts(line) else {
            return line.to_string();
        };
        let value = js_trim(value);
        if !regex!(r"\bmissed\b", "i").is_match(value)
            && !regex!(r"\bappointment\b", "i").is_match(value)
        {
            return line.to_string();
        }
        let mut value = regex!(r"\bcustomer\s+missed\b", "i")
            .replace_all(value, NoExpand("salesperson missed"))
            .into_owned();
        if regex!(r"\bmissed\b", "i").is_match(&value)
            && !regex!(r"\b(?:salesperson|sales\s+rep|rep)\s+missed\b", "i").is_match(&value)
            && regex!(
                r"\b(?:appointment|10:?\d{2}|estimate|measurement|saturday|sunday|monday|tuesday|wednesday|thursday|friday)\b",
                "i"
            )
            .is_match(&value)
        {
            value = regex!(r"\bmissed\b", "i")
                .replace(&value, NoExpand("salesperson missed"))
                .into_owned();
        }
        format!("- {}: {value}", js_trim(label))
    })
}

/// `LEGACY_VOICEMAIL_HEADING_PATTERN`: an old "Call type: Voicemail" heading.
fn is_legacy_voicemail_heading(trimmed: &str) -> bool {
    regex!(
        r"^\s*-?\s*Call type:\s*Voicemail(?:\.\s*No answer)?\s*$",
        "i"
    )
    .is_match(trimmed)
}

/// `VOICEMAIL_HEADING_LINE_PATTERN`: a "Voicemail" heading.
fn is_voicemail_heading(trimmed: &str) -> bool {
    regex!(r"^\s*-?\s*Voicemail\s*$", "i").is_match(trimmed)
}

/// `replaceLegacyVoicemailHeadingLines`.
fn replace_legacy_voicemail_heading_lines(note: &str) -> String {
    map_lines(note, |line| {
        if is_legacy_voicemail_heading(js_trim(line)) {
            VOICEMAIL_HEADING_LINE.to_string()
        } else {
            line.to_string()
        }
    })
}

/// `stripVoicemailHeadingLines`.
fn strip_voicemail_heading_lines(note: &str) -> String {
    let kept: Vec<&str> = note
        .split('\n')
        .filter(|line| {
            let trimmed = js_trim(line);
            !is_voicemail_heading(trimmed) && !is_legacy_voicemail_heading(trimmed)
        })
        .collect();
    js_trim(&kept.join("\n")).to_string()
}

/// `withVoicemailCallType(content, true)`: the note under one "- Voicemail"
/// heading.
fn with_voicemail_call_type(content: &str) -> String {
    let body =
        strip_voicemail_heading_lines(&replace_legacy_voicemail_heading_lines(js_trim(content)));
    if body.is_empty() {
        return VOICEMAIL_HEADING_LINE.to_string();
    }
    format!("{VOICEMAIL_HEADING_LINE}\n{body}")
}

/// `normalizeVoicemailNoteLine`.
fn normalize_voicemail_note_line(line: &str) -> String {
    let trimmed = js_trim(line);
    if trimmed.is_empty() {
        return String::new();
    }
    let contacted = regex!(r"^\s*-\s*Contacted\b[^:]*:\s*", "i");
    if contacted.is_match(trimmed) {
        return contacted
            .replace(trimmed, NoExpand("- Action: "))
            .into_owned();
    }
    let goal = regex!(r"^\s*-\s*Goal of the call:\s*", "i");
    if goal.is_match(trimmed) {
        return goal.replace(trimmed, NoExpand("- Action: ")).into_owned();
    }
    trimmed.to_string()
}

/// `isVoicemailBulletLine`.
fn is_voicemail_bullet_line(line: &str) -> bool {
    let trimmed = js_trim(line);
    !trimmed.is_empty()
        && (is_voicemail_heading(trimmed)
            || is_legacy_voicemail_heading(trimmed)
            || bullet_parts(trimmed).is_some())
}

/// `stripUnstructuredVoicemailLines`.
fn strip_unstructured_voicemail_lines(note: &str) -> String {
    let kept: Vec<&str> = note
        .split('\n')
        .filter(|line| {
            let trimmed = js_trim(line);
            if trimmed.is_empty() {
                return false;
            }
            if is_voicemail_bullet_line(trimmed) {
                return true;
            }
            if regex!(
                r"\b(?:please\s+leave(?:\s+(?:your|a))?\s+message|this\s+is\s+\w+\s+with\b|get\s+back\s+to\s+you|thank\s+you\.?$|just\s+let\s+me\s+know)\b",
                "i"
            )
            .is_match(trimmed)
            {
                return false;
            }
            if js_len(trimmed) > 72 && !trimmed.starts_with('-') {
                return false;
            }
            trimmed.starts_with('-')
        })
        .collect();
    js_trim(&kept.join("\n")).to_string()
}

/// `compactPriorTimingPhrase`.
fn compact_prior_timing_phrase(value: &str) -> String {
    let text = regex!(r"^previously:\s*", "i")
        .replace(js_trim(value), NoExpand(""))
        .into_owned();
    if regex!(r"\bmay\b", "i").is_match(&text)
        && regex!(r"mention|potential|timing|ready|project", "i").is_match(&text)
    {
        return "prior note: May".to_string();
    }
    if js_len(&text) > 35 {
        return format!("prior: {}…", js_trim(js_slice(&text, 0, 32)));
    }
    if text.starts_with("prior") {
        text
    } else {
        format!("prior: {text}")
    }
}

/// `combineVoicemailBullets`.
fn combine_voicemail_bullets(lines: &[String]) -> Vec<String> {
    let mut action: Option<String> = None;
    let mut prior_timing: Option<String> = None;
    let mut next_steps: Option<String> = None;
    for line in lines {
        let Some((label, value)) = bullet_parts(line) else {
            continue;
        };
        let label = js_trim(label).to_lowercase();
        let value = js_trim(value).to_string();
        if label == "call type" || label == "voicemail" {
            continue;
        }
        if label == "action" {
            action = Some(value);
        } else if label.starts_with("previously") || label == "project timing" {
            prior_timing = Some(value);
        } else if label == "next steps discussed" {
            next_steps = Some(value);
        }
    }
    let action = action.unwrap_or_default();
    let prior_timing = prior_timing.unwrap_or_default();
    let mut merged = Vec::new();
    if !action.is_empty() || !prior_timing.is_empty() {
        let mut action_text = action;
        if !prior_timing.is_empty() && !regex!(r"\bprior\b", "i").is_match(&action_text) {
            let prior = compact_prior_timing_phrase(&prior_timing);
            action_text = if action_text.is_empty() {
                prior
            } else {
                format!("{action_text}; {prior}")
            };
        }
        merged.push(format!("- Action: {action_text}"));
    }
    if let Some(next_steps) = next_steps.filter(|steps| !steps.is_empty()) {
        let step = regex!(r"\s+in proceeding\.?$", "i").replace(&next_steps, NoExpand(""));
        let step = regex!(r"^confirm if still interested", "i")
            .replace(js_trim(&step), NoExpand("confirm still interested"))
            .into_owned();
        merged.push(format!("- Next steps discussed: {step}"));
    }
    merged
}

/// `normalizeVoicemailNote`: one line each (by its words, any case), then
/// the bullets a voicemail keeps.
fn normalize_voicemail_note(note: &str) -> String {
    let mut seen = HashSet::new();
    let mut deduped = Vec::new();
    for line in note
        .split('\n')
        .map(normalize_voicemail_note_line)
        .filter(|line| !line.is_empty())
    {
        let key = regex!(r"\s+")
            .replace_all(&line, NoExpand(" "))
            .to_lowercase();
        if seen.insert(key) {
            deduped.push(line);
        }
    }
    combine_voicemail_bullets(&deduped).join("\n")
}

/// `Number.parseFloat` of an amount with its commas dropped; `None` for NaN.
fn parse_amount(raw: &str) -> Option<f64> {
    raw.replace(',', "")
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())
}

/// `budgetAmountIsPlausible`: $500 to $500,000.
fn budget_amount_is_plausible(raw: &str) -> bool {
    parse_amount(raw).is_some_and(|value| (500.0..=500_000.0).contains(&value))
}

/// `toLocaleString('en-US')`'s grouping of whole digits: 8000 → 8,000.
fn group_thousands(digits: &str) -> String {
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (at, digit) in digits.chars().enumerate() {
        if at > 0 && (digits.len() - at).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

/// `formatBudgetAmount`: `$8,000`, or `$8,000.50` with cents.
fn format_budget_amount(raw: &str) -> String {
    let Some(value) = parse_amount(raw) else {
        return format!("${raw}");
    };
    // `Number.isInteger`, an exact test.
    #[allow(clippy::float_cmp)]
    let whole = value.trunc() == value;
    if whole {
        return format!("${}", group_thousands(&format!("{value:.0}")));
    }
    let fixed = format!("{value:.2}");
    let (int, cents) = fixed.split_once('.').unwrap_or((&fixed, "00"));
    format!("${}.{cents}", group_thousands(int))
}

/// `extractBudgetFromTranscript`.
fn extract_budget_from_transcript(transcript: &str) -> Option<String> {
    let context = regex!(
        r"\b(?:below|under|around|about|right about|stay(?:ing)?(?:\s+below|\s+under)?|target(?:ing)?|budget(?:\s+of)?|try to stay under)\s*\$?\s*([\d,]+(?:\.\d{2})?)\b",
        "i"
    );
    if let Some(raw) = context.captures(transcript).and_then(|caps| caps.get(1))
        && budget_amount_is_plausible(raw.as_str())
    {
        return Some(format_budget_amount(raw.as_str()));
    }
    for caps in regex!(r"\$\s*([\d,]+(?:\.\d{2})?)\b").captures_iter(transcript) {
        let (Some(dollars), Some(raw)) = (caps.get(0), caps.get(1)) else {
            continue;
        };
        if !budget_amount_is_plausible(raw.as_str()) {
            continue;
        }
        let at = js_len(&transcript[..dollars.start()]);
        let around = js_slice(
            transcript,
            at.saturating_sub(90),
            at + js_len(dollars.as_str()) + 40,
        )
        .to_lowercase();
        if regex!(r"\b(?:budget|target|below|under|about|around|stay|league|installation|quartz|granite|estimate|quote|trying to)\b")
            .is_match(&around)
        {
            return Some(format_budget_amount(raw.as_str()));
        }
    }
    None
}

/// The first of `candidates` with the highest score: what the CRM's stable
/// sort by score, highest first, puts first.
fn first_best(candidates: &[String], score: impl Fn(&str) -> i64) -> Option<&String> {
    let mut best: Option<(&String, i64)> = None;
    for candidate in candidates {
        let points = score(candidate);
        if best.is_none_or(|(_, top)| points > top) {
            best = Some((candidate, points));
        }
    }
    best.map(|(candidate, _)| candidate)
}

/// The non-empty, trimmed parts of `value` between commas.
fn comma_parts(value: &str) -> Vec<&str> {
    value
        .split(',')
        .map(js_trim)
        .filter(|part| !part.is_empty())
        .collect()
}

/// `addressCandidateScore`.
fn address_candidate_score(address: &str) -> i64 {
    let parts = comma_parts(address);
    let mut score = score_of(parts.len()) * 10;
    if regex!(r"\b\d{1,6}\s+\w").is_match(parts.first().copied().unwrap_or_default()) {
        score += 5;
    }
    if parts.len() >= 3 {
        score += 10;
    }
    score
}

/// `appendStreetDirectionFromTranscript`.
fn append_street_direction_from_transcript(street: &str, transcript: &str) -> String {
    let Some(number) = regex!(r"^\d{1,6}").find(street) else {
        return street.to_string();
    };
    // Built from digits and a direction word: they always compile.
    let Ok(pattern) = js(
        &format!(
            r"\b{}\s+[\w\s'-]+Court\s+(East|West|North|South)\b",
            number.as_str()
        ),
        "i",
    ) else {
        return street.to_string();
    };
    let Some(direction) = pattern.captures(transcript).and_then(|caps| caps.get(1)) else {
        return street.to_string();
    };
    let direction = direction.as_str();
    if js(&format!(r"\b{direction}\b"), "i").is_ok_and(|named| named.is_match(street)) {
        return street.to_string();
    }
    format!("{street} {direction}")
}

/// `buildStreetAddressCandidate` (never given a direction by the CRM).
fn build_street_address_candidate(street: &str, city: &str, state: Option<&str>) -> Option<String> {
    let spaces = regex!(r"\s+");
    let street = spaces.replace_all(js_trim(street), NoExpand(" "));
    let city = spaces.replace_all(js_trim(city), NoExpand(" "));
    if street.is_empty() || city.is_empty() {
        return None;
    }
    let state = state.map(js_trim).filter(|state| !state.is_empty());
    Some(state.map_or_else(
        || format!("{street}, {city}"),
        |state| {
            format!(
                "{street}, {city}, {}",
                spaces.replace_all(state, NoExpand(" "))
            )
        },
    ))
}

/// `extractFullAddressFromTranscript`: a "... Court" street address the
/// customer said.
fn extract_full_address_from_transcript(transcript: &str) -> Option<String> {
    let mut candidates = Vec::new();
    let full = regex!(
        r"\b(\d{1,6}\s+(?:[A-Za-z][\w\s'-]*\s+){0,4}Court(?:\s+(?:East|West|North|South))?)\s*,?\s*([A-Za-z][\w\s'-]*?)(?:,\s*([A-Za-z][\w\s'-]{2,20}))?\b",
        "gi"
    );
    for caps in full.captures_iter(transcript) {
        let (Some(street), Some(city)) = (caps.get(1), caps.get(2)) else {
            continue;
        };
        let state = caps.get(3).map(|state| state.as_str());
        if let Some(candidate) =
            build_street_address_candidate(street.as_str(), city.as_str(), state)
        {
            candidates.push(candidate);
        }
    }
    let court_for_city = regex!(
        r"\b(\d{1,6})\s+[\w\s,-]{2,60}?\s+Court\s+for\s+([A-Za-z][\w\s'-]*?),\s*([A-Za-z][\w\s'-]{2,20})\b",
        "gi"
    );
    for caps in court_for_city.captures_iter(transcript) {
        let (Some(number), Some(city), Some(state)) = (caps.get(1), caps.get(2), caps.get(3))
        else {
            continue;
        };
        let number = js_trim(number.as_str());
        // Built from digits: it always compiles.
        let street_pattern = js(
            &format!(r"{number}\s+([A-Za-z][\w\s'-]+?)\s*,?\s*(?:[A-Z]-){{0,20}}\s*Court"),
            "i",
        );
        let street_name = street_pattern
            .ok()
            .and_then(|pattern| pattern.captures(transcript).and_then(|caps| caps.get(1)))
            .map(|name| {
                js_trim(&regex!(r"[,\s-]+").replace_all(name.as_str(), NoExpand(" "))).to_string()
            })
            .unwrap_or_default();
        let street = if street_name.is_empty() {
            format!("{number} Court")
        } else {
            format!("{number} {street_name} Court")
        };
        let street = append_street_direction_from_transcript(&street, transcript);
        if let Some(candidate) =
            build_street_address_candidate(&street, city.as_str(), Some(state.as_str()))
        {
            candidates.push(candidate);
        }
    }
    first_best(&candidates, address_candidate_score).cloned()
}

/// `locationValueScore`.
fn location_value_score(value: &str) -> i64 {
    let parts = comma_parts(value);
    let first = parts.first().copied().unwrap_or_default();
    let mut score = score_of(parts.len()) * 10;
    if regex!(r"\b\d{1,6}\s+\w").is_match(first) {
        score += 5;
    }
    if parts.len() >= 3 {
        score += 10;
    }
    if regex!(r"\bcourt\s+(?:east|west|north|south)\b", "i").is_match(first) {
        score += 5;
    }
    if regex!(r"court,\s*(?:east|west|north|south)\b", "i").is_match(value) {
        score -= 8;
    }
    score
}

/// `consolidateLocationBullets`: one Location bullet, the fullest address
/// of the note's and the transcript's, where the first one was.
fn consolidate_location_bullets(note: &str, transcript: &str) -> String {
    let lines: Vec<&str> = note.split('\n').collect();
    let is_location = |line: &&str| label_of(line).as_deref() == Some("location");
    let mut values: Vec<String> = Vec::new();
    let mut first_location = None;
    for (at, line) in lines.iter().enumerate() {
        if let Some((label, value)) = bullet_parts(line)
            && js_trim(label).to_lowercase() == "location"
        {
            values.push(js_trim(value).to_string());
            first_location.get_or_insert(at);
        }
    }
    let mut candidates = values.clone();
    candidates.extend(extract_full_address_from_transcript(transcript));
    let Some(best) = first_best(&candidates, location_value_score) else {
        return note.to_string();
    };
    if best.is_empty() || (values.len() == 1 && values[0] == *best) {
        return note.to_string();
    }
    let mut kept: Vec<String> = lines
        .iter()
        .filter(|line| !is_location(line))
        .map(|line| (*line).to_string())
        .collect();
    let insert_at = first_location.map_or(kept.len(), |first| {
        lines[..first]
            .iter()
            .filter(|line| !is_location(line))
            .count()
    });
    kept.insert(insert_at, format!("- Location: {best}"));
    kept.join("\n")
}

/// `transcriptConfirmsGraniteTearOut`.
fn transcript_confirms_granite_tear_out(transcript: &str) -> bool {
    let lower = transcript.to_lowercase();
    regex!(r"\bgranite\b").is_match(&lower)
        && regex!(r"\b(?:countertop|counter top|backsplash|remove|tear|demolish|replace)\b")
            .is_match(&lower)
}

/// `enhanceTearOutMaterial`.
fn enhance_tear_out_material(note: &str, transcript: &str) -> String {
    if !transcript_confirms_granite_tear_out(transcript) {
        return note.to_string();
    }
    map_lines(note, |line| {
        let Some((label, value)) = bullet_parts(line) else {
            return line.to_string();
        };
        if js_trim(label).to_lowercase() != "tear-out" {
            return line.to_string();
        }
        let value = js_trim(value);
        if regex!(r"\bgranite\b", "i").is_match(value) {
            return line.to_string();
        }
        let existing = regex!(r"\bexisting countertops\b", "i");
        let countertops = regex!(r"\bcountertops\b", "i");
        let removes = regex!(r"\bremoves existing\b", "i");
        let value = if existing.is_match(value) {
            existing
                .replace(value, NoExpand("existing granite countertops"))
                .into_owned()
        } else if countertops.is_match(value) {
            countertops
                .replace(value, NoExpand("granite countertops"))
                .into_owned()
        } else if removes.is_match(value) {
            removes
                .replace(value, NoExpand("removes existing granite countertops"))
                .into_owned()
        } else {
            format!("granite tear-out, {value}")
        };
        format!("- Tear-out: {value}")
    })
}

/// `injectBudgetBullet`.
fn inject_budget_bullet(note: &str, transcript: &str) -> String {
    if note
        .split('\n')
        .any(|line| label_of(line).as_deref() == Some("budget"))
    {
        return note.to_string();
    }
    let Some(budget) = extract_budget_from_transcript(transcript) else {
        return note.to_string();
    };
    js_trim(&format!(
        "{note}\n- Budget: {budget} for quartz and installation"
    ))
    .to_string()
}

/// `stripSinkNegations`.
fn strip_sink_negations(note: &str) -> String {
    map_lines(note, |line| {
        let Some((label, value)) = bullet_parts(line) else {
            return line.to_string();
        };
        if js_trim(label).to_lowercase() != "sink" {
            return line.to_string();
        }
        let value = regex!(r"\s*\(\s*not\s+(?:in\s+)?(?:the\s+)?island\s*\)", "i")
            .replace_all(js_trim(value), NoExpand(""));
        let value = regex!(r"\s*,?\s*not\s+(?:in\s+)?(?:the\s+)?island\b", "i")
            .replace_all(&value, NoExpand(""));
        let value = regex!(
            r"\s*,?\s*located\s+in\s+(?:the\s+)?l-?shaped\s+section\s*\(\s*not\s+island\s*\)",
            "i"
        )
        .replace_all(&value, NoExpand(", in L-shaped section"));
        let value = regex!(r"\s{2,}").replace_all(&value, NoExpand(" "));
        let value = regex!(r"\s+,").replace_all(&value, NoExpand(","));
        format!("- Sink: {}", js_trim(&value))
    })
}

/// `estimateBulletIsLeadTime`.
fn estimate_bullet_is_lead_time(value: &str) -> bool {
    let lower = value.to_lowercase();
    regex!(r"\b\d+\s*(?:to|-)\s*\d+\s*weeks?\b").is_match(&lower)
        || regex!(r"\blead\s*time\b").is_match(&lower)
        || regex!(r"\bwaiting\s+(?:on|for)\b").is_match(&lower)
}

/// A clock time, today, tomorrow or a weekday in lower-case `text` (the
/// CRM's scheduled-time pattern).
fn has_scheduled_time(text: &str) -> bool {
    regex!(r"\b(?:\d{1,2}(?::\d{2})?\s*(?:am|pm)|today|tomorrow|monday|tuesday|wednesday|thursday|friday|saturday|sunday)\b")
        .is_match(text)
}

/// `estimateBulletIsUnscheduledOnSiteRequest`.
fn estimate_bullet_is_unscheduled_on_site_request(value: &str) -> bool {
    let lower = value.to_lowercase();
    if has_scheduled_time(&lower) {
        return false;
    }
    regex!(r"\b(?:would\s+appreciate|wants?|request(?:ed|ing)?|like)\b").is_match(&lower)
        && regex!(r"\b(?:on[- ]?site|come\s+over|someone\s+coming)\b").is_match(&lower)
}

/// `transcriptShowsRepAskedCustomerToCall`.
fn transcript_shows_rep_asked_customer_to_call(transcript: &str) -> bool {
    let lower = transcript.to_lowercase();
    regex!(r"\bgive\s+me\s+a\s+call\b").is_match(&lower)
        || regex!(r"\b(?:you\s+)?call\s+(?:me\s+)?in\s+advance\b").is_match(&lower)
        || (regex!(r"\bcall\b").is_match(&lower)
            && regex!(r"\b(?:advance|a\s+week\s+before|week\s+before)\b").is_match(&lower))
}

/// `estimateBulletIsRepProcessOrSchedulingAdvice`.
fn estimate_bullet_is_rep_process_or_scheduling_advice(value: &str) -> bool {
    let lower = value.to_lowercase();
    let scheduled = has_scheduled_time(&lower);
    if estimate_bullet_is_on_site_scheduling(&lower) && scheduled {
        return false;
    }
    regex!(r"\b(?:salesperson|sales\s+rep|rep)\s+will\s+call\b").is_match(&lower)
        || regex!(r"\bcustomer\s+advised\b").is_match(&lower)
        || regex!(r"\badvised\s+to\s+call\b").is_match(&lower)
        || regex!(r"\bcall\s+(?:in\s+)?advance\b").is_match(&lower)
        || regex!(r"\b(?:one|a)\s+week\s+before\b").is_match(&lower)
        || regex!(r"\bschedule\s+(?:your\s+)?template\b").is_match(&lower)
        || regex!(r"\btemplate\s+(?:on\s+)?(?:the\s+)?next\s+day\b").is_match(&lower)
        || regex!(r"\bmeasurement\s+person\b").is_match(&lower)
        || regex!(r"\binstall(?:ed)?\s+in\s+(?:two|three|2|3)\s+weeks?\b").is_match(&lower)
        || regex!(r"\bsave\s+you\s+another\s+week\b").is_match(&lower)
        || (regex!(r"\b(?:schedule|template|measurement|appointment)\b").is_match(&lower)
            && !scheduled)
}

/// `extractCustomerTimelineFromEstimate`.
fn extract_customer_timeline_from_estimate(value: &str) -> Option<String> {
    let lower = value.to_lowercase();
    let mut parts = Vec::new();
    if regex!(r"\b(?:cabinets?|cabinetry)\b").is_match(&lower)
        && regex!(r"\bnext\s+month\b").is_match(&lower)
    {
        parts.push("cabinets expected done next month");
    }
    if regex!(r"\bon\s+hold\b").is_match(&lower) {
        parts.push("project was on hold, now resuming");
    }
    if regex!(r"\bexpired\s+quote\b").is_match(&lower)
        || regex!(r"\bquote\s+expired\b").is_match(&lower)
    {
        parts.push("needs updated quote");
    }
    (!parts.is_empty()).then(|| parts.join("; "))
}

/// `appendProjectTimingValue`.
fn append_project_timing_value(current: Option<String>, value: &str) -> String {
    match current {
        Some(current) if !current.is_empty() => {
            if current.to_lowercase().contains(&value.to_lowercase()) {
                current
            } else {
                format!("{current}; {value}")
            }
        }
        _ => value.to_string(),
    }
}

/// `normalizeMisplacedEstimateBullets`: Estimate bullets that are lead
/// times, wishes, the salesperson's process or the customer's timeline go
/// (timelines and lead times into a Project timing bullet when there is
/// none).
fn normalize_misplaced_estimate_bullets(note: &str, transcript: Option<&str>) -> String {
    let mut project_timing: Option<String> = None;
    let lines: Vec<&str> = note
        .split('\n')
        .filter(|line| {
            let Some((label, value)) = bullet_parts(line) else {
                return true;
            };
            if js_trim(label).to_lowercase() != "estimate" {
                return true;
            }
            let value = js_trim(value);
            let timeline = extract_customer_timeline_from_estimate(value);
            if let Some(timeline) = &timeline {
                project_timing = Some(append_project_timing_value(project_timing.take(), timeline));
            }
            if estimate_bullet_is_lead_time(value) {
                project_timing = Some(append_project_timing_value(project_timing.take(), value));
                return false;
            }
            if estimate_bullet_is_unscheduled_on_site_request(value)
                || estimate_bullet_is_rep_process_or_scheduling_advice(value)
            {
                return false;
            }
            if transcript.is_some_and(transcript_shows_rep_asked_customer_to_call)
                && regex!(r"\b(?:salesperson|sales\s+rep|rep)\s+will\s+call\b", "i").is_match(value)
            {
                return false;
            }
            timeline.is_none() || estimate_bullet_is_on_site_scheduling(&value.to_lowercase())
        })
        .collect();
    let Some(project_timing) = project_timing.filter(|timing| !timing.is_empty()) else {
        return lines.join("\n");
    };
    if lines
        .iter()
        .any(|line| label_of(line).as_deref() == Some("project timing"))
    {
        return lines.join("\n");
    }
    let mut lines: Vec<String> = lines.into_iter().map(str::to_string).collect();
    lines.push(format!("- Project timing: {project_timing}"));
    lines.join("\n")
}

/// `boldCallNoteBulletLabels`: "- Label: text" becomes "- **Label**: text".
fn bold_call_note_bullet_labels(note: &str) -> String {
    map_lines(note, |line| {
        if regex!(r"^\s*-\s*\*\*.+?\*\*:").is_match(line) {
            return line.to_string();
        }
        let Some(caps) = regex!(r"^(\s*-\s*)([^:*\n]{1,48}?)(:)(\s*)(.*)$").captures(line) else {
            return line.to_string();
        };
        let label = js_trim(&caps[2]);
        if label.is_empty() || regex!(r"^voicemail$", "i").is_match(label) {
            return line.to_string();
        }
        format!(
            "{}**{label}**{}{}{}",
            &caps[1], &caps[3], &caps[4], &caps[5]
        )
    })
}

/// The CRM's `sanitizeCallNoteContent`: `content`, the AI's note of a call
/// (`is_voicemail`: of a voicemail), tidied with the call's `transcript`
/// when there is one. Empty when nothing is left to note.
pub fn sanitize(content: &str, is_voicemail: bool, transcript: Option<&str>) -> String {
    let transcript = transcript.filter(|text| !text.is_empty());
    if is_voicemail && transcript.is_some_and(voicemail::greeting_only) {
        return VOICEMAIL_NO_ANSWER_NOTE.to_string();
    }
    let mut note = strip_note_lines(js_trim(content), regex!(r"^\s*-?\s*Customer\s*:", "i"));
    note = strip_note_lines(&note, regex!(r"^\s*-?\s*Next steps discussed\s*:", "i"));
    note = strip_unknown_note_bullets(&note);
    if !is_voicemail {
        note = normalize_estimate_note_bullets(&note);
        note = normalize_misplaced_estimate_bullets(&note, transcript);
        note = strip_sink_negations(&note);
    }
    if let Some(transcript) = transcript {
        note = correct_rep_call_back_attribution(&note, transcript);
        note = correct_missed_appointment_attribution(&note, transcript);
        if !is_voicemail {
            note = consolidate_location_bullets(&note, transcript);
            note = enhance_tear_out_material(&note, transcript);
            note = inject_budget_bullet(&note, transcript);
        }
    }
    if !is_voicemail {
        note = strip_note_lines(&note, regex!(r"^\s*-?\s*Call type\s*:", "i"));
        return bold_call_note_bullet_labels(&note);
    }
    note = strip_unstructured_voicemail_lines(&note);
    note = normalize_voicemail_note(&note);
    bold_call_note_bullet_labels(&with_voicemail_call_type(&note))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (the AI's note, a voicemail, the transcript, the tidied note). Every
    /// expected note is what the CRM's own `sanitizeCallNoteContent` returned
    /// for the same input (run with Bun on `app/lib/callAiHelpers.ts`).
    const CASES: &[(&str, bool, Option<&str>, &str)] = &[
        (
            "- Customer: Jane Doe\n- Project: kitchen remodel, white quartz, about 45 sq ft\n- Color: not specified\n- Sink: N/A\n- Backsplash: none\n- Edge profile: no specific profile mentioned\n- Next steps discussed: send the quote\n- Call type: Outbound sales call\nRandom unstructured line\n- **Pricing:** per square foot\n- **Sample**: showroom",
            false,
            None,
            "- **Project**: kitchen remodel, white quartz, about 45 sq ft\nRandom unstructured line\n- **Pricing:** per square foot\n- **Sample**: showroom",
        ),
        (
            "- Estimate: call me Friday to discuss the quote\n- Site visit: Tuesday at 3 pm to measure\n- Project timing: missed Saturday 10:30 appointment; rescheduled for today at 5:30 pm\n- Project timing: waiting on cabinets",
            false,
            None,
            "- **Estimate**: Tuesday at 3 pm to measure\n- **Estimate**: missed Saturday 10:30 appointment; rescheduled for today at 5:30 pm\n- **Project timing**: waiting on cabinets",
        ),
        (
            "- Project: kitchen\n- Estimate: 6 to 8 weeks lead time on the slabs\n- Estimate: customer would like someone coming on-site\n- Estimate: salesperson will call a week before to schedule template\n- Estimate: cabinets done next month, project was on hold\n- Estimate: Saturday at 10 am on-site measurement\n- Estimate: quote expired, needs refresh",
            false,
            None,
            "- **Project**: kitchen\n- **Estimate**: Saturday at 10 am on-site measurement\n- **Project timing**: 6 to 8 weeks lead time on the slabs; cabinets expected done next month; project was on hold, now resuming; needs updated quote",
        ),
        (
            "- Sink: double, white (not in the island)\n- Sink: single,  not island , undermount\n- Sink: undermount, located in the L-shaped section (not island)\n- Sink: farmhouse, not in the island",
            false,
            None,
            "- **Sink**: double, white\n- **Sink**: single, undermount\n- **Sink**: undermount, located in the L-shaped section\n- **Sink**: farmhouse",
        ),
        (
            "- Project timing: customer will call next week to confirm\n- Project timing: customer will call when ready\n- Estimate: customer missed Saturday 10:30 appointment; rescheduled for today at 5:30 pm\n- Color: customer will call about colors",
            false,
            Some(
                "Speaker A: I'm so sorry, I missed our Saturday appointment, my mistake.\nSpeaker B: That's okay. You can call me next Friday.",
            ),
            "- **Project timing**: salesperson will call customer next week to confirm\n- **Project timing**: customer will call when ready\n- **Estimate**: salesperson missed Saturday 10:30 appointment; rescheduled for today at 5:30 pm\n- **Color**: salesperson will call customer about colors",
        ),
        (
            "- Project: kitchen, granite tear-out and quartz install\n- Location: Carmel\n- Tear-out: company removes existing countertops\n- Location: Carmel, IN",
            false,
            Some(
                "Speaker B: We're at 1234 Maple Leaf Court East, Carmel, Indiana. We have granite countertops now and want you to remove them. We're trying to stay under $8,000 for the whole thing.",
            ),
            "- **Project**: kitchen, granite tear-out and quartz install\n- **Location**: 1234 Maple Leaf Court East, Carmel, Indiana\n- **Tear-out**: company removes existing granite countertops\n- **Budget**: $8,000 for quartz and installation",
        ),
        (
            "- Project: bathroom vanity\n- Tear-out: customer removes them\n- Location: Fishers",
            false,
            Some(
                "It is 4521 Willow Run Court for Fishers, Indiana. The old top is granite and we will remove it ourselves. Budget of 12000 maybe.",
            ),
            "- **Project**: bathroom vanity\n- **Tear-out**: granite tear-out, customer removes them\n- **Location**: 4521 Willow Run Court, Fishers, Indiana\n- **Budget**: $12,000 for quartz and installation",
        ),
        (
            "- Project: island top",
            false,
            Some(
                "Speaker A: The last quote we got was $4,500.50 from another shop, and $50 for the sink.\nSpeaker B: ok",
            ),
            "- **Project**: island top\n- **Budget**: $4,500.50 for quartz and installation",
        ),
        (
            "- Project: island top\n- Budget: around $6,000",
            false,
            Some("We want to stay under $9,000 for this."),
            "- **Project**: island top\n- **Budget**: around $6,000",
        ),
        (
            "- Project: island top",
            false,
            Some("It was about 300 dollars, maybe $250, and the stone is nice."),
            "- **Project**: island top",
        ),
        (
            "- Project: kitchen",
            false,
            Some("We live at 77 Oak Hill Court, Zionsville."),
            "- **Project**: kitchen\n- **Location**: 77 Oak Hill Court, Zionsville",
        ),
        (
            "- Estimate: salesperson will call Monday and come out to measure\n- Project: vanity",
            false,
            Some("Speaker A: Give me a call next week."),
            "- **Project**: vanity",
        ),
        (
            "- Estimate: salesperson will call Monday and come out to measure\n- Project: vanity",
            false,
            Some("Speaker A: Talk to you soon."),
            "- **Estimate**: salesperson will call Monday and come out to measure\n- **Project**: vanity",
        ),
        (
            "- Estimate: missed the Saturday appointment, rescheduled to Monday at 2 pm",
            false,
            Some("Speaker A: No way I missed that, I'm sorry."),
            "- **Estimate**: salesperson missed the Saturday appointment, rescheduled to Monday at 2 pm",
        ),
        (
            "- Tear-out: removes existing tops\n- Tear-out: existing countertops removed by company\n- tear-out: granite already\n- Tear-out: countertops by customer",
            false,
            Some("The granite has to be removed first."),
            "- **Tear-out**: removes existing tops\n- **Tear-out**: existing countertops removed by company\n- **tear-out**: granite already\n- **Tear-out**: countertops by customer",
        ),
        (
            "- Tear-out: removes existing tops\n- Tear-out: existing countertops removed by company\n- tear-out: granite already\n- Tear-out: countertops by customer\n- Tear-out: company does it",
            false,
            Some("The granite has to come out. Can you remove it?"),
            "- **Tear-out**: removes existing granite countertops tops\n- **Tear-out**: existing granite countertops removed by company\n- **tear-out**: granite already\n- **Tear-out**: granite countertops by customer\n- **Tear-out**: granite tear-out, company does it",
        ),
        (
            "- Estimate: follow-up after they compare the Home Depot quote\n- Estimate: will check pricing questions next Monday\n- Project: kitchen",
            false,
            None,
            "- **Project**: kitchen",
        ),
        (
            "- Location: 9 Pine Court, Carmel, IN\n- Project: kitchen",
            false,
            Some("We talked about the kitchen."),
            "- **Location**: 9 Pine Court, Carmel, IN\n- **Project**: kitchen",
        ),
        (
            "- Project: kitchen\n- Location: 55 Elm Court West, Noblesville, Indiana",
            false,
            Some("It is 55 Elm Court, West Noblesville."),
            "- **Project**: kitchen\n- **Location**: 55 Elm Court West, Noblesville, Indiana",
        ),
        (
            "- Contacted (voicemail): called about the vanity quote\n- Next steps: none",
            true,
            None,
            "- Voicemail\n- **Action**: called about the vanity quote",
        ),
        (
            "- Call type: Voicemail. No answer\n- Contacted: left a voicemail about the countertop quote\n- Previously: customer mentioned the project might be ready in May\n- Next steps discussed: confirm if still interested in proceeding.\nThis is Rita with Acme, just following up on your quote, thank you\n- this is Rita with Acme calling back\n- just a dash line\nA long unstructured line of transcript prose that goes on and on and on beyond seventy two characters",
            true,
            Some(
                "Hi, this is Rita with Acme Stone following up on your countertop quote. Please give me a call back. Thank you.",
            ),
            "- Voicemail\n- **Action**: left a voicemail about the countertop quote; prior note: May",
        ),
        (
            "- Goal of the call: follow up on the estimate\n- Project timing: they said the kitchen would not be ready until after the holidays this year\n- goal of the call:   follow up on the estimate",
            true,
            None,
            "- Voicemail\n- **Action**: follow up on the estimate; prior: they said the kitchen would not…",
        ),
        (
            "- Action: Follow-up on inquiry ~5 months ago; prior note: ready in May\n- Previously: ready in May",
            true,
            None,
            "- Voicemail\n- **Action**: Follow-up on inquiry ~5 months ago; prior note: ready in May",
        ),
        (
            "- Voicemail\n- Call type: Voicemail\n- Action: Called about the quote",
            true,
            None,
            "- Voicemail\n- **Action**: Called about the quote",
        ),
        (
            "- Action: Left a message",
            true,
            Some(
                "The person you are trying to reach is not available. Please leave a message after the tone.",
            ),
            "- Voicemail\n- Action: No answer",
        ),
        (
            "- **Project:** kitchen, white quartz\n- **Sink:** double",
            true,
            Some("Hi, this is Vera. Please call me back about the quote."),
            "- Voicemail",
        ),
        (
            "- **Project:** kitchen, white quartz\n- **Sink:** double",
            false,
            Some(
                "Speaker A: Hi, this is Rita from Acme.\nSpeaker B: Hi Rita, we want white quartz for the kitchen.",
            ),
            "- **Project:** kitchen, white quartz\n- **Sink:** double",
        ),
        (
            "- Action: Rep called about the quote\n- Previously: ready in May",
            true,
            Some("Hi, this is Rita from Acme about your quote, call me back."),
            "- Voicemail\n- **Action**: Rep called about the quote; prior note: May",
        ),
        (
            "\u{feff}- Project:\u{a0}kitchen in the Café\u{a0}\n- Location: 12 Rue Ébène, Montréal\n-  Edge profile :  ogee\n- Sink: double\r\n- Label that is way too long to be bolded because it has more than forty eight chars: x\n- : empty label\n- Color:\u{2028}white",
            false,
            Some("Speaker A: José here — we’re trying to stay under $9,500 for quartz."),
            "- **Project**:\u{a0}kitchen in the Café\u{a0}\n- **Location**: 12 Rue Ébène, Montréal\n-  **Edge profile**:  ogee\n- **Sink**: double\n- Label that is way too long to be bolded because it has more than forty eight chars: x\n- : empty label\n- **Color**:\u{2028}white\n- **Budget**: $9,500 for quartz and installation",
        ),
        (
            "- Action: Called back\n- Previously: 😀😀 the customer said it would be after the holidays, maybe later",
            true,
            None,
            "- Voicemail\n- **Action**: Called back; prior: 😀😀 the customer said it would…",
        ),
        ("   ", false, None, ""),
        ("- Customer: Jane", false, None, ""),
        ("", true, None, "- Voicemail"),
        ("- Call type: Inbound", false, Some("Speaker A: Hello."), ""),
    ];

    #[test]
    fn notes_are_tidied_as_the_crm_tidies_them() {
        for (n, (content, voicemail, transcript, expected)) in CASES.iter().enumerate() {
            assert_eq!(
                sanitize(content, *voicemail, *transcript),
                *expected,
                "case {n}: {content:?}"
            );
        }
    }

    /// A JavaScript pattern, compiled.
    fn re(pattern: &str, flags: &str) -> Regex {
        js(pattern, flags).unwrap()
    }

    #[test]
    fn every_pattern_compiles() {
        // Each `regex!` in this file, read from the source: a pattern on a
        // branch no case reaches still compiles.
        let site = Regex::new(r#"regex!\(\s*r"([^"]*)"(?:,\s*"([a-z]*)")?"#).unwrap();
        let mut compiled = 0;
        for caps in site.captures_iter(include_str!("call_notes.rs")) {
            let flags = caps.get(2).map_or("", |flags| flags.as_str());
            assert!(js(&caps[1], flags).is_ok(), "{}", &caps[1]);
            compiled += 1;
        }
        assert!(compiled > 90, "{compiled} patterns");
    }

    #[test]
    fn a_voicemail_keeps_its_next_steps_short() {
        // Not reached through `sanitize` (it drops Next steps discussed lines
        // first), but kept as the CRM has it.
        let lines =
            ["- Next steps discussed: confirm if still interested in proceeding.".to_string()];
        assert_eq!(
            combine_voicemail_bullets(&lines),
            ["- Next steps discussed: confirm still interested"]
        );
    }

    #[test]
    fn patterns_keep_their_javascript_meaning() {
        // \b and \w are ASCII: "é" is no word character.
        assert!(re(r"\bcaf\b", "").is_match("café"));
        assert!(!re(r"^\w+$", "").is_match("José"));
        // \d is ASCII, \s is JavaScript's white space (no U+0085), `.` stops at
        // any line break.
        assert!(!re(r"^\d$", "").is_match("\u{663}"));
        for space in ["\u{a0}", "\u{feff}", "\u{2028}", "\u{3000}", "\t"] {
            assert!(re(r"^\s$", "").is_match(space), "{space:?}");
        }
        assert!(!re(r"^\s$", "").is_match("\u{85}"));
        assert!(!re(r"^a.b$", "").is_match("a\rb"));
        assert!(!re(r"^a.b$", "").is_match("a\u{2028}b"));
        assert!(re(r"^a.b$", "").is_match("a b"));
        // Inside classes too, and escapes JavaScript allows.
        assert!(re(r"^[\w\s'-]+$", "").is_match("O'Neil Court-\u{a0}2"));
        assert!(!re(r"^[\w\s'-]+$", "").is_match("Zoë"));
        assert!(re(r"^n\/a$", "i").is_match("N/A"));
        assert!(re(r"^\$\s*[\d,]+$", "").is_match("$ 8,000"));
    }

    #[test]
    fn strings_are_measured_and_trimmed_as_in_javascript() {
        assert_eq!(js_trim("\u{feff} x \u{a0}\u{2028}"), "x");
        assert_eq!(js_trim("\u{85}x"), "\u{85}x");
        assert_eq!(js_len("😀a"), 3);
        assert_eq!(js_slice("a😀b", 0, 3), "a😀");
        assert_eq!(js_slice("a😀b", 1, 4), "😀b");
        assert_eq!(js_slice("abc", 2, 10), "c");
        // A cut inside a character keeps it whole (JavaScript would split it).
        assert_eq!(js_slice("a😀b", 0, 2), "a😀");
    }

    #[test]
    fn budgets_are_written_like_to_locale_string() {
        assert_eq!(format_budget_amount("8000"), "$8,000");
        assert_eq!(format_budget_amount("8,000.00"), "$8,000");
        assert_eq!(format_budget_amount("4,500.50"), "$4,500.50");
        assert_eq!(format_budget_amount("500000"), "$500,000");
        assert_eq!(format_budget_amount("0500"), "$500");
        assert_eq!(format_budget_amount("1234.5"), "$1,234.50");
        assert_eq!(format_budget_amount(",,"), "$,,");
        assert!(budget_amount_is_plausible("500") && budget_amount_is_plausible("500,000"));
        assert!(!budget_amount_is_plausible("499.99") && !budget_amount_is_plausible("500,000.01"));
        assert!(!budget_amount_is_plausible(","));
    }

    #[test]
    fn ties_go_to_the_first_candidate() {
        let candidates = [
            "a, b".to_string(),
            "c, d".to_string(),
            "1 Main St, e, f".to_string(),
        ];
        assert_eq!(
            first_best(&candidates[..2], location_value_score),
            Some(&candidates[0])
        );
        assert_eq!(
            first_best(&candidates, location_value_score),
            Some(&candidates[2])
        );
        assert_eq!(first_best(&[], location_value_score), None);
    }
}
