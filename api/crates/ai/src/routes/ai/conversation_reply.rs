//! `POST /v1/ai/conversation-reply`: the AI Reply button.
//!
//! Drafts the next SMS or email from the whole conversation (what the
//! follow-up writer reads): a fast reply, a follow-up, or a custom draft and
//! its revisions. Port of the Remix `api.aiRecommend.conversationReply`
//! route and `aiConversationReply.server.ts`.

use crate::ai::{AiError, Caller, Chat, LARGE_MODEL, MINI_MODEL};
use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::followups::crm_hold::parse_followup_timeline_at;
use crate::followups::gather::{FollowupRequest, gather_followup_context};
use crate::followups::greeting::{conversation_recency, describe_ago, greeting_instruction};
use crate::followups::js::{js_len, js_slice, trim};
use crate::followups::prompt::FollowupTimelineMessage;
use crate::followups::rep_voice::{RepVoice, format_voice_block};
use crate::followups::send_window::FollowupChannel;
use crate::followups::timeline::Direction;
use crate::openapi::ToSchema;
use crate::re;
use crate::routes::ai::compose::format_rep_notes_block;
use crate::routes::ai::reply_context;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// The model when the main one is not available to the key.
pub const REPLY_FALLBACK_MODEL: &str = "gpt-5.6-luna";
/// The drafting model: the larger one, since a draft that sounds like the
/// rep and uses the customer's quote is worth the cost.
pub const REPLY_MODEL: &str = LARGE_MODEL;
/// It rejects `none`; `low` keeps the button quick.
const REPLY_REASONING_EFFORT: &str = "low";
/// The whole request must answer within the 30 s the API's CDN waits, so the
/// large model gets most of it and the quick fallback the rest.
const REPLY_MODEL_TIMEOUT: Duration = Duration::from_secs(18);
const FALLBACK_TIMEOUT: Duration = Duration::from_secs(9);
const SMS_MAX_TEXT: usize = 1600;

pub const CONVERSATION_REPLY_SYSTEM_PROMPT: &str = r#"You help a countertop sales rep draft the next SMS/email to a customer. Write it the way THIS rep writes, using THIS customer's real details.

PRIMARY TASK — write the next message this conversation itself calls for:
1. Read the actual thread. SMS and email are one history. Use what they already said: timing, wait periods, promised callbacks, the project, and personal situation.
2. If the customer just wrote: reply to that message.
3. If the last message is from the rep: write the next outreach the thread requires now — especially a follow-through you already promised.
4. Move it forward with ONE natural question or one concrete next step.

## Greeting and the customer's name: obey the GREETING line in the request
- GREETING: NONE → the conversation is ongoing (we already sent them an SMS or email today). No greeting ("Hi", "Hey", "Hello", "Good morning", "Dear") and no customer name anywhere in the message, on SMS and email alike. Start straight with the content, like the next text in a running thread.
- GREETING: BY NAME → a brand-new conversation, or we have not written to them yet today (even when they wrote to us this morning): open with a short greeting and their first name ("Hi Ann,"). Never "Hello <Full Name>" or "Dear".
- The customer's name appears at most once in a message.
- Never add a greeting just because the channel is email.

## Be specific — use the facts you have
- Build the message on concrete facts from QUOTES, CUSTOMER / PROJECT INFO and the thread: the stone/color they picked or are choosing between, the area (kitchen, island, vanity), the edge, the quote and whether they opened it, a measurement or install date, the open next step.
- Sent quote NOT opened yet → offer to resend it or walk them through it. Opened it (especially more than once) → ask what they think or which option they lean toward. Accepted or signed → the next step (template/measure, deposit, scheduling), not more selling.
- The rep already followed up on the sent quote and the customer went quiet (see a QUOTE ALREADY FOLLOWED UP note) → stop asking about the quote. They have it and have no questions. Ask one short question: is the project still on track / still moving forward? Do not repeat the quote's stones, address or options.
- A contractor, builder or designer quoting for their own client (the client is named in the thread) → ask about that client by first name ("Is Maria still moving forward?"), never "what did Maria think of the quote".

## Dates: read them
Every message and quote shows its date and how long ago it was. Know how long the customer has had the quote and how long they have been quiet, and write for that moment. Never write as if an old message were recent.
- Never invent prices, dates, availability, discounts, or promises. Mention a price only from QUOTES (never from a draft quote), and only when it helps.
- With a FOCUS DEAL, prefer that deal's quote and facts.

## Sound like the rep
- Match HOW THE REP WRITES and RECENT MESSAGES BY THE REP: their length, openers, punctuation, casing, emoji (or none), and how they end messages. Borrow the voice, never the content.
- Mirror the channel and the thread: SMS or a text-like thread → 1–3 short sentences, texting tone. Email → still short (2–5 sentences), plain words, no corporate filler. If the customer writes in a few words, keep yours short too.
- Do not sound like AI: no stock lines ("I hope this finds you well", "I hope you're doing well", "Just following up", "Just checking in", "I wanted to reach out", "Feel free to reach out", "Looking forward to hearing from you"), no em dashes, no polished or salesy phrasing. Write what the rep would actually type.
- Follow the SIGNATURE line in the request.

Promised follow-up is the next message, not a new intro:
Use "reaching out as we agreed" ONLY when the customer asked you to wait and you agreed ("10-12 days would be best" / "Great, will do").
Then ask how that specific situation is going.
If nobody agreed on a later check-in, never write "as we agreed", "reaching out as we agreed", or a generic "how is the project / renovation coming along?"
Continue the open topic instead: the quote they have not answered, the slabs they are choosing between, the edge, or the discount. Once the rep has already followed up on the quote with no answer, the open topic is whether the project is still on track.
Do not re-introduce yourself. Do not invent an agreement. Do not write a first-contact "you reached out about countertops" message.

NEPQ style when they just answered: curious, calm, conversational, non-pushy.

STRUCTURE: Continue the thread → ONE question.
Usually only one question. Make it feel like a natural continuation.

## Soft curiosity is required for short confirmations

When the customer gives a short confirmation like "Pushed back", "Later", "Not sure", "Still thinking", "Too expensive", "Went with someone else", or "Not interested":

You MUST soften the question. Direct interrogation sounds pushy.

Use a softener such as:
- "Just out of curiosity,"
- "If you don't mind me asking,"
- "Just so I understand,"

EXACT TRAINING EXAMPLE — follow this:

Rep: "Hi, Valeria. Are you still planning the island countertop, or has the project been pushed back?"
Customer: "Pushed back"

BAD (kills the conversation):
"Okay, thank you for letting me know. Please contact us whenever you're ready."

BAD (too blunt / interrogating — missing softener):
"Got it. What changed that caused you to push it back?"

BAD (corporate / project-manager tone):
"Got it. What's causing the delay with the island project?"

GOOD (required style):
"Got it. Just out of curiosity, what changed that caused you to push it back?"

For this exact situation, prefer wording very close to the GOOD example.

Promised later text — ONLY when they asked you to wait and you agreed:
Customer: "10-12 days would be best"
Rep: "Great, will do"
GOOD next message, two weeks later (GREETING: NONE, the thread is only weeks old):
"Reaching out as we agreed. How is the project going?"
BAD next message:
"Hi Lenora, you reached out about new countertops. Is there a good time for a call?"

No agreement in the thread (quote sent, customer chose an edge, named two slabs, then went quiet):
BAD: "Hi Ahoura, reaching out as we agreed. How is the kitchen renovation coming along?"
GOOD: "Did you get a chance to look over the quote? Between Viraldi and Idillio, which one are you leaning toward?"

Ongoing conversation (GREETING: NONE): the rep texted 20 minutes ago "Just sent the island quote", and now drafts the email:
BAD: "Hi Maria, I hope you're doing well! As promised, please find the quote for your island."
GOOD: "Here's the island quote in Calacatta Karmelo with the eased edge we talked about. Want me to hold that slab while you look it over?"

Customer opened the quote three times but has not replied (QUOTES shows it):
GOOD: "Saw you had a chance to look at the quote. Is the Viraldi option still the one you like for the kitchen, or should I price the island in it too?"

A contractor asked for a quote for his client Maria. Quote emailed 26 days ago, he confirmed he got it, the rep asked 14 days ago "Have you had a chance to review the quote?", no reply since:
BAD: "What did Maria think of the quote for the Calacatta Karmelo countertops and backsplash at 512 Elm St?"
GOOD: "Is Maria still moving forward with the project?"

Last message 4 months ago (GREETING: BY NAME):
GOOD: "Hi Ahoura, are you still planning the kitchen this year? I can check if the Viraldi you liked is still in stock."

More GOOD patterns:
- "Maybe later." → "Got it. Is it mainly the timing that's holding things up, or is there something else?"
- "We're still thinking." → "Of course. What are you still trying to figure out before making a decision?"
- "It's too expensive." → "I understand. When you say it's too expensive, is it more than you were expecting to spend, or are you comparing it with another option?"
- "We went with someone else." → "Got it. Just so I understand, what ended up being the biggest reason you decided to go with them?"
- "Not interested anymore." → "Understood. Did something change with the project itself, or did you decide to go in a different direction?"

## Rules

- Read SMS + email as one history. Do not re-ask known facts.
- If they ask a direct question: answer first, then optionally one follow-up.
- Never end the conversation with dead closers ("let us know", "reach out whenever", "we'll be here").
- Do not push for an appointment before you understand the situation.
- Mirror their words ("pushed back" not "delay" / "timeline" / "project status").
- Do not restate long project descriptions when a short curious question works.
- Sound like a real salesperson texting: short sentences, natural language.
- Vary openers: "Got it.", "Gotcha.", "Makes sense.", "I see.", "Understood."
- Write in first person (I/we). Never name the rep in the third person.
- SMS: 1–3 short sentences. Email: still concise.
- Email body is the message only. Never prefix it with "Subject:"; the thread already has a subject.

## Custom requests override length and structure
When REPLY MODE is Custom or Custom revision, the rep's instruction decides what the message says and how long it is. Include every point the rep wrote, even when that takes many sentences or several questions. The sentence limits, "ONE question" and "short" rules above apply only to Fast reply and Follow up. Voice, greeting, signature, no invented facts and no AI stock lines still apply.

Output ONLY valid JSON: {"body":"draft message"}"#;

/// The rep's unanswered follow-up on a sent quote is this old before the
/// next message checks the project is still on track.
const QUOTE_FOLLOW_UP_QUIET_DAYS: i64 = 3;

/// `2026-10-12 14:30, 14 days ago`; the bare time when it does not parse.
fn dated(at: &str, now: chrono::DateTime<chrono::Utc>) -> String {
    parse_followup_timeline_at(at).map_or_else(
        || at.to_string(),
        |t| format!("{at}, {}", describe_ago((now - t).num_minutes().max(0))),
    )
}

fn format_timeline(
    messages: &[FollowupTimelineMessage],
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    if messages.is_empty() {
        return "(no prior SMS or email)".into();
    }
    messages
        .iter()
        .map(|m| {
            let who = if m.direction == Direction::Inbound {
                "Customer"
            } else {
                "Sales Rep"
            };
            format!(
                "[{}] {} {who}: {}",
                dated(&m.at, now),
                m.channel.as_str().to_uppercase(),
                m.text
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// A reply short or vague enough to call for a soft discovery question.
fn is_short_vague_reply(text: &str) -> bool {
    let lower = trim(text).to_lowercase();
    let normalized = re!(r"[.!?]+$", "g").replace(&lower, "");
    if normalized.is_empty() {
        return false;
    }
    if js_len(&normalized) <= 40 {
        return true;
    }
    [
        "maybe later",
        "not sure",
        "still thinking",
        "pushed back",
        "not interested",
        "too expensive",
        "went with someone else",
        "we have to wait",
        "not right now",
    ]
    .iter()
    .any(|phrase| normalized.contains(phrase))
}

/// The customer asked to wait and the rep agreed to follow up later.
pub fn conversation_has_promised_follow_up(messages: &[FollowupTimelineMessage]) -> bool {
    let recent = &messages[messages.len().saturating_sub(12)..];
    let customer_asked = recent.iter().any(|m| {
        m.direction == Direction::Inbound
            && re!(
                r"\b(\d+\s*[-–to]+\s*\d+\s*days|\d+\s*days would be best|in a week|next week|text me (?:later|in|after)|reach out (?:later|in \d+))\b",
                "i"
            )
            .is_match(&m.text)
    });
    let rep_agreed = recent.iter().any(|m| {
        m.direction == Direction::Outbound
            && re!(
                r"\b(i'?ll text you|i will text you|will do|i'?ll follow up|i'?ll check back|i'?ll reach out|text you in \d+|should i text you in)\b",
                "i"
            )
            .is_match(&m.text)
    });
    customer_asked && rep_agreed
}

/// What kind of draft.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReplyMode {
    /// A short reply to the latest customer message.
    #[default]
    FastReply,
    /// The next message on the open topic the customer has not answered.
    FollowUp,
    /// A draft following the rep's request (or a revision of one).
    Custom,
}

/// The user prompt (port of `buildConversationReplyUserPrompt`).
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub fn build_user_prompt(
    channel: FollowupChannel,
    user_name: &str,
    profile: &str,
    company_instructions: &str,
    timeline: &[FollowupTimelineMessage],
    deal_id: Option<i64>,
    mode: ReplyMode,
    custom: &CustomDraft,
    extras: &ReplyExtras,
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    let channel_label = if channel == FollowupChannel::Email {
        "EMAIL"
    } else {
        "SMS"
    };
    let latest_inbound = timeline
        .iter()
        .rev()
        .find(|m| m.direction == Direction::Inbound);
    let last = timeline.last();
    let instructions = trim(company_instructions);
    let instructions = if instructions.is_empty() {
        "(none)"
    } else {
        instructions
    };
    let latest_inbound_note = latest_inbound.map_or_else(
        || "LATEST CUSTOMER MESSAGE: (none in thread — write a short re-engagement that invites a response without a dead-end closer)".to_string(),
        |m| format!("LATEST CUSTOMER MESSAGE ({} at {}):\n{}", m.channel.as_str().to_uppercase(), dated(&m.at, now), m.text),
    );
    let quote_followed_up_note = match (
        last.filter(|m| m.direction == Direction::Outbound)
            .and_then(|m| parse_followup_timeline_at(&m.at)),
        extras.last_quote_emailed,
    ) {
        (Some(nudged), Some(emailed))
            if nudged - emailed >= chrono::Duration::hours(12)
                && now - nudged >= chrono::Duration::days(QUOTE_FOLLOW_UP_QUIET_DAYS) =>
        {
            format!(
                "\nNOTE: QUOTE ALREADY FOLLOWED UP. The customer got the quote {} and the rep's follow-up after it ({}) got no reply.\nDo NOT ask again whether they reviewed the quote, what they (or their client) think of it, or about its stones, options, price or address.\nAsk ONE short question: is the project still on track / still moving forward? If the customer quotes for their own client named in the thread, ask about that client by first name.",
                describe_ago((now - emailed).num_minutes().max(0)),
                describe_ago((now - nudged).num_minutes().max(0)),
            )
        }
        _ => String::new(),
    };
    let promised_note = if last.is_some_and(|m| m.direction == Direction::Outbound)
        && conversation_has_promised_follow_up(timeline)
    {
        "\nNOTE: This thread already set a later check-in (they asked you to wait / you said you would text or follow up).\nDraft that follow-through now.\nGOOD: You reached out as agreed, and ask how the project is going.\nBAD: Re-introducing yourself, asking if they got your last message, or a generic \"still interested in countertops?\" opener.\nDo not treat this as a new cold follow-up."
    } else {
        ""
    };
    let direction_note = match last.map(|m| m.direction) {
        Some(Direction::Outbound) => format!(
            "NOTE: The last message in the thread was sent by the sales rep ({user_name}). The customer has not replied to it yet. Write the NEXT message this conversation now requires — the follow-through from what you already discussed — not another answer to an older unanswered question, and not a generic check-in."
        ),
        Some(Direction::Inbound) => "NOTE: The last message was from the customer. Draft a direct reply to what they just said.".to_string(),
        None => String::new(),
    };
    let greeting_note = greeting_instruction(conversation_recency(timeline, now));
    let signature_note = match channel {
        FollowupChannel::Sms => "SIGNATURE: none — texts never end with the rep's name.".to_string(),
        FollowupChannel::Email if extras.voice.has_email_signature => "SIGNATURE: The rep's email signature is added automatically below the body. Do not sign off with a name, title, phone, or company.".to_string(),
        FollowupChannel::Email => format!(
            "SIGNATURE: No signature is added. End the way the rep's own emails end (often just their first name{}), or with no sign-off. Never \"Best regards, <full name>\" plus a title.",
            extras
                .voice
                .email_name
                .as_deref()
                .and_then(|n| n.split_whitespace().next())
                .map(|first| format!(", \"{first}\""))
                .unwrap_or_default()
        ),
    };
    let quotes_block = if trim(&extras.quotes).is_empty() {
        "QUOTES: (none on file for this customer)".to_string()
    } else {
        format!(
            "QUOTES (this customer's quotes, newest first; times UTC):\n{}",
            trim(&extras.quotes)
        )
    };
    let notes_block = format_rep_notes_block(user_name, &extras.rep_notes, channel);
    let voice_block = format_voice_block(&extras.voice.examples);
    let style_section = [notes_block, voice_block]
        .into_iter()
        .filter(|b| !b.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    let style_section = if style_section.is_empty() {
        String::new()
    } else {
        format!("\n{style_section}\n")
    };
    let vague_note = match (last.map(|m| m.direction), latest_inbound) {
        (Some(Direction::Inbound), Some(inbound)) if is_short_vague_reply(&inbound.text) => {
            format!(
                "\nNOTE: Customer's last message is short/vague (\"{}\").\nThis is a discovery moment. Soften the question — do NOT ask bluntly.\nBAD for this case: \"Got it. What changed that caused you to push it back?\"\nGOOD for this case: \"Got it. Just out of curiosity, what changed that caused you to push it back?\"\nRequired: Acknowledge + softener (\"Just out of curiosity,\" / \"If you don't mind me asking,\" / \"Just so I understand,\") + ONE natural question about what changed / what's holding them up.\nDo NOT close the conversation. Do NOT use formal delay/timeline language.",
                trim(&inbound.text)
            )
        }
        _ => String::new(),
    };
    let length_note = if mode == ReplyMode::Custom {
        if channel == FollowupChannel::Sms {
            "CHANNEL: SMS — conversational texting tone. Length follows the rep's instruction (see REPLY MODE), not the 1–3 sentence rule."
        } else {
            "CHANNEL: EMAIL — length follows the rep's instruction (see REPLY MODE), not the 2–5 sentence rule. Do not include a Subject: line in the body."
        }
    } else if channel == FollowupChannel::Sms {
        "CHANNEL: SMS — keep the reply to 1–3 short sentences. One main question. Conversational texting tone."
    } else {
        "CHANNEL: EMAIL — slightly more detail is OK, but stay concise. Do not turn a simple exchange into a long email. Do not include a Subject: line in the body."
    };
    let deal_focus = deal_id.filter(|id| *id > 0).map_or_else(String::new, |id| {
        format!("\nFOCUS DEAL: Prioritize context for deal id {id} when relevant.\n")
    });
    let rep_outbound: Vec<&str> = timeline
        .iter()
        .filter(|m| m.direction == Direction::Outbound)
        .map(|m| trim(&m.text))
        .filter(|t| !t.is_empty())
        .collect();
    let rep_outbound = &rep_outbound[rep_outbound.len().saturating_sub(8)..];
    let avoid_note = if rep_outbound.is_empty() {
        String::new()
    } else {
        format!(
            "\nAVOID repeating or lightly rewording these prior rep messages:\n{}",
            rep_outbound
                .iter()
                .enumerate()
                .map(|(i, t)| format!("{}. {t}", i + 1))
                .collect::<Vec<_>>()
                .join("\n")
        )
    };
    let mode_note = mode_note(mode, custom);
    format!(
        "Draft the next {channel_label} message for sales rep {user_name} to send to this customer.\n{deal_focus}\n{length_note}\n{greeting_note}\n{signature_note}\n\n{mode_note}\n\n{latest_inbound_note}\n{direction_note}\n{promised_note}{quote_followed_up_note}\n{vague_note}\n{avoid_note}\n{style_section}\nCUSTOMER / PROJECT INFO:\n{}\n\n{quotes_block}\n\nCOMPANY ADMINISTRATIVE INSTRUCTIONS / GUARDRAILS:\n{instructions}\n\nFULL CONVERSATION — SMS and email combined, oldest first (times UTC):\n{}\n\nNOW: {} UTC\n\nUse the concrete facts above (chosen stone/color, quote, open next step). Return JSON with a single \"body\" field containing the draft message only. For email, that field is the body — never a Subject line.",
        if trim(profile).is_empty() {
            "(none)"
        } else {
            trim(profile)
        },
        format_timeline(timeline, now),
        now.format("%Y-%m-%d %H:%M"),
    )
}

/// What the draft is written from besides the thread and profile.
#[derive(Debug, Clone, Default)]
pub struct ReplyExtras {
    /// The customer's quotes ([`reply_context::format_quotes`]).
    pub quotes: String,
    /// When a quote was last emailed to the customer.
    pub last_quote_emailed: Option<chrono::DateTime<chrono::Utc>>,
    /// The rep's learned writing notes.
    pub rep_notes: Vec<String>,
    /// The rep's own recent messages and signature habit.
    pub voice: RepVoice,
}

/// A custom draft's request, and the revision of an earlier draft.
#[derive(Debug, Clone, Default)]
pub struct CustomDraft {
    pub request: String,
    pub previous_draft: String,
    pub revision_instruction: String,
    pub instruction_history: Vec<String>,
}

fn mode_note(mode: ReplyMode, custom: &CustomDraft) -> String {
    match mode {
        ReplyMode::FastReply => "REPLY MODE: Fast reply.\nPrefer a short, immediate reply to the latest customer message.\nKeep it tight. Acknowledge what they said, then one natural next step or question if needed.\nDo not write a long check-in or restart the conversation.".into(),
        ReplyMode::FollowUp => "REPLY MODE: Follow up.\nThe customer has not answered the latest rep message. Continue that specific topic.\nUse facts already in the thread and in QUOTES: the quote (and whether they opened it), the stones they named, the edge they chose, a discount, or a visit they cannot make.\nAsk one question about that open decision.\nDo not invent an agreement to reconnect. Never write \"as we agreed\" or \"reaching out as we agreed\" unless the thread shows they asked you to wait and you agreed.\nDo not ask how the project or renovation is coming along when a concrete next step is already open, unless a QUOTE ALREADY FOLLOWED UP note says the rep already chased it: then ask if the project is still on track.".into(),
        ReplyMode::Custom if !custom.previous_draft.is_empty() && !custom.revision_instruction.is_empty() => {
            let history: Vec<&str> = if custom.instruction_history.is_empty() {
                if custom.request.is_empty() { Vec::new() } else { vec![custom.request.as_str()] }
            } else {
                custom.instruction_history.iter().map(String::as_str).collect()
            };
            format!(
                "REPLY MODE: Custom revision.\nKeep the CURRENT DRAFT as the starting point. Apply the NEW INSTRUCTION to that draft.\nDo not start over unless the new instruction asks for a full rewrite.\nKeep wording that still fits. Honor every earlier instruction that still applies.\n\nCURRENT DRAFT:\n\"{}\"\n\nINSTRUCTIONS SO FAR (oldest first):\n{}\n\nNEW INSTRUCTION (apply this now):\n\"{}\"\n\nReturn a revised draft of the CURRENT DRAFT. Keep every point the draft and the instructions cover unless the new instruction removes it; the instructions set the length, not the usual length rules. Stay consistent with the conversation history.\nDo not invent facts the rep did not ask for.",
                custom.previous_draft,
                history.iter().enumerate().map(|(i, item)| format!("{}. {item}", i + 1)).collect::<Vec<_>>().join("\n"),
                custom.revision_instruction
            )
        }
        ReplyMode::Custom => format!(
            "REPLY MODE: Custom.\nThe sales rep gave this instruction for the draft:\n\"{}\"\nThe instruction is the content of the message. Cover EVERY point, detail, fact and question in it, in a natural order; drop none and do not shrink several points into one vague line. Turn the rep's notes into a clear, well-written message to the customer (fix grammar, keep their facts and names exactly).\nThe instruction sets the length, not the usual length rules: a short instruction gets a short message, a detailed one gets a complete message with every point. Several questions are fine when the rep asked them. On email, use short paragraphs when there are several points.\nStill sound natural and stay consistent with the conversation history.\nDo not ignore the thread. Do not invent facts the rep did not ask for.",
            custom.request
        ),
    }
}

/// Body of `POST /v1/ai/conversation-reply`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ConversationReplyInput {
    /// `sms` or `email`.
    pub channel: String,
    /// The customer.
    pub customer_id: Option<i32>,
    /// For an email to someone not on file yet.
    pub customer_email: Option<String>,
    /// The SMS thread's phone (5 to 15 digits).
    pub phone_digits: Option<String>,
    /// The deal to focus on.
    pub deal_id: Option<i64>,
    #[serde(default)]
    pub mode: ReplyMode,
    /// What the rep asks for (custom drafts, at most 2000 characters).
    pub custom_request: Option<String>,
    /// The draft to revise (at most 8000 characters).
    pub previous_draft: Option<String>,
    /// What to change in it (at most 2000 characters).
    pub revision_instruction: Option<String>,
    /// Every instruction so far, oldest first (at most 12).
    #[serde(default)]
    pub instruction_history: Vec<String>,
    /// Custom drafts: follow the company's instructions (default true).
    pub include_instructions: Option<bool>,
}

/// The draft.
#[derive(Debug, Serialize, ToSchema)]
pub struct ConversationReply {
    /// The message (an email's body, without a subject line).
    pub body: String,
}

fn too_long(value: Option<&str>, max: usize) -> bool {
    value.is_some_and(|v| v.chars().count() > max)
}

/// The request's channel, phone, email and custom draft, checked.
fn checked(
    input: &ConversationReplyInput,
) -> ApiResult<(FollowupChannel, Option<String>, String, bool, CustomDraft)> {
    let channel = FollowupChannel::parse(&input.channel)
        .ok_or_else(|| ApiError::BadRequest("Invalid request data".into()))?;
    if too_long(input.customer_email.as_deref(), 320)
        || too_long(input.custom_request.as_deref(), 2000)
        || too_long(input.previous_draft.as_deref(), 8000)
        || too_long(input.revision_instruction.as_deref(), 2000)
        || input.instruction_history.len() > 12
        || input
            .instruction_history
            .iter()
            .any(|i| i.chars().count() > 2000)
        || input.customer_id.is_some_and(|id| id <= 0)
        || input.deal_id.is_some_and(|id| id <= 0)
    {
        return Err(ApiError::BadRequest("Invalid request data".into()));
    }
    let phone = input.phone_digits.as_deref().map_or("", trim);
    let phone_digits = if phone.is_empty() {
        None
    } else if (5..=15).contains(&phone.len()) && phone.bytes().all(|b| b.is_ascii_digit()) {
        Some(phone.to_string())
    } else {
        return Err(ApiError::BadRequest("Invalid phone number".into()));
    };
    let email = trim(input.customer_email.as_deref().unwrap_or("")).to_lowercase();
    let has_email = channel == FollowupChannel::Email && email.contains('@');
    if input.customer_id.is_none() && phone_digits.is_none() && !has_email {
        return Err(ApiError::BadRequest("Customer or phone required".into()));
    }
    let custom = if input.mode == ReplyMode::Custom {
        CustomDraft {
            request: trim(input.custom_request.as_deref().unwrap_or("")).to_string(),
            previous_draft: trim(input.previous_draft.as_deref().unwrap_or("")).to_string(),
            revision_instruction: trim(input.revision_instruction.as_deref().unwrap_or(""))
                .to_string(),
            instruction_history: input
                .instruction_history
                .iter()
                .map(|i| trim(i).to_string())
                .filter(|i| !i.is_empty())
                .take(12)
                .collect(),
        }
    } else {
        CustomDraft::default()
    };
    if input.mode == ReplyMode::Custom && custom.request.is_empty() {
        return Err(ApiError::BadRequest("Custom request required".into()));
    }
    Ok((channel, phone_digits, email, has_email, custom))
}

/// The trimmed `body` of the model's JSON; empty when it has none.
fn reply_body(text: &str) -> String {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|v| {
            v.get("body")
                .and_then(|b| b.as_str())
                .map(|b| trim(b).to_string())
        })
        .unwrap_or_default()
}

/// The customer's quotes and the rep's voice. The quotes are best effort: a
/// failed lookup leaves them out rather than failing the draft.
async fn load_extras(
    state: &AppState,
    user: &crate::auth::session::SessionUser,
    customer_id: Option<i32>,
    deal_id: Option<i64>,
    gathered: &crate::followups::gather::GatheredContext,
    now: chrono::DateTime<chrono::Utc>,
) -> ReplyExtras {
    let (quotes, last_quote_emailed) = match customer_id {
        Some(customer_id) => {
            match reply_context::load_quotes(&state.pool, user.company_id, customer_id, deal_id)
                .await
            {
                Ok(quotes) => (
                    reply_context::format_quotes(&quotes, deal_id, now),
                    reply_context::last_quote_emailed(&quotes),
                ),
                Err(err) => {
                    tracing::warn!(error = %err, customer_id, "AI reply: quotes unavailable");
                    (String::new(), None)
                }
            }
        }
        None => (String::new(), None),
    };
    ReplyExtras {
        quotes,
        last_quote_emailed,
        rep_notes: gathered
            .style_profile
            .as_ref()
            .and_then(|p| p.notes.clone())
            .unwrap_or_default(),
        voice: gathered.rep_voice.clone(),
    }
}

/// Draft the next message
///
/// Drafts the next SMS or email to a customer from the whole conversation
/// (SMS and email together), the customer's profile, deals and quotes
/// (colors, totals, sent / opened / signed), the company's AI instructions
/// and the rep's own recent messages: a fast reply, a follow-up on the open
/// topic, or a custom draft (and revisions of it). The draft greets the
/// customer by name until we have sent them an SMS or email today (store
/// time); after that it has no greeting and no name. Takes several seconds (a larger model).
#[crate::openapi::path(
    post,
    path = "/v1/ai/conversation-reply",
    operation_id = "draft_conversation_reply",
    tag = "ai",
    request_body = ConversationReplyInput,
    responses(
        (status = 200, description = "The draft", body = ConversationReply),
        (status = 400, description = "Invalid request, phone, or no customer, phone or email", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such customer in the caller's company", body = ErrorBody),
        (status = 500, description = "The model wrote nothing", body = ErrorBody),
        (status = 502, description = "The model failed", body = ErrorBody),
        (status = 503, description = "AI is not configured", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn conversation_reply(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<ConversationReplyInput>,
) -> ApiResult<Json<ConversationReply>> {
    let body = draft(&state, &user, &input, "quick_reply").await?;
    Ok(Json(ConversationReply { body }))
}

/// The AI Reply's draft for `user`, its AI calls logged as `feature` (the
/// promise job's ready messages are custom drafts too).
pub async fn draft(
    state: &AppState,
    user: &crate::auth::session::SessionUser,
    input: &ConversationReplyInput,
    feature: &'static str,
) -> ApiResult<String> {
    let (channel, phone_digits, email, has_email, custom) = checked(input)?;
    if let Some(id) = input.customer_id {
        let found: Option<i32> = sqlx::query_scalar(
            "SELECT id FROM customers WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
        )
        .bind(id)
        .bind(user.company_id)
        .fetch_optional(&state.pool)
        .await?;
        found.ok_or(ApiError::NotFound)?;
    }
    let mut request = FollowupRequest::new(user.company_id, user.id, channel);
    request.customer_id = input.customer_id;
    request.phone_digits = phone_digits;
    request.customer_email = has_email.then(|| email.clone());
    request.preferred_deal_id = input.deal_id;
    let now = chrono::Utc::now();
    let gathered = gather_followup_context(&state.pool, &request, now).await?;
    let include_instructions =
        input.mode != ReplyMode::Custom || input.include_instructions != Some(false);
    let extras = load_extras(
        state,
        user,
        gathered.customer_id,
        input.deal_id,
        &gathered,
        now,
    )
    .await;
    let user_prompt = build_user_prompt(
        channel,
        user.name.as_deref().unwrap_or(""),
        &gathered.profile,
        if include_instructions {
            &gathered.company_instructions
        } else {
            ""
        },
        &gathered.timeline,
        input.deal_id,
        input.mode,
        &custom,
        &extras,
        now,
    );
    let caller = Caller {
        feature,
        user_id: Some(user.id),
        company_id: Some(user.company_id),
    };
    let chat = |model, timeout| {
        let mut chat = Chat::new(model)
            .message("system", CONVERSATION_REPLY_SYSTEM_PROMPT)
            .message("user", &user_prompt);
        chat.json = true;
        chat.timeout = timeout;
        chat.prompt_cache_key = Some(format!("conversation-reply:{}", user.company_id));
        chat
    };
    let mut large = chat(REPLY_MODEL, REPLY_MODEL_TIMEOUT);
    large.reasoning_effort = Some(REPLY_REASONING_EFFORT);
    // The large model first; on any failure (no access, overloaded, too slow)
    // or an empty draft, the quick model so the button still answers.
    let mut body = match state.ai.chat(&state.pool, caller, &large).await {
        Ok(text) => reply_body(&text),
        Err(AiError::NotConfigured) => {
            return Err(ApiError::Unavailable(AiError::NotConfigured.to_string()));
        }
        Err(err) => {
            tracing::warn!(error = %err, model = REPLY_MODEL, "AI reply falling back to the quick model");
            String::new()
        }
    };
    if body.is_empty() {
        let text = match state
            .ai
            .chat(&state.pool, caller, &chat(MINI_MODEL, FALLBACK_TIMEOUT))
            .await
        {
            Err(err) if err.is_model_permission() => {
                state
                    .ai
                    .chat(
                        &state.pool,
                        caller,
                        &chat(REPLY_FALLBACK_MODEL, FALLBACK_TIMEOUT),
                    )
                    .await
            }
            result => result,
        }
        .map_err(|err| match err {
            AiError::NotConfigured => ApiError::Unavailable(err.to_string()),
            other => ApiError::Upstream(format!("Failed to generate AI reply: {other}")),
        })?;
        body = reply_body(&text);
    }
    if channel == FollowupChannel::Email {
        body =
            trim(&re!(r"^(?:Subject:\s*[^\n\r]*(?:\r?\n)+)+", "iu").replace(&body, "")).to_string();
    }
    if channel == FollowupChannel::Sms {
        body = js_slice(
            trim(&re!(r"\s+", "g").replace_all(&body, " ")),
            0,
            SMS_MAX_TEXT,
        );
    }
    if body.is_empty() {
        crate::posthog::capture(
            &state.telephony_http,
            "ai_conversation_reply_empty",
            "server-spa",
            serde_json::json!({ "companyId": user.company_id, "channel": channel.as_str(), "mode": input.mode, "$process_person_profile": false }),
        )
        .await;
        return Err(ApiError::Internal("Failed to generate AI reply".into()));
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(at: &str, direction: Direction, text: &str) -> FollowupTimelineMessage {
        FollowupTimelineMessage {
            at: at.into(),
            channel: FollowupChannel::Sms,
            direction,
            text: text.into(),
            from_email: None,
            from_phone: None,
        }
    }

    fn lenora() -> Vec<FollowupTimelineMessage> {
        use Direction::{Inbound as I, Outbound as O};
        vec![
            message(
                "2026-07-24 12:21",
                O,
                "Hi Lenora, this is Dema from Granite Depot. You reached out about new countertops — is there a good time for a call?",
            ),
            message(
                "2026-08-10 12:52",
                O,
                "Hi Lenora, just wanted to see if you got my last message.",
            ),
            message(
                "2026-08-26 17:15",
                O,
                "Hi Lenora, are you still hoping to start the kitchen update soon?",
            ),
            message("2026-08-26 17:29", I, "I'm still hoping to start soon"),
            message(
                "2026-08-26 17:38",
                O,
                "Got it. What does soon look like — next few weeks, or does something else need to happen first?",
            ),
            message(
                "2026-08-26 18:12",
                I,
                "We are about to have a baby in a week and then we should be able to get started",
            ),
            message(
                "2026-08-26 18:23",
                O,
                "Congratulations! Should I text you in 7-10 days?",
            ),
            message("2026-08-26 18:25", I, "10-12 days would be best"),
            message("2026-08-26 18:25", O, "Great, will do"),
        ]
    }

    fn prompt(
        timeline: &[FollowupTimelineMessage],
        mode: ReplyMode,
        custom: &CustomDraft,
    ) -> String {
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-13T15:00:00Z")
            .unwrap()
            .to_utc();
        build_user_prompt(
            FollowupChannel::Sms,
            "Dema",
            "Name: Lenora",
            "",
            timeline,
            None,
            mode,
            custom,
            &ReplyExtras::default(),
            now,
        )
    }

    #[test]
    fn an_agreed_later_check_in_is_followed_through() {
        let timeline = lenora();
        assert!(conversation_has_promised_follow_up(&timeline));
        assert!(!conversation_has_promised_follow_up(&timeline[..6]));
        let text = prompt(&timeline, ReplyMode::FastReply, &CustomDraft::default());
        assert!(
            text.contains("reached out as agreed") && text.contains("how the project is going")
        );
        assert!(text.contains("10-12 days would be best") && text.contains("Great, will do"));
        assert!(!text.contains("This is a discovery moment"));
        assert!(text.contains("REPLY MODE: Fast reply"));
        let follow_up = prompt(&timeline, ReplyMode::FollowUp, &CustomDraft::default());
        assert!(
            follow_up.contains("REPLY MODE: Follow up")
                && follow_up.contains("reached out as agreed")
        );
    }

    #[test]
    fn a_short_vague_reply_asks_softly() {
        let timeline = vec![
            message(
                "2026-08-26 17:15",
                Direction::Outbound,
                "Hi Valeria. Are you still planning the island countertop, or has the project been pushed back?",
            ),
            message("2026-08-26 17:29", Direction::Inbound, "Pushed back"),
        ];
        let text = prompt(&timeline, ReplyMode::FastReply, &CustomDraft::default());
        assert!(text.contains("NOTE: Customer's last message is short/vague (\"Pushed back\")."));
        assert!(text.contains("NOTE: The last message was from the customer."));
    }

    #[test]
    fn custom_drafts_and_revisions() {
        let custom = CustomDraft {
            request: "Ask about quartzite timing only".into(),
            ..CustomDraft::default()
        };
        let text = prompt(&lenora(), ReplyMode::Custom, &custom);
        assert!(
            text.contains("REPLY MODE: Custom.")
                && text.contains("Ask about quartzite timing only")
        );
        assert!(
            text.contains("Cover EVERY point")
                && !text.contains("1–3 short sentences. One main question")
        );
        let revision = CustomDraft {
            request: "Price 1-2 white quartzite options".into(),
            previous_draft: "Hi! I can send pricing".into(),
            revision_instruction: "Mention Calacatta Karmelo".into(),
            instruction_history: vec![
                "Price 1-2 white quartzite options".into(),
                "Mention Calacatta Karmelo".into(),
            ],
        };
        let text = prompt(&lenora(), ReplyMode::Custom, &revision);
        assert!(
            text.contains("REPLY MODE: Custom revision")
                && text.contains("CURRENT DRAFT:\n\"Hi! I can send pricing\"")
        );
        assert!(text.contains("INSTRUCTIONS SO FAR (oldest first):\n1. Price 1-2 white quartzite options\n2. Mention Calacatta Karmelo"));
        assert!(!text.contains("Do not ignore the thread."));
        assert!(CONVERSATION_REPLY_SYSTEM_PROMPT.contains("reaching out as we agreed"));
        assert!(CONVERSATION_REPLY_SYSTEM_PROMPT.contains("How is the project going?"));
    }

    #[test]
    fn our_message_from_today_on_any_channel_skips_the_greeting() {
        // The customer wrote this morning (5:00 AM in Indianapolis) and we
        // have not answered today: our reply greets her.
        let asked = vec![message(
            "2026-10-13 09:00",
            Direction::Inbound,
            "Can you send the price for the island top please?",
        )];
        let text = prompt(&asked, ReplyMode::FastReply, &CustomDraft::default());
        assert!(text.contains("GREETING: BY NAME") && !text.contains("GREETING: NONE"));
        // Once we answered today, her next question continues the thread.
        let mut answered = asked.clone();
        answered.push(message(
            "2026-10-13 09:30",
            Direction::Outbound,
            "Sure, one sec",
        ));
        answered.push(message("2026-10-13 09:40", Direction::Inbound, "Thanks!"));
        let text = prompt(&answered, ReplyMode::FastReply, &CustomDraft::default());
        assert!(text.contains("GREETING: NONE"));
        assert!(text.contains("Do NOT open with a greeting"));
        assert!(text.contains("do NOT use the customer's name"));
        // The rep just texted; an email draft still continues without "Hi".
        let mut texted = lenora();
        texted.push(message(
            "2026-10-13 14:40",
            Direction::Outbound,
            "Just sent the quote",
        ));
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-13T15:00:00Z")
            .unwrap()
            .to_utc();
        let email = build_user_prompt(
            FollowupChannel::Email,
            "Dema",
            "Name: Lenora",
            "",
            &texted,
            None,
            ReplyMode::FollowUp,
            &CustomDraft::default(),
            &ReplyExtras::default(),
            now,
        );
        assert!(email.contains("GREETING: NONE") && email.contains("20 minutes ago"));
        // Lenora's last message was 48 days ago: greet by name.
        assert!(
            prompt(&lenora(), ReplyMode::FastReply, &CustomDraft::default())
                .contains("GREETING: BY NAME")
        );
        // Yesterday evening (6:30 PM in Indianapolis), or no history at all:
        // greet by name.
        let old = vec![message(
            "2026-10-12 22:30",
            Direction::Inbound,
            "Can you send the price for the island top please?",
        )];
        let text = prompt(&old, ReplyMode::FastReply, &CustomDraft::default());
        assert!(text.contains("GREETING: BY NAME") && !text.contains("GREETING: NONE"));
        assert!(
            prompt(&[], ReplyMode::FastReply, &CustomDraft::default())
                .contains("GREETING: BY NAME")
        );
    }

    #[test]
    fn quotes_voice_and_signature_reach_the_prompt() {
        let extras = ReplyExtras {
            quotes: "Quote \"Kitchen\" (#4, status active)\n  Option 1: Kitchen: Quartz Viraldi · total $3,100.00".into(),
            rep_notes: vec!["Opens with 'Hey'".into()],
            voice: RepVoice {
                examples: vec!["Hey! Slabs are in, want to come see them Thursday?".into()],
                has_email_signature: false,
                email_name: Some("Dema Petrova".into()),
            },
            last_quote_emailed: None,
        };
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-13T15:00:00Z")
            .unwrap()
            .to_utc();
        let email = build_user_prompt(
            FollowupChannel::Email,
            "Dema",
            "Name: Lenora",
            "Always mention free sink cutouts",
            &lenora(),
            Some(9),
            ReplyMode::FollowUp,
            &CustomDraft::default(),
            &extras,
            now,
        );
        assert!(email.contains(
            "QUOTES (this customer's quotes, newest first; times UTC):\nQuote \"Kitchen\""
        ));
        assert!(email.contains("total $3,100.00"));
        assert!(email.contains("HOW DEMA WRITES"));
        assert!(
            email.contains("RECENT MESSAGES BY THE REP") && email.contains("- Hey! Slabs are in")
        );
        assert!(email.contains("SIGNATURE: No signature is added") && email.contains("\"Dema\""));
        assert!(email.contains("Always mention free sink cutouts"));
        assert!(email.contains("NOW: 2026-10-13 15:00 UTC"));
        let signed = ReplyExtras {
            voice: RepVoice {
                has_email_signature: true,
                ..RepVoice::default()
            },
            ..ReplyExtras::default()
        };
        let email = build_user_prompt(
            FollowupChannel::Email,
            "Dema",
            "",
            "",
            &[],
            None,
            ReplyMode::FastReply,
            &CustomDraft::default(),
            &signed,
            now,
        );
        assert!(email.contains("signature is added automatically"));
        assert!(email.contains("QUOTES: (none on file for this customer)"));
        assert!(!email.contains("RECENT MESSAGES BY THE REP"));
        assert!(
            prompt(&lenora(), ReplyMode::FastReply, &CustomDraft::default())
                .contains("SIGNATURE: none — texts never end")
        );
    }

    #[test]
    fn a_quote_already_chased_asks_if_the_project_is_on_track() {
        use Direction::{Inbound as I, Outbound as O};
        let at = |s: &str| chrono::DateTime::parse_from_rfc3339(s).unwrap().to_utc();
        let timeline = vec![
            message(
                "2026-10-06 16:00",
                O,
                "The quote was sent on September 30th.",
            ),
            message(
                "2026-10-07 11:13",
                I,
                "I see now that I received the quote.",
            ),
            message(
                "2026-10-12 14:30",
                O,
                "Have you had a chance to review the quote?",
            ),
        ];
        let extras = ReplyExtras {
            last_quote_emailed: Some(at("2026-09-30T21:35:00Z")),
            ..ReplyExtras::default()
        };
        let build = |now| {
            build_user_prompt(
                FollowupChannel::Email,
                "Dema",
                "",
                "",
                &timeline,
                None,
                ReplyMode::FollowUp,
                &CustomDraft::default(),
                &extras,
                now,
            )
        };
        let text = build(at("2026-10-26T19:45:00Z"));
        assert!(text.contains("NOTE: QUOTE ALREADY FOLLOWED UP"));
        assert!(text.contains("got the quote 25 days ago") && text.contains("(14 days ago)"));
        assert!(text.contains("still on track"));
        // Every message shows how old it is.
        assert!(text.contains("[2026-10-12 14:30, 14 days ago] SMS Sales Rep"));
        assert!(text.contains("LATEST CUSTOMER MESSAGE (SMS at 2026-10-07 11:13, 19 days ago)"));
        // A day after the nudge it is too early; without a sent quote there is none.
        assert!(!build(at("2026-10-13T15:00:00Z")).contains("NOTE: QUOTE ALREADY FOLLOWED UP"));
        let no_quote = build_user_prompt(
            FollowupChannel::Email,
            "Dema",
            "",
            "",
            &timeline,
            None,
            ReplyMode::FollowUp,
            &CustomDraft::default(),
            &ReplyExtras::default(),
            at("2026-10-26T19:45:00Z"),
        );
        assert!(!no_quote.contains("NOTE: QUOTE ALREADY FOLLOWED UP"));
    }

    #[test]
    fn the_system_prompt_does_not_anchor_a_greeting() {
        let p = CONVERSATION_REPLY_SYSTEM_PROMPT;
        assert!(p.contains("GREETING: NONE") && p.contains("GREETING: BY NAME"));
        assert!(p.contains("at most once"));
        // GOOD examples open without a greeting except after a 60+ day gap,
        // and the examples model plain writing: no em dashes in them.
        let goods: Vec<&str> = p.lines().filter(|l| l.starts_with("GOOD: \"")).collect();
        assert!(goods.len() >= 4);
        assert_eq!(
            goods.iter().filter(|l| l.starts_with("GOOD: \"Hi")).count(),
            1
        );
        assert!(goods.iter().all(|l| !l.contains('—')));
        assert!(p.contains("two weeks later (GREETING: NONE"));
        assert!(p.contains("\"Reaching out as we agreed. How is the project going?\""));
        assert!(p.contains("\"Here's the island quote in Calacatta Karmelo"));
        assert!(reply_body(r#"{"body":"  Sounds good  "}"#) == "Sounds good");
        assert_eq!(reply_body("not json"), "");
    }
}
