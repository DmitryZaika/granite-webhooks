//! The company's AI follow-up instruction: the send gap and first-message
//! rules it states, and its prompt block (`aiFollowupAdminInstruction.ts`).

use super::js::trim;
use crate::re;

/// The reserved instructions-table title, hidden from the instruction lists.
pub const AI_FOLLOWUP_ADMIN_INSTRUCTION_TITLE: &str = "AI follow-up instruction";
pub const AI_FOLLOWUP_ADMIN_INSTRUCTION_MAX: usize = 2000;

pub fn is_ai_followup_admin_instruction_title(title: Option<&str>) -> bool {
    title == Some(AI_FOLLOWUP_ADMIN_INSTRUCTION_TITLE)
}

fn day_count(raw: &str) -> Option<i64> {
    let token = trim(raw).to_lowercase();
    if re!(r"^\d+$").is_match(&token) {
        return token
            .parse::<f64>()
            .ok()
            .filter(|v| *v > 0.0 && *v <= 90.0)
            .map(|v| {
                #[allow(clippy::cast_possible_truncation)]
                let days = v as i64;
                days
            });
    }
    [
        "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
    ]
    .iter()
    .position(|word| *word == token)
    .and_then(|index| i64::try_from(index + 1).ok())
}

/// Minutes between follow-ups when the instruction states a send gap.
pub fn admin_instruction_gap_minutes(instruction: &str) -> Option<i64> {
    let text = trim(instruction).to_lowercase();
    if text.is_empty() {
        return None;
    }
    if re!(r"\b(?:every day|each day|daily)\b").is_match(&text) {
        return Some(24 * 60);
    }
    let patterns = [
        re!(r"(\d+|one|two|three|four|five|six|seven|eight|nine|ten)\s*-?\s*days?\s+gaps?"),
        re!(r"gaps?\s+of\s+(\d+|one|two|three|four|five|six|seven|eight|nine|ten)\s+days?"),
        re!(r"every\s+(\d+|one|two|three|four|five|six|seven|eight|nine|ten)\s+days?"),
        re!(r"(\d+|one|two|three|four|five|six|seven|eight|nine|ten)\s+days?\s+apart"),
        re!(r"(\d+|one|two|three|four|five|six|seven|eight|nine|ten)\s+days?\s+between"),
    ];
    patterns.iter().find_map(|pattern| {
        let caps = pattern.captures(&text)?;
        day_count(caps.get(1)?.as_str()).map(|days| days * 24 * 60)
    })
}

/// The instruction says the first follow-up goes out today.
pub fn admin_instruction_sends_first_message_today(instruction: &str) -> bool {
    let text = trim(instruction).to_lowercase();
    !text.is_empty()
        && (re!(r"\b(?:1|one)\s+messages?\s+today\b").is_match(&text)
            || re!(r"\bfirst\s+messages?\s+today\b").is_match(&text))
}

/// The company rule block for the follow-up model (empty when none is saved).
pub fn ai_followup_admin_instruction_prompt_block(instruction: &str) -> String {
    let text = trim(instruction);
    if text.is_empty() {
        return String::new();
    }
    let first_today = admin_instruction_sends_first_message_today(text);
    let gap_line = match (admin_instruction_gap_minutes(text), first_today) {
        (None, false) => String::new(),
        (None, true) => "\nThe first message delayMinutes must be 0 so it sends today. Later messages keep the usual cadence.".into(),
        (Some(gap), true) => format!(
            "\nThis rule sets the send gap. The first message delayMinutes must be 0 so it sends today. Every later message delayMinutes must be {gap}. It overrides the default week or month cadence and the Sunday rule."
        ),
        (Some(gap), false) => format!(
            "\nThis rule sets the send gap. delayMinutes must be {gap} on every message, including Sunday. It overrides the default week or month cadence."
        ),
    };
    format!(
        "ADMINISTRATIVE INSTRUCTION — this user instruction has higher priority than the default cadence, the Sunday rule, and any wait you would otherwise choose. Follow it in every follow-up. Do not let it override an opt-out, and do not invent facts it does not state.{gap_line}\n{text}"
    )
}
