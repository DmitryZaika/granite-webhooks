//! How a salesperson writes, learned from what they change in AI drafts
//! (port of `smsFlowAiFollowupStyle.ts`).

use super::cadence::types::AiCadencePreviewMessage;
use super::js::{escape_reg_exp, js_len, js_regex_try, trim};
use super::send_window::FollowupChannel;
use crate::{re, str_enum};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;

/// An AI draft and what the rep sent instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FollowupEditPairDraft {
    pub channel: FollowupChannel,
    pub generated_text: String,
    pub final_text: String,
    pub angle: Option<String>,
    pub phase: Option<String>,
    pub customer_first_name: Option<String>,
}

str_enum! { pub enum FollowupStyleLength { Short = "SHORT", Medium = "MEDIUM", Long = "LONG" } }
str_enum! { pub enum FollowupQuestionPreference { None = "NONE", OneMax = "ONE_MAX" } }
str_enum! { pub enum FollowupTogglePreference { Add = "ADD", Remove = "REMOVE" } }
str_enum! { pub enum ExclamationPreference { None = "NONE" } }

/// The rep's learned writing preferences.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FollowupStyleProfile {
    pub sms_length: Option<FollowupStyleLength>,
    pub email_length: Option<FollowupStyleLength>,
    pub question_count: Option<FollowupQuestionPreference>,
    pub exclamation: Option<ExclamationPreference>,
    pub customer_name: Option<FollowupTogglePreference>,
    pub greeting: Option<FollowupTogglePreference>,
    pub link: Option<FollowupTogglePreference>,
    /// Short notes on how this rep writes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<Vec<String>>,
}

/// Counts of a text.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FollowupTextStats {
    pub character_count: usize,
    pub word_count: usize,
    pub sentence_count: usize,
    pub question_count: usize,
    pub exclamation_count: usize,
    pub paragraph_count: usize,
}

/// What changed between the draft and the sent text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(clippy::struct_excessive_bools)]
pub struct FollowupEditComparison {
    pub original: FollowupTextStats,
    #[serde(rename = "final")]
    pub final_stats: FollowupTextStats,
    pub customer_name_removed: bool,
    pub customer_name_added: bool,
    pub greeting_removed: bool,
    pub greeting_added: bool,
    pub link_removed: bool,
    pub link_added: bool,
}

/// One learned sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FollowupStyleSample {
    pub channel: FollowupChannel,
    pub comparison: FollowupEditComparison,
}

const MIN_SAMPLES: usize = 3;
const MIN_RATIO: f64 = 2.0 / 3.0;
const SMS_SHORT_CHARS: usize = 130;
const SMS_MEDIUM_CHARS: usize = 200;
const EMAIL_SHORT_CHARS: usize = 280;
const EMAIL_MEDIUM_CHARS: usize = 550;

fn greeting_re() -> &'static regex::Regex {
    re!(
        r"^(hi|hey|hello|good morning|good afternoon|good evening|hi there)\b",
        "i"
    )
}

pub fn empty_followup_style_profile() -> FollowupStyleProfile {
    FollowupStyleProfile::default()
}

/// Drafts the rep changed before sending.
pub fn collect_edited_followup_pairs(
    preview: &[AiCadencePreviewMessage],
    customer_first_name: Option<&str>,
) -> Vec<FollowupEditPairDraft> {
    preview
        .iter()
        .filter(|message| !message.skip)
        .filter_map(|message| {
            let generated_body = message.generated_body.as_deref()?;
            let email = message.channel == FollowupChannel::Email;
            let generated_text = compose_followup_pair_text(
                if email {
                    message.generated_subject.clone().flatten()
                } else {
                    None
                }
                .as_deref(),
                generated_body,
            );
            let final_text = compose_followup_pair_text(
                if email {
                    message.subject.as_deref()
                } else {
                    None
                },
                &message.body,
            );
            if trim(&generated_text).is_empty() || trim(&generated_text) == trim(&final_text) {
                return None;
            }
            Some(FollowupEditPairDraft {
                channel: message.channel,
                generated_text,
                final_text,
                angle: message.angle.clone(),
                phase: message.phase.clone(),
                customer_first_name: customer_first_name.map(str::to_string),
            })
        })
        .collect()
}

pub fn compose_followup_pair_text(subject: Option<&str>, body: &str) -> String {
    let subject = trim(subject.unwrap_or(""));
    let body = trim(body);
    if subject.is_empty() {
        body.to_string()
    } else {
        format!("{subject}\n\n{body}")
    }
}

/// The first word of the profile's `Name:` (two letters or more).
pub fn first_name_from_customer_profile(profile: &str) -> Option<String> {
    let caps = re!(r"^Name:\s*(.+)$", "m").captures(profile)?;
    let full = trim(&caps[1]);
    let first = re!(r"\s+").split(full).next()?;
    (js_len(first) >= 2).then(|| first.to_string())
}

pub fn followup_text_stats(text: &str) -> FollowupTextStats {
    let trimmed = trim(text);
    let count_parts = |pattern: &regex::Regex| {
        if trimmed.is_empty() {
            0
        } else {
            pattern
                .split(trimmed)
                .map(trim)
                .filter(|p| !p.is_empty())
                .count()
        }
    };
    FollowupTextStats {
        character_count: js_len(trimmed),
        word_count: if trimmed.is_empty() {
            0
        } else {
            re!(r"\s+").split(trimmed).filter(|w| !w.is_empty()).count()
        },
        sentence_count: count_parts(re!(r"[.!?]+")),
        question_count: trimmed.matches('?').count(),
        exclamation_count: trimmed.matches('!').count(),
        paragraph_count: count_parts(re!(r"\n\s*\n")),
    }
}

fn has_customer_name(text: &str, first_name: Option<&str>) -> bool {
    first_name
        .filter(|n| !n.is_empty())
        .and_then(|name| js_regex_try(&format!(r"\b{}\b", escape_reg_exp(name)), "i"))
        .is_some_and(|re| re.is_match(text))
}

fn has_greeting(text: &str) -> bool {
    let first_line = trim(trim(text).split('\n').next().unwrap_or(""));
    greeting_re().is_match(first_line)
        || greeting_re().is_match(&re!(r"^[\p{L}'-]+,\s*", "u").replace(first_line, ""))
}

fn has_link(text: &str) -> bool {
    re!(r"https?:\/\/|www\.", "i").is_match(text)
}

pub fn compare_followup_edit(
    generated_text: &str,
    final_text: &str,
    first_name: Option<&str>,
) -> FollowupEditComparison {
    let (name_before, name_after) = (
        has_customer_name(generated_text, first_name),
        has_customer_name(final_text, first_name),
    );
    let (greeting_before, greeting_after) =
        (has_greeting(generated_text), has_greeting(final_text));
    let (link_before, link_after) = (has_link(generated_text), has_link(final_text));
    FollowupEditComparison {
        original: followup_text_stats(generated_text),
        final_stats: followup_text_stats(final_text),
        customer_name_removed: name_before && !name_after,
        customer_name_added: !name_before && name_after,
        greeting_removed: greeting_before && !greeting_after,
        greeting_added: !greeting_before && greeting_after,
        link_removed: link_before && !link_after,
        link_added: !link_before && link_after,
    }
}

fn word_set(text: &str) -> HashSet<String> {
    re!(r"\s+")
        .split(&re!(r"[^\p{L}\p{N}\s]", "gu").replace_all(&text.to_lowercase(), " "))
        .filter(|word| js_len(word) > 1)
        .map(str::to_string)
        .collect()
}

#[allow(clippy::cast_precision_loss)]
fn word_overlap_ratio(generated_text: &str, final_text: &str) -> f64 {
    let original = word_set(generated_text);
    let edited = word_set(final_text);
    if original.is_empty() || edited.is_empty() {
        return 0.0;
    }
    let overlap = original
        .iter()
        .filter(|word| edited.contains(*word))
        .count();
    overlap as f64 / original.len().max(edited.len()) as f64
}

/// A rewrite we cannot explain by name, greeting, link, punctuation or
/// length, with little shared wording.
#[allow(clippy::cast_precision_loss)]
pub fn is_unclear_followup_edit(
    generated_text: &str,
    final_text: &str,
    c: &FollowupEditComparison,
) -> bool {
    let char_ratio = if c.original.character_count == 0 {
        1.0
    } else {
        c.final_stats.character_count as f64 / c.original.character_count as f64
    };
    let explained = c.customer_name_removed
        || c.customer_name_added
        || c.greeting_removed
        || c.greeting_added
        || c.link_removed
        || c.link_added
        || c.original.question_count != c.final_stats.question_count
        || c.original.exclamation_count != c.final_stats.exclamation_count
        || (1.0 - char_ratio).abs() >= 0.25;
    !explained && word_overlap_ratio(generated_text, final_text) < 0.5
}

#[allow(clippy::cast_precision_loss)]
fn ratio(part: usize, whole: usize) -> f64 {
    part as f64 / whole as f64
}

fn majority_toggle(
    samples: &[FollowupStyleSample],
    removed: impl Fn(&FollowupStyleSample) -> bool,
    added: impl Fn(&FollowupStyleSample) -> bool,
) -> Option<FollowupTogglePreference> {
    let removed_count = samples.iter().filter(|s| removed(s)).count();
    let added_count = samples.iter().filter(|s| added(s)).count();
    let decided = removed_count + added_count;
    if decided < MIN_SAMPLES {
        return None;
    }
    if ratio(removed_count, decided) >= MIN_RATIO && removed_count >= MIN_SAMPLES {
        return Some(FollowupTogglePreference::Remove);
    }
    (ratio(added_count, decided) >= MIN_RATIO && added_count >= MIN_SAMPLES)
        .then_some(FollowupTogglePreference::Add)
}

const fn length_bucket(character_count: usize, channel: FollowupChannel) -> FollowupStyleLength {
    let (short, medium) = match channel {
        FollowupChannel::Sms => (SMS_SHORT_CHARS, SMS_MEDIUM_CHARS),
        FollowupChannel::Email => (EMAIL_SHORT_CHARS, EMAIL_MEDIUM_CHARS),
    };
    if character_count <= short {
        FollowupStyleLength::Short
    } else if character_count <= medium {
        FollowupStyleLength::Medium
    } else {
        FollowupStyleLength::Long
    }
}

fn majority_length(
    samples: &[FollowupStyleSample],
    channel: FollowupChannel,
) -> Option<FollowupStyleLength> {
    let channel_samples: Vec<&FollowupStyleSample> =
        samples.iter().filter(|s| s.channel == channel).collect();
    if channel_samples.len() < MIN_SAMPLES {
        return None;
    }
    [
        FollowupStyleLength::Short,
        FollowupStyleLength::Medium,
        FollowupStyleLength::Long,
    ]
    .into_iter()
    .find(|bucket| {
        let count = channel_samples
            .iter()
            .filter(|s| length_bucket(s.comparison.final_stats.character_count, channel) == *bucket)
            .count();
        ratio(count, channel_samples.len()) >= MIN_RATIO
    })
}

pub fn derive_followup_style_profile(samples: &[FollowupStyleSample]) -> FollowupStyleProfile {
    let mut profile = empty_followup_style_profile();
    if samples.is_empty() {
        return profile;
    }
    profile.sms_length = majority_length(samples, FollowupChannel::Sms);
    profile.email_length = majority_length(samples, FollowupChannel::Email);
    let dropped_all = samples
        .iter()
        .filter(|s| {
            s.comparison.original.question_count > 0 && s.comparison.final_stats.question_count == 0
        })
        .count();
    let capped_at_one = samples
        .iter()
        .filter(|s| {
            s.comparison.original.question_count >= 2
                && s.comparison.final_stats.question_count == 1
        })
        .count();
    if dropped_all >= MIN_SAMPLES && ratio(dropped_all, samples.len()) >= MIN_RATIO {
        profile.question_count = Some(FollowupQuestionPreference::None);
    } else if capped_at_one >= MIN_SAMPLES && ratio(capped_at_one, samples.len()) >= MIN_RATIO {
        profile.question_count = Some(FollowupQuestionPreference::OneMax);
    }
    let removed_exclamations = samples
        .iter()
        .filter(|s| {
            s.comparison.original.exclamation_count > 0
                && s.comparison.final_stats.exclamation_count == 0
        })
        .count();
    if removed_exclamations >= MIN_SAMPLES
        && ratio(removed_exclamations, samples.len()) >= MIN_RATIO
    {
        profile.exclamation = Some(ExclamationPreference::None);
    }
    profile.customer_name = majority_toggle(
        samples,
        |s| s.comparison.customer_name_removed,
        |s| s.comparison.customer_name_added,
    );
    profile.greeting = majority_toggle(
        samples,
        |s| s.comparison.greeting_removed,
        |s| s.comparison.greeting_added,
    );
    profile.link = majority_toggle(
        samples,
        |s| s.comparison.link_removed,
        |s| s.comparison.link_added,
    );
    profile
}

/// A stored profile, dropping values it does not know.
pub fn parse_followup_style_profile(value: &Value) -> Option<FollowupStyleProfile> {
    let record = value.as_object()?;
    let text = |key: &str| record.get(key).and_then(Value::as_str);
    Some(FollowupStyleProfile {
        sms_length: text("smsLength").and_then(FollowupStyleLength::parse),
        email_length: text("emailLength").and_then(FollowupStyleLength::parse),
        question_count: text("questionCount").and_then(FollowupQuestionPreference::parse),
        exclamation: (text("exclamation") == Some("NONE")).then_some(ExclamationPreference::None),
        customer_name: text("customerName").and_then(FollowupTogglePreference::parse),
        greeting: text("greeting").and_then(FollowupTogglePreference::parse),
        link: text("link").and_then(FollowupTogglePreference::parse),
        notes: record.get("notes").and_then(Value::as_array).map(|notes| {
            notes
                .iter()
                .filter_map(Value::as_str)
                .filter(|note| !trim(note).is_empty())
                .map(str::to_string)
                .collect()
        }),
    })
}

/// The prompt block with the rep's preferences (empty when none).
pub fn format_style_preference_prompt_block(profile: Option<&FollowupStyleProfile>) -> String {
    let Some(profile) = profile else {
        return String::new();
    };
    let mut lines: Vec<String> = Vec::new();
    let mut push = |line: &str| lines.push(line.to_string());
    match profile.sms_length {
        Some(FollowupStyleLength::Short) => {
            push("- SMS_LENGTH = SHORT: keep SMS around 120 characters or less.");
        }
        Some(FollowupStyleLength::Medium) => {
            push("- SMS_LENGTH = MEDIUM: keep SMS around 130–200 characters.");
        }
        Some(FollowupStyleLength::Long) => {
            push("- SMS_LENGTH = LONG: SMS may be a bit longer when it stays useful.");
        }
        None => {}
    }
    match profile.email_length {
        Some(FollowupStyleLength::Short) => {
            push("- EMAIL_LENGTH = SHORT: keep the email body brief.");
        }
        Some(FollowupStyleLength::Medium) => {
            push("- EMAIL_LENGTH = MEDIUM: keep the email body moderate, not long.");
        }
        Some(FollowupStyleLength::Long) => {
            push("- EMAIL_LENGTH = LONG: a fuller email body is acceptable.");
        }
        None => {}
    }
    match profile.question_count {
        Some(FollowupQuestionPreference::None) => {
            push("- QUESTION_COUNT = NONE: do not ask a question.");
        }
        Some(FollowupQuestionPreference::OneMax) => {
            push("- QUESTION_COUNT = ONE_MAX: at most one question.");
        }
        None => {}
    }
    if profile.exclamation.is_some() {
        push("- EXCLAMATION = NONE: no exclamation marks.");
    }
    match profile.customer_name {
        Some(FollowupTogglePreference::Remove) => push("- Do not use the customer first name."),
        Some(FollowupTogglePreference::Add) => push("- Always include the customer first name."),
        None => {}
    }
    match profile.greeting {
        Some(FollowupTogglePreference::Remove) => push("- Do not start with Hi / Hey / Hello."),
        Some(FollowupTogglePreference::Add) => {
            push("- Start with a greeting (Hi / Hey / Hello) when the GREETING rule allows one.")
        }
        None => {}
    }
    match profile.link {
        Some(FollowupTogglePreference::Remove) => push("- Do not include links."),
        Some(FollowupTogglePreference::Add) => push("- Include a relevant link when it fits."),
        None => {}
    }
    for note in profile.notes.iter().flatten() {
        lines.push(format!("- {note}"));
    }
    if lines.is_empty() {
        return String::new();
    }
    format!(
        "SALESPERSON WRITING PREFERENCES (learned from this user's edits — these override generic style notes above):\n{}",
        lines.join("\n")
    )
}
