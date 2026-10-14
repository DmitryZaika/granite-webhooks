//! The legacy follow-up prompt: system and user prompts for one message or
//! a cadence, the conversation and template blocks, and parsing the model's
//! JSON (port of `smsFlowAiFollowupPrompt.ts`).

// Message counts are JS numbers holding whole values, so exact compares are safe.
#![allow(clippy::float_cmp)]

use super::cadence::plan::next_cadence_anchor;
use super::cadence::ramp::{
    days_since_last_outbound, first_outbound_at_from_timeline,
    is_established_conversation_customer, ramp_outbound_count,
};
use super::cadence::types::FollowupSituation;
use super::crm_hold::{AI_HOLD_MAX_DELAY_MINUTES, CrmHoldUntil, format_crm_hold_prompt_line};
use super::dates::Instant;
use super::instruction::{
    admin_instruction_gap_minutes, ai_followup_admin_instruction_prompt_block,
};
use super::js::{js_len, js_number, js_round, js_slice, trim};
use super::pressure::build::newer_customer_statement_beats_terminal_note;
use super::pressure::format::format_contact_pressure_block;
use super::pressure::timeline::count_outbound_touches_by_store_day;
use super::pressure::{CommunicationHistoryStatus, CustomerFollowupState, FollowupContactPressure};
use super::prompt_texts::{
    CADENCE_FOLLOWUP_CORE_INSTRUCTIONS, CADENCE_FOLLOWUP_JSON_RULES, FOLLOWUP_ADMIN_INSTRUCTIONS,
    FOLLOWUP_JSON_RULES, FORCE_GENERATE_CADENCE_INSTRUCTION, TEMPLATE_PROMPT_HEADING,
};
use super::send_window::{FollowupChannel, format_store_clock, greeting_instruction_for_hour};
use super::store_time::store_zoned_parts;
use super::style::{FollowupStyleProfile, format_style_preference_prompt_block};
use super::timeline::{Channel, Direction, TimelineMessage};
use crate::{re, str_enum};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Drops a trailing email sign-off the model adds (the mailbox appends the
/// real signature).
pub fn strip_followup_email_signature(body: &str) -> String {
    let trim_end = |text: &str| re!(r"\s+$", "u").replace(text, "").into_owned();
    let mut text = trim_end(body);
    text = trim_end(&re!(
        r"\n+(?:best(?:\s+regards)?|thanks(?:\s+again)?|thank you|sincerely|regards|warm regards|kind regards|cheers|respectfully),?\s*\n+[^\n]{1,60}\s*$",
        "iu"
    )
    .replace(&text, ""));
    trim_end(&re!(
        r"\n+(?:best(?:\s+regards)?|thanks(?:\s+again)?|thank you|sincerely|regards|warm regards|kind regards|cheers|respectfully),?\s+[A-Za-z][^\n]{0,40}\s*$",
        "iu"
    )
    .replace(&text, ""))
}

/// A message of the conversation as the prompt prints it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FollowupTimelineMessage {
    pub at: String,
    pub channel: FollowupChannel,
    pub direction: Direction,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_phone: Option<String>,
}

impl FollowupTimelineMessage {
    /// The engine's timeline message for the same message.
    pub fn to_timeline(&self) -> TimelineMessage {
        let mut message = TimelineMessage::new(self.at.clone(), self.direction, self.text.clone());
        message.channel = Some(match self.channel {
            FollowupChannel::Sms => Channel::Sms,
            FollowupChannel::Email => Channel::Email,
        });
        message.from_email.clone_from(&self.from_email);
        message.from_phone.clone_from(&self.from_phone);
        message
    }
}

str_enum! { pub enum TemplateScope { Company = "company", Personal = "personal" } }

/// A template shown to the model as a voice example.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FollowupTemplateExample {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<FollowupChannel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<TemplateScope>,
}

const PROMPT_TIMELINE_MAX_CHARS: usize = 16_000;
/// Recent rep messages keep their own room.
const PROMPT_TIMELINE_OUTBOUND_RESERVED_CHARS: usize = 5_000;
const PROMPT_TIMELINE_MAX_INBOUND: usize = 16;
const PROMPT_TIMELINE_MAX_OUTBOUND: usize = 12;
const PROMPT_INBOUND_TEXT_CHARS: usize = 2_500;
const PROMPT_OUTBOUND_TEXT_CHARS: usize = 1_200;
/// Templates are voice examples: a few short ones teach tone.
const PROMPT_TEMPLATE_MAX_PER_GROUP: usize = 4;
const PROMPT_TEMPLATE_MAX_BODY_CHARS: usize = 400;

fn truncate_for_prompt(text: &str, max: usize) -> String {
    let trimmed = trim(text);
    if js_len(trimmed) <= max {
        return trimmed.to_string();
    }
    format!("{}… (truncated)", js_slice(trimmed, 0, max))
}

const fn prompt_timeline_text_limit(direction: Direction) -> usize {
    match direction {
        Direction::Inbound => PROMPT_INBOUND_TEXT_CHARS,
        Direction::Outbound => PROMPT_OUTBOUND_TEXT_CHARS,
    }
}

fn pick_prompt_timeline_indexes(
    timeline: &[FollowupTimelineMessage],
    direction: Direction,
    max_messages: usize,
    chars_already_used: usize,
    char_limit: usize,
) -> (Vec<usize>, usize) {
    let mut indexes = Vec::new();
    let mut chars = chars_already_used;
    for index in (0..timeline.len()).rev() {
        let message = &timeline[index];
        if message.direction != direction {
            continue;
        }
        if indexes.len() >= max_messages {
            break;
        }
        let size = js_len(trim(&message.text)).min(prompt_timeline_text_limit(direction));
        if chars + size > char_limit && !indexes.is_empty() {
            break;
        }
        indexes.push(index);
        chars += size;
    }
    (indexes, chars)
}

/// The FULL CONVERSATION block: customer messages first and with a larger
/// budget, older messages summarized as a count.
pub fn format_followup_timeline_block(timeline: &[FollowupTimelineMessage]) -> String {
    if timeline.is_empty() {
        return "(no prior SMS or email)".into();
    }
    let (inbound, inbound_chars) = pick_prompt_timeline_indexes(
        timeline,
        Direction::Inbound,
        PROMPT_TIMELINE_MAX_INBOUND,
        0,
        PROMPT_TIMELINE_MAX_CHARS - PROMPT_TIMELINE_OUTBOUND_RESERVED_CHARS,
    );
    let (outbound, _) = pick_prompt_timeline_indexes(
        timeline,
        Direction::Outbound,
        PROMPT_TIMELINE_MAX_OUTBOUND,
        inbound_chars,
        PROMPT_TIMELINE_MAX_CHARS,
    );
    let mut kept: Vec<usize> = inbound.into_iter().chain(outbound).collect();
    kept.sort_unstable();
    let rendered = kept
        .iter()
        .map(|index| {
            let message = &timeline[*index];
            let who = if message.direction == Direction::Inbound {
                "Customer"
            } else {
                "Rep"
            };
            let text =
                truncate_for_prompt(&message.text, prompt_timeline_text_limit(message.direction));
            format!(
                "[{}] {} {who}: {text}",
                message.at,
                message.channel.as_str().to_uppercase()
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let omitted = timeline.len() - kept.len();
    if omitted == 0 {
        return rendered;
    }
    format!(
        "({omitted} older message{} not reprinted here. Every count, date and fact from the full history is already in CONTACT PRESSURE and CUSTOMER / PROJECT INFO — use those. Never assume anything about the messages that are not printed.)\n\n{rendered}",
        if omitted == 1 { "" } else { "s" }
    )
}

fn format_one_template(
    template: &FollowupTemplateExample,
    index: usize,
    max_body_chars: Option<usize>,
) -> String {
    let subject_line = template
        .subject
        .as_deref()
        .filter(|s| !s.is_empty())
        .map_or_else(String::new, |s| format!("\nSubject: {s}"));
    let body = max_body_chars.map_or_else(
        || template.body.clone(),
        |max| truncate_for_prompt(&template.body, max),
    );
    format!("{}. {}{subject_line}\n{body}", index + 1, template.name)
}

/// The templates grouped as the rep's own first, then the company's.
pub fn format_followup_template_block(
    templates: &[FollowupTemplateExample],
    max_per_group: Option<usize>,
    max_body_chars: Option<usize>,
) -> String {
    if templates.is_empty() {
        return "(none on file)".into();
    }
    let groups: [(&str, FollowupChannel, TemplateScope); 4] = [
        (
            "THIS SALESPERSON'S SMS TEMPLATES (match this voice first)",
            FollowupChannel::Sms,
            TemplateScope::Personal,
        ),
        (
            "THIS SALESPERSON'S EMAIL TEMPLATES (match this voice first)",
            FollowupChannel::Email,
            TemplateScope::Personal,
        ),
        (
            "COMPANY SMS TEMPLATES (house style)",
            FollowupChannel::Sms,
            TemplateScope::Company,
        ),
        (
            "COMPANY EMAIL TEMPLATES (house style)",
            FollowupChannel::Email,
            TemplateScope::Company,
        ),
    ];
    let in_group = |template: &FollowupTemplateExample, channel, scope| {
        template.channel == Some(channel) && template.scope == Some(scope)
    };
    fn cap(
        items: Vec<&FollowupTemplateExample>,
        max_per_group: Option<usize>,
    ) -> Vec<&FollowupTemplateExample> {
        match max_per_group {
            Some(max) => items.into_iter().take(max).collect(),
            None => items,
        }
    }
    let render = |items: &[&FollowupTemplateExample]| {
        items
            .iter()
            .enumerate()
            .map(|(index, t)| format_one_template(t, index, max_body_chars))
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    let mut labeled: Vec<String> = groups
        .iter()
        .filter_map(|(label, channel, scope)| {
            let items = cap(
                templates
                    .iter()
                    .filter(|t| in_group(t, *channel, *scope))
                    .collect(),
                max_per_group,
            );
            (!items.is_empty()).then(|| format!("{label}:\n{}", render(&items)))
        })
        .collect();
    let leftover = cap(
        templates
            .iter()
            .filter(|t| !groups.iter().any(|(_, c, s)| in_group(t, *c, *s)))
            .collect(),
        max_per_group,
    );
    if !leftover.is_empty() {
        labeled.push(render(&leftover));
    }
    labeled.join("\n\n")
}

/// Everything the one-message prompt reads.
#[derive(Debug, Clone)]
pub struct FollowupPromptContext {
    pub channel: FollowupChannel,
    pub step_position: usize,
    pub situation: FollowupSituation,
    pub customer_profile: String,
    pub company_instructions: String,
    /// The company rule saved from the follow-up settings.
    pub admin_instruction: Option<String>,
    pub templates: Vec<FollowupTemplateExample>,
    pub timeline: Vec<FollowupTimelineMessage>,
    pub now: Option<Instant>,
    pub crm_hold: Option<CrmHoldUntil>,
    pub contact_pressure: Option<FollowupContactPressure>,
    pub style_profile: Option<FollowupStyleProfile>,
    pub force_generate: bool,
}

/// A generated message, or why the model skipped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum GeneratedFollowup {
    Skip {
        skip: bool,
        reason: String,
    },
    Message {
        skip: bool,
        body: String,
        subject: Option<String>,
    },
}

/// One message of a generated cadence.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GeneratedCadenceFollowup {
    pub skip: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub body: String,
    pub subject: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delay_minutes: Option<f64>,
}

impl GeneratedCadenceFollowup {
    pub fn skipped(reason: impl Into<String>) -> Self {
        Self {
            skip: true,
            reason: Some(reason.into()),
            body: String::new(),
            subject: None,
            delay_minutes: None,
        }
    }
}

pub fn cadence_has_writable_copy(parsed: Option<&[GeneratedCadenceFollowup]>) -> bool {
    parsed.is_some_and(|messages| {
        messages
            .iter()
            .any(|m| !m.skip && !trim(&m.body).is_empty())
    })
}

/// First contact, the second follow-up, or a stalled conversation.
pub fn classify_followup_situation(
    messages: &[TimelineMessage],
    has_relationship_history: bool,
) -> FollowupSituation {
    let outbound = count_outbound_touches_by_store_day(
        messages
            .iter()
            .filter(|m| m.direction == Direction::Outbound)
            .map(|m| Some(m.at.as_str())),
    );
    let inbound = messages
        .iter()
        .filter(|m| m.direction == Direction::Inbound)
        .count();
    if outbound == 0.0 {
        return if has_relationship_history {
            FollowupSituation::StalledConversation
        } else {
            FollowupSituation::FirstContact
        };
    }
    if outbound == 1.0 && inbound <= 1 {
        return FollowupSituation::SecondFollowup;
    }
    if messages
        .last()
        .is_some_and(|m| m.direction == Direction::Outbound)
        || messages.len() >= 6
    {
        return FollowupSituation::StalledConversation;
    }
    if outbound <= 1.0 {
        FollowupSituation::SecondFollowup
    } else {
        FollowupSituation::StalledConversation
    }
}

pub fn total_outbound_count(timeline: &[TimelineMessage]) -> f64 {
    count_outbound_touches_by_store_day(
        timeline
            .iter()
            .filter(|m| m.direction == Direction::Outbound)
            .map(|m| Some(m.at.as_str())),
    )
}

/// An outbound message already carried this company's stones page link.
pub fn stones_link_already_sent(
    timeline: &[FollowupTimelineMessage],
    customer_profile: &str,
) -> bool {
    let profile_url = re!(r#"https?:\/\/\S*\/customer\/\d+\/stones[^\s)"]*"#, "i")
        .find(customer_profile)
        .map(|m| m.as_str());
    let profile_path = re!(r"\/customer\/\d+\/stones", "i")
        .find(customer_profile)
        .map(|m| m.as_str());
    timeline
        .iter()
        .filter(|m| m.direction == Direction::Outbound)
        .any(|m| {
            profile_url.is_some_and(|url| !url.is_empty() && m.text.contains(url))
                || profile_path.is_some_and(|path| !path.is_empty() && m.text.contains(path))
                || re!(r"\/customer\/\d+\/stones", "i").is_match(&m.text)
        })
}

pub fn stones_link_supports_angle(angle: &str) -> bool {
    matches!(
        angle,
        "material_preference" | "design_help" | "helpful_information"
    )
}

pub fn stones_link_instruction(
    timeline: &[FollowupTimelineMessage],
    customer_profile: &str,
    angles: &[String],
    suppress_catalog_link: bool,
) -> &'static str {
    if stones_link_already_sent(timeline, customer_profile) {
        return "STONES LINK: already sent in an outbound SMS/email in FULL CONVERSATION — do not send the Customer page URL again unless they ask for it.";
    }
    if suppress_catalog_link {
        return "STONES LINK: do not include the Customer page URL for this customer/product scope.";
    }
    if !angles.is_empty() && !angles.iter().any(|a| stones_link_supports_angle(a)) {
        return "STONES LINK: do not include the Customer page URL for these steps.";
    }
    "STONES LINK: not yet sent. You may include the Customer page URL when it helps get a reply (browsing materials/options). At most once in the sequence. Keep SMS short if you include it."
}

fn established_conversation_delay_instruction(
    pressure: Option<&FollowupContactPressure>,
    admin_instruction: Option<&str>,
) -> String {
    if let Some(gap) = admin_instruction_gap_minutes(admin_instruction.unwrap_or("")) {
        #[allow(clippy::cast_precision_loss)]
        let days = js_number(gap as f64 / (24.0 * 60.0));
        return format!(
            "COMPANY CADENCE: the user instruction has higher priority than the default cadence and the Sunday rule. It requires a {days} day gap. Set delayMinutes to {gap} on every message, including message 1 and including Sunday. Write copy that fits a {days}-day wait, not weeks of silence."
        );
    }
    if !is_established_conversation_customer(pressure) {
        return String::new();
    }
    "CADENCE SPEED: this customer already has a long conversation. Do not follow up every few days. Message 1 waits about a week (delayMinutes around 10080), not immediately. Later messages wait 2–4 weeks. delayMinutes must match that slower cadence. Copy should sound like a check-in after time has passed, not a same-week chase.".into()
}

/// How long since we last wrote, and what that means for message 1.
pub fn contact_gap_instruction(
    days_since_last_outbound: Option<f64>,
    outbound_count: f64,
    pressure: Option<&FollowupContactPressure>,
) -> String {
    let Some(days) = days_since_last_outbound else {
        if pressure.is_some_and(|p| p.has_relationship_history) {
            return "NO SMS/EMAIL HISTORY, but CRM relationship history exists (notes, activities, holds, or calls). digital_message_history = none, relationship_history = existing. This is NOT a brand-new first_contact lead. Do not use fresh-lead cadence language or open with material/design if they were on hold. Reconnect from hold/timing context. Never mention specific dollar amounts from notes.".into();
        }
        return "NO SMS OR EMAIL ON FILE — we have not texted or emailed this customer and CRM shows no prior interaction. This is a true first contact. Open CRM activities/notes may be weeks old. Do not write as if you were just talking. Do not say you understand they are waiting. Keep it short.".into();
    };
    let prior = outbound_count.max(0.0);
    let plural = |word: &str| {
        if prior == 1.0 {
            word.to_string()
        } else {
            format!("{word}s")
        }
    };
    let days_text = js_number(days);
    let prior_text = js_number(prior);
    if prior >= 1.0 && days >= 14.0 {
        return format!(
            "Last outbound was {days_text} days ago — a long silence. We have already sent {prior_text} unanswered {}/{}. Message 1 reopens the conversation: pick up the last unfinished step or ask what has changed with the project. Never open with a detail question from their original form or old notes (which room, colors, measurements, removal) as if no time passed. Do NOT say \"I emailed you\", \"I texted you\", or \"wanted to make sure you got it\".",
            plural("email"),
            plural("text")
        );
    }
    if prior >= 1.0 {
        let when = if days == 0.0 {
            "today".to_string()
        } else {
            format!("{days_text} day{} ago", if days == 1.0 { "" } else { "s" })
        };
        return format!(
            "Last outbound was {when}. We have already sent {prior_text} unanswered {}/{} (messages on the same calendar day count as one). Do NOT say \"I emailed you\", \"I texted you\", or \"wanted to make sure you got it\". Use a new useful angle.",
            plural("email"),
            plural("text")
        );
    }
    format!(
        "Last outbound SMS/email was {days_text} days ago — a long silence. Do not claim you understand their situation. Low-pressure. No extra statements. Do not say you texted or emailed recently."
    )
}

fn situation_label(
    situation: FollowupSituation,
    outbound_count: f64,
    pressure: Option<&FollowupContactPressure>,
) -> &'static str {
    if let Some(p) = pressure.filter(|p| p.has_relationship_history && outbound_count == 0.0) {
        if p.customer_state == CustomerFollowupState::Hold || p.hold_reason_type.is_some() {
            return "HOLD REACTIVATION — existing old lead coming off a CRM hold. Not first_contact. First message tests whether timing is better now. Acknowledge the reconnect reason in general terms (fall, after home repairs). Do not mention dollar amounts. Do not open with material/design. Do not use fresh-lead urgency.";
        }
        return "EXISTING CUSTOMER, NO DIGITAL HISTORY — CRM notes/activities prove prior interaction. Not a true first_contact. Reconnect from known context. Do not treat missing SMS/email as a brand-new lead.";
    }
    if situation == FollowupSituation::FirstContact {
        return "FIRST CONTACT — no SMS/email and no meaningful CRM relationship history. Introduce naturally and narrow with one leading question from what we already know.";
    }
    if pressure
        .is_some_and(|p| p.communication_history_status == CommunicationHistoryStatus::TrulyNew)
        && (1.0..=2.0).contains(&outbound_count)
    {
        return "FIRST FOLLOW-UP / EARLY FOLLOW-UP — brand-new unanswered lead, not reactivation. Follow up soon while the lead is warm. Confirm they received the first message or whether they still want help with the known project. Use known kitchen scope naturally. Do not ask kitchen vs bathroom if known_scope is set. Do not write as if they are waiting, paused, or not ready. Do not repeat the last outbound color/style question.";
    }
    if outbound_count >= 1.0 {
        return "FOLLOW-UP — prior email/text still unanswered. Do NOT say you texted or emailed recently or ask if they received a prior message. New useful angle only. Do not rephrase the last unanswered message.";
    }
    "STALLED CONVERSATION — new useful angle only. Do not say you texted or emailed recently."
}

pub fn build_followup_system_prompt() -> String {
    format!(
        "{FOLLOWUP_ADMIN_INSTRUCTIONS}\n\n## Output format\n\nReturn ONLY valid JSON:\n{{\"skip\": false, \"subject\": \"email subject or empty string for SMS\", \"body\": \"the message to send\"}}\nor, only when the customer already declined, already booked, or asked not to be contacted:\n{{\"skip\": true, \"reason\": \"short reason\"}}\n\n{FOLLOWUP_JSON_RULES}"
    )
}

pub fn build_cadence_followup_system_prompt(force_generate: bool) -> String {
    let force_block = if force_generate {
        format!("\n\n{FORCE_GENERATE_CADENCE_INSTRUCTION}")
    } else {
        String::new()
    };
    format!(
        "{CADENCE_FOLLOWUP_CORE_INSTRUCTIONS}\n\n## Output format\n\nReturn ONLY valid JSON with exactly the requested number of messages:\n{{\"skip\": false, \"messages\": [{{\"skip\": false, \"subject\": \"email subject or empty string for SMS\", \"body\": \"the message to send\", \"delayMinutes\": 0}}]}}\nor, only when the customer already declined, already booked, or asked not to be contacted:\n{{\"skip\": true, \"reason\": \"short reason\"}}\n\n{CADENCE_FOLLOWUP_JSON_RULES}\n- delayMinutes is required on every non-skip message: minutes to wait after the previous message (0 = send as soon as allowed for message 1).\n- YOU choose delayMinutes for each message from this customer's conversation. There is no fixed maximum gap. Never land a send on Sunday in store local time; if a wait would fall on Sunday, use Monday. Sunday is allowed only when the customer asked to be contacted that Sunday. An administrative instruction has higher priority than this choice, the default cadence, and the Sunday rule.\n- These messages are a no-response sequence. Later messages assume earlier ones got no reply.\n- YOU choose what each message says and how to approach it. Match only the Channel (SMS short / EMAIL with subject). Do not follow a fixed message-type sequence.\n- If the sequence mixes SMS and email, do not copy the same text across channels — each message should stand on its own.\n- When the conversation has an unfinished next step the customer asked for, you may resume it early if that is the best way to get a reply.{force_block}"
    )
}

fn admin_instruction_section(instruction: Option<&str>) -> String {
    let block = ai_followup_admin_instruction_prompt_block(instruction.unwrap_or(""));
    if block.is_empty() {
        block
    } else {
        format!("{block}\n\n")
    }
}

fn information_precedence_block(pressure: Option<&FollowupContactPressure>) -> String {
    let extra = if pressure.is_some_and(newer_customer_statement_beats_terminal_note) {
        "The latest customer message is newer than any Closed/Lost/Completed mark. Treat older \"completed elsewhere\" / lost notes as stale. Do not skip."
    } else {
        ""
    };
    format!(
        "INFORMATION PRECEDENCE: Recent direct customer statement > recent salesperson-confirmed information > CRM activity/hold > old internal note > inferred information. If facts contradict, prefer the newer customer communication unless a human marked Closed/Lost/Completed after it.\n{extra}"
    )
}

fn engine_timeline(timeline: &[FollowupTimelineMessage]) -> Vec<TimelineMessage> {
    timeline
        .iter()
        .map(FollowupTimelineMessage::to_timeline)
        .collect()
}

fn style_line(profile: Option<&FollowupStyleProfile>) -> String {
    let block = format_style_preference_prompt_block(profile);
    if block.is_empty() {
        block
    } else {
        format!("\n{block}")
    }
}

fn profile_block(profile: &str) -> &str {
    let trimmed = trim(profile);
    if trimmed.is_empty() {
        "(none)"
    } else {
        trimmed
    }
}

/// The user prompt for one flow message.
pub fn build_followup_user_prompt(ctx: &FollowupPromptContext) -> String {
    let channel_label = if ctx.channel == FollowupChannel::Email {
        "EMAIL"
    } else {
        "SMS"
    };
    let template_block = format_followup_template_block(
        &ctx.templates,
        Some(PROMPT_TEMPLATE_MAX_PER_GROUP),
        Some(PROMPT_TEMPLATE_MAX_BODY_CHARS),
    );
    let timeline_block = format_followup_timeline_block(&ctx.timeline);
    let now = ctx.now.unwrap_or_else(chrono::Utc::now);
    let timeline = engine_timeline(&ctx.timeline);
    let days_since = days_since_last_outbound(&timeline, now);
    let ramp_count = ramp_outbound_count(&timeline, false, None);
    let outbound_count = total_outbound_count(&timeline);
    let chase_days = first_outbound_at_from_timeline(&timeline).map_or(days_since, |first| {
        #[allow(clippy::cast_precision_loss)]
        let days = (now - first).num_milliseconds() as f64 / (24.0 * 60.0 * 60.0 * 1000.0);
        Some(days.max(0.0))
    });
    let sequence_block = next_cadence_anchor(days_since, Some(ramp_count), Some(chase_days))
        .map_or_else(String::new, |step| {
            format!("SEQUENCE STEP: {} — {}", step.goal, step.purpose)
        });
    let parts = store_zoned_parts(now);
    let greeting_block = format!(
        "STORE LOCAL TIME: {} — {}",
        format_store_clock(parts.hour, parts.minute),
        greeting_instruction_for_hour(parts.hour)
    );
    let pending_step_block = if ctx.step_position <= 1 {
        "UNFINISHED NEXT STEP: If the conversation shows something the customer already agreed to or asked for that never happened (rescheduling a measurement or estimate, a call back, a price, photos), resume it with one concrete offer. If our last message already asked that exact question and got no answer, offer a different concrete option instead of repeating it."
    } else {
        "UNFINISHED NEXT STEP: An earlier step already offered to pick up any pending next step. Do not ask to book again — follow the goal for this step."
    };
    let pressure = ctx.contact_pressure.as_ref();
    let crm_hold_block = ctx
        .crm_hold
        .as_ref()
        .map_or_else(String::new, format_crm_hold_prompt_line);
    let stones_block = stones_link_instruction(
        &ctx.timeline,
        &ctx.customer_profile,
        &[],
        pressure.is_some_and(|p| {
            p.customer_state == CustomerFollowupState::Hold || p.hold_reason_type.is_some()
        }),
    );
    let pressure_block = pressure.map_or_else(String::new, format_contact_pressure_block);
    format!(
        "Write the next {channel_label} follow-up for automated flow step #{}.\n\nThis prompt contains exactly one customer. Use only this customer's structured state and source evidence.\n\nWrite as the salesperson in first person (I/we). Never name the sales rep or introduce yourself (\"<name> here\"). Sound like a real human SMS/email — plain and concrete — never corporate or AI coaching language.{}\n\n{pressure_block}\nTIME SINCE LAST CONTACT: {}\nPRIOR OUTBOUND COUNT: {} (one per calendar day — several messages sent the same day count as one — including any not reprinted in FULL CONVERSATION — this number is authoritative). Build each wait from that count. Do not use delivery-confirmation language.\n{greeting_block}\n{pending_step_block}\n{sequence_block}\n{crm_hold_block}\n{}\n{stones_block}\n\nSITUATION: {}\n\n{}CUSTOMER / PROJECT INFO:\n{}\n\n{TEMPLATE_PROMPT_HEADING}\n{template_block}\n\nFULL CONVERSATION (oldest first):\n{timeline_block}\n\nReturn JSON only.",
        ctx.step_position,
        style_line(ctx.style_profile.as_ref()),
        contact_gap_instruction(days_since, outbound_count, pressure),
        js_number(outbound_count),
        information_precedence_block(pressure),
        situation_label(ctx.situation, outbound_count, pressure),
        admin_instruction_section(ctx.admin_instruction.as_deref()),
        profile_block(&ctx.customer_profile),
    )
}

/// Everything the cadence prompt reads.
#[derive(Debug, Clone)]
pub struct CadencePromptContext {
    pub channel: FollowupChannel,
    pub customer_profile: String,
    pub company_instructions: String,
    pub admin_instruction: Option<String>,
    pub templates: Vec<FollowupTemplateExample>,
    pub timeline: Vec<FollowupTimelineMessage>,
    pub now: Option<Instant>,
    pub crm_hold: Option<CrmHoldUntil>,
    pub contact_pressure: Option<FollowupContactPressure>,
    pub style_profile: Option<FollowupStyleProfile>,
    pub force_generate: bool,
    /// The channel of each planned message, in order.
    pub step_channels: Vec<FollowupChannel>,
}

/// The user prompt for a whole cadence.
pub fn build_cadence_followup_user_prompt(ctx: &CadencePromptContext) -> String {
    build_cadence_followup_user_prompt_at(ctx, chrono::Utc::now())
}

/// [`build_cadence_followup_user_prompt`] with the wall clock given: the
/// time since the last outbound is counted from `clock`, not `ctx.now`
/// (only the greeting reads `ctx.now`).
pub fn build_cadence_followup_user_prompt_at(ctx: &CadencePromptContext, clock: Instant) -> String {
    let channel_label = if ctx.channel == FollowupChannel::Email {
        "EMAIL"
    } else {
        "SMS"
    };
    let template_block = format_followup_template_block(
        &ctx.templates,
        Some(PROMPT_TEMPLATE_MAX_PER_GROUP),
        Some(PROMPT_TEMPLATE_MAX_BODY_CHARS),
    );
    let timeline_block = format_followup_timeline_block(&ctx.timeline);
    let timeline = engine_timeline(&ctx.timeline);
    let days_since = days_since_last_outbound(&timeline, clock);
    let outbound_count = total_outbound_count(&timeline);
    let now_parts = store_zoned_parts(ctx.now.unwrap_or(clock));
    let step_block = ctx
        .step_channels
        .iter()
        .enumerate()
        .map(|(index, channel)| {
            format!(
                "MESSAGE {}\nChannel: {}",
                index + 1,
                if *channel == FollowupChannel::Email {
                    "EMAIL"
                } else {
                    "SMS"
                }
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let mixed = ctx.step_channels.contains(&FollowupChannel::Sms)
        && ctx.step_channels.contains(&FollowupChannel::Email);
    let sequence_label = if mixed {
        "SMS and EMAIL"
    } else {
        channel_label
    };
    let pressure = ctx.contact_pressure.as_ref();
    let pressure_block = pressure.map_or_else(String::new, format_contact_pressure_block);
    let force_block = if ctx.force_generate {
        format!("{FORCE_GENERATE_CADENCE_INSTRUCTION}\n\n")
    } else {
        String::new()
    };
    let crm_hold_line = ctx.crm_hold.as_ref().map_or_else(String::new, |hold| {
        format!("{}\n", format_crm_hold_prompt_line(hold))
    });
    let count = ctx.step_channels.len();
    format!(
        "{force_block}Write {count} {sequence_label} follow-ups for an automated no-response sequence.\n\nThis prompt contains exactly one customer. Use only this customer's structured state and source evidence.\n\nWrite as the salesperson in first person (I/we). Never name the sales rep or introduce yourself (\"<name> here\"). Sound like a real human SMS/email — plain and concrete — never corporate or AI coaching language.{}\nMatch each message to its Channel: SMS stays short; EMAIL needs a specific subject.\nYOU choose what each message is about and how to open it (greeting, soft reconnect, offer, question, value, etc.). There is no required message-type order.\n{}\nFor each message you MUST set delayMinutes (minutes after the previous message; 0 = send as soon as allowed for message 1). You choose the wait from this customer's conversation — there is no fixed maximum gap. Never land a send on Sunday in store local time; if a wait would fall on Sunday, use Monday. Sunday is allowed only when the customer asked to be contacted that Sunday. An administrative instruction has higher priority than this choice, the default cadence, and the Sunday rule. Write copy that fits the wait that instruction sets, or the wait you chose when it sets none. Relative time phrases (\"next month\", \"this spring\") must match that schedule, not today.\n{}\nUse the customer's first name often when it is in CUSTOMER / PROJECT INFO (most messages should include it). Skip it on occasional messages so the sequence does not feel robotic. Never invent a name.\nSTORE LOCAL TIME NOW: {} — use a natural greeting when it fits; do not invent a fake clock time.\n\n{pressure_block}\nTIME SINCE LAST CONTACT: {}\nPRIOR OUTBOUND COUNT: {} (one per calendar day — several messages sent the same day count as one — including any not reprinted in FULL CONVERSATION — this number is authoritative). Build each wait from that count. Never say \"I emailed you\", \"I texted you\", or \"make sure you got it\". Each message needs a new useful reason to exist.\nUse how long since last contact as context when choosing delayMinutes and tone. Later messages assume still no reply after each wait — keep them short; do not merely rephrase earlier ones.\n{crm_hold_line}\n{}\nUNFINISHED NEXT STEP: Prefer facts from unfinished_next_step only if unfinished_possibly_satisfied is no AND unfinished_next_step_kind is customer_request. A salesperson_task is weak context. If deal_product_scope is CABINETS, stay on cabinets — no stone inventory or quartz/granite. Do not invent product scope.\n\n{}CUSTOMER / PROJECT INFO:\n{}\n\n{TEMPLATE_PROMPT_HEADING}\n{template_block}\n\nFULL CONVERSATION (oldest first):\n{timeline_block}\n\nSEQUENCE (oldest / soonest first) — YOU choose the approach for each:\n{step_block}\n\nReturn JSON only with {count} messages in this order, each with delayMinutes.",
        style_line(ctx.style_profile.as_ref()),
        stones_link_instruction(
            &ctx.timeline,
            &ctx.customer_profile,
            &[],
            pressure
                .is_some_and(|p| p.deal_product_scope == super::facts::DealProductScope::Cabinets)
        ),
        established_conversation_delay_instruction(pressure, ctx.admin_instruction.as_deref()),
        format_store_clock(now_parts.hour, now_parts.minute),
        contact_gap_instruction(days_since, outbound_count, pressure),
        js_number(outbound_count),
        information_precedence_block(pressure),
        admin_instruction_section(ctx.admin_instruction.as_deref()),
        profile_block(&ctx.customer_profile),
    )
}

fn strip_code_fence(raw: &str) -> String {
    let trimmed = trim(raw);
    let without_open = re!(r"^```(?:json)?\s*", "i").replace(trimmed, "");
    re!(r"\s*```$", "u").replace(&without_open, "").into_owned()
}

fn parse_record(
    record: &serde_json::Map<String, Value>,
    channel: FollowupChannel,
) -> Option<GeneratedFollowup> {
    let text = |key: &str| {
        record
            .get(key)
            .and_then(Value::as_str)
            .map(|s| trim(s).to_string())
            .unwrap_or_default()
    };
    if record.get("skip") == Some(&Value::Bool(true)) {
        let reason = text("reason");
        return Some(GeneratedFollowup::Skip {
            skip: true,
            reason: if reason.is_empty() {
                "skip".into()
            } else {
                reason
            },
        });
    }
    let body_raw = text("body");
    let body = if channel == FollowupChannel::Email {
        trim(&strip_followup_email_signature(&body_raw)).to_string()
    } else {
        body_raw
    };
    if body.is_empty() {
        return None;
    }
    let subject_raw = text("subject");
    let subject = (channel == FollowupChannel::Email).then(|| {
        if subject_raw.is_empty() {
            "Following up".to_string()
        } else {
            subject_raw
        }
    });
    Some(GeneratedFollowup::Message {
        skip: false,
        body,
        subject,
    })
}

/// The model's reply for one message (`None` when unusable).
pub fn parse_followup_generation(raw: &str, channel: FollowupChannel) -> Option<GeneratedFollowup> {
    match serde_json::from_str::<Value>(&strip_code_fence(raw)).ok()? {
        Value::Object(record) => parse_record(&record, channel),
        _ => None,
    }
}

/// A model-chosen gap, `None` when omitted.
pub fn parse_model_delay_minutes(value: Option<&Value>) -> Option<f64> {
    #[allow(clippy::cast_precision_loss)]
    let cap = AI_HOLD_MAX_DELAY_MINUTES as f64;
    value
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite())
        .map(|v| (js_round(v).max(0.0) + 0.0).min(cap))
}

fn parse_one_cadence_message(
    value: &Value,
    channel: FollowupChannel,
) -> Option<GeneratedCadenceFollowup> {
    let record = value.as_object()?;
    match parse_record(record, channel)? {
        GeneratedFollowup::Skip { reason, .. } => Some(GeneratedCadenceFollowup::skipped(reason)),
        GeneratedFollowup::Message { body, subject, .. } => Some(GeneratedCadenceFollowup {
            skip: false,
            reason: None,
            body,
            subject,
            delay_minutes: parse_model_delay_minutes(record.get("delayMinutes")),
        }),
    }
}

/// The model's cadence reply: one message per channel (`None` when
/// unusable, empty when the model skipped the whole sequence).
pub fn parse_cadence_followup_generation(
    raw: &str,
    channels: &[FollowupChannel],
) -> Option<Vec<GeneratedCadenceFollowup>> {
    let count = channels.len();
    if count == 0 {
        return Some(Vec::new());
    }
    let parsed: Value = serde_json::from_str(&strip_code_fence(raw)).ok()?;
    let list = match &parsed {
        Value::Array(items) => items,
        Value::Object(record) => {
            if record.get("skip") == Some(&Value::Bool(true)) {
                return Some(Vec::new());
            }
            record.get("messages")?.as_array()?
        }
        _ => return None,
    };
    if list.len() < count {
        return None;
    }
    list.iter()
        .take(count)
        .enumerate()
        .map(|(index, item)| {
            parse_one_cadence_message(
                item,
                channels.get(index).copied().unwrap_or(FollowupChannel::Sms),
            )
        })
        .collect()
}
