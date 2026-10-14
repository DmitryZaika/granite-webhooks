//! `POST /v1/ai/conversation-reply`: the AI Reply button.
//!
//! Drafts the next SMS or email from the whole conversation (what the
//! follow-up writer reads): a fast reply, a follow-up, or a custom draft and
//! its revisions. Port of the Remix `api.aiRecommend.conversationReply`
//! route and `aiConversationReply.server.ts`.

use crate::ai::{AiError, Caller, Chat, MINI_MODEL};
use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::followups::gather::{FollowupRequest, gather_followup_context};
use crate::followups::js::{js_len, js_slice, trim};
use crate::followups::prompt::FollowupTimelineMessage;
use crate::followups::send_window::FollowupChannel;
use crate::followups::timeline::Direction;
use crate::re;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// The model when the main one is not available to the key.
pub const REPLY_FALLBACK_MODEL: &str = "gpt-5.6-luna";
const SMS_MAX_TEXT: usize = 1600;

pub const CONVERSATION_REPLY_SYSTEM_PROMPT: &str = r#"You help a countertop sales rep draft the next SMS/email to a customer.

PRIMARY TASK — write the next message this conversation itself calls for:
1. Read the actual thread. Use what they already said: timing, wait periods, promised callbacks, the project, and personal situation.
2. If the customer just wrote: reply to that message.
3. If the last message is from the rep: write the next outreach the thread requires now — especially a follow-through you already promised.
4. Keep the conversation going with one natural question when it helps.

Promised follow-up is the next message, not a new intro:
Use "reaching out as we agreed" ONLY when the customer asked you to wait and you agreed ("10-12 days would be best" / "Great, will do").
Then ask how that specific situation is going.
If nobody agreed on a later check-in, never write "as we agreed", "reaching out as we agreed", or a generic "how is the project / renovation coming along?"
Continue the open topic instead: the quote they have not answered, the slabs they are choosing between, the edge, or the discount.
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
GOOD next message:
"Hi Lenora — reaching out as we agreed. How is the project going?"
BAD next message:
"Hi Lenora, you reached out about new countertops. Is there a good time for a call?"

No agreement in the thread (quote sent, customer chose an edge, named two slabs, then went quiet):
BAD: "Hi Ahoura, reaching out as we agreed. How is the kitchen renovation coming along?"
GOOD: "Hi Ahoura, did you get a chance to look over the quote? Between Viraldi and Idillio, which one are you leaning toward?"

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
- Write in first person (I/we). No signature unless clearly needed.
- SMS: 1–3 short sentences. Email: still concise.
- Email body is the message only. Never prefix it with "Subject:" — the thread already has a subject.

Output ONLY valid JSON: {"body":"draft message"}"#;

fn format_timeline(messages: &[FollowupTimelineMessage]) -> String {
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
                m.at,
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
        |m| format!("LATEST CUSTOMER MESSAGE ({} at {}):\n{}", m.channel.as_str().to_uppercase(), m.at, m.text),
    );
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
    // The server's day is UTC, like the timeline's times.
    let sent_today =
        last.is_some_and(|m| m.at.get(..10) == Some(now.format("%Y-%m-%d").to_string().as_str()));
    let greeting_note = if sent_today {
        "NOTE: The last message was sent today. Do NOT start with \"Hi\", \"Hello\", or \"Dear\". Jump straight into the reply."
    } else {
        ""
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
    let length_note = if channel == FollowupChannel::Sms {
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
        "Draft the next {channel_label} message for sales rep {user_name} to send to this customer.\n{deal_focus}\n{length_note}\n\n{mode_note}\n\n{latest_inbound_note}\n{direction_note}\n{greeting_note}\n{promised_note}\n{vague_note}\n{avoid_note}\n\nCUSTOMER / PROJECT INFO:\n{}\n\nCOMPANY ADMINISTRATIVE INSTRUCTIONS / GUARDRAILS:\n{instructions}\n\nFULL CONVERSATION — SMS and email combined, oldest first:\n{}\n\nReturn JSON with a single \"body\" field containing the draft message only. For email, that field is the body — never a Subject line.",
        if trim(profile).is_empty() {
            "(none)"
        } else {
            trim(profile)
        },
        format_timeline(timeline),
    )
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
        ReplyMode::FollowUp => "REPLY MODE: Follow up.\nThe customer has not answered the latest rep message. Continue that specific topic.\nUse facts already in the thread: the quote, the stones they named, the edge they chose, a discount, or a visit they cannot make.\nAsk one question about that open decision.\nDo not invent an agreement to reconnect. Never write \"as we agreed\" or \"reaching out as we agreed\" unless the thread shows they asked you to wait and you agreed.\nDo not ask how the project or renovation is coming along when a concrete next step is already open.".into(),
        ReplyMode::Custom if !custom.previous_draft.is_empty() && !custom.revision_instruction.is_empty() => {
            let history: Vec<&str> = if custom.instruction_history.is_empty() {
                if custom.request.is_empty() { Vec::new() } else { vec![custom.request.as_str()] }
            } else {
                custom.instruction_history.iter().map(String::as_str).collect()
            };
            format!(
                "REPLY MODE: Custom revision.\nKeep the CURRENT DRAFT as the starting point. Apply the NEW INSTRUCTION to that draft.\nDo not start over unless the new instruction asks for a full rewrite.\nKeep wording that still fits. Honor every earlier instruction that still applies.\n\nCURRENT DRAFT:\n\"{}\"\n\nINSTRUCTIONS SO FAR (oldest first):\n{}\n\nNEW INSTRUCTION (apply this now):\n\"{}\"\n\nReturn a revised draft of the CURRENT DRAFT. Stay consistent with the conversation history.\nDo not invent facts the rep did not ask for.",
                custom.previous_draft,
                history.iter().enumerate().map(|(i, item)| format!("{}. {item}", i + 1)).collect::<Vec<_>>().join("\n"),
                custom.revision_instruction
            )
        }
        ReplyMode::Custom => format!(
            "REPLY MODE: Custom.\nThe sales rep gave this instruction for the draft:\n\"{}\"\nFollow that instruction closely while still sounding natural and staying consistent with the conversation history.\nDo not ignore the thread. Do not invent facts the rep did not ask for.",
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

/// Draft the next message
///
/// Drafts the next SMS or email to a customer from the whole conversation:
/// a fast reply, a follow-up on the open topic, or a custom draft (and
/// revisions of it).
#[utoipa::path(
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
    let (channel, phone_digits, email, has_email, custom) = checked(&input)?;
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
        now,
    );
    let caller = Caller {
        feature: "quick_reply",
        user_id: Some(user.id),
        company_id: Some(user.company_id),
    };
    let chat = |model| {
        let mut chat = Chat::new(model)
            .message("system", CONVERSATION_REPLY_SYSTEM_PROMPT)
            .message("user", &user_prompt);
        chat.json = true;
        chat
    };
    let text = match state.ai.chat(&state.pool, caller, &chat(MINI_MODEL)).await {
        Err(err) if err.is_model_permission() => {
            state
                .ai
                .chat(&state.pool, caller, &chat(REPLY_FALLBACK_MODEL))
                .await
        }
        result => result,
    }
    .map_err(|err| match err {
        AiError::NotConfigured => ApiError::Unavailable(err.to_string()),
        other => ApiError::Upstream(format!("Failed to generate AI reply: {other}")),
    })?;
    let mut body = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| {
            v.get("body")
                .and_then(|b| b.as_str())
                .map(|b| trim(b).to_string())
        })
        .unwrap_or_default();
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
    Ok(Json(ConversationReply { body }))
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
    fn a_message_sent_today_skips_the_greeting() {
        let timeline = vec![message(
            "2026-10-13 09:00",
            Direction::Inbound,
            "Can you send the price for the island top please?",
        )];
        assert!(
            prompt(&timeline, ReplyMode::FastReply, &CustomDraft::default())
                .contains("Do NOT start with \"Hi\"")
        );
        let old = vec![message(
            "2026-10-12 09:00",
            Direction::Inbound,
            "Can you send the price for the island top please?",
        )];
        assert!(
            !prompt(&old, ReplyMode::FastReply, &CustomDraft::default())
                .contains("Do NOT start with \"Hi\"")
        );
    }
}
