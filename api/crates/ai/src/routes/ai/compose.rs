//! Inline autocomplete in the SMS / email composer, and what the rep's sent
//! messages teach it.
//!
//! - `GET  /v1/ai/compose/inventory-url` → the customer stones page the rep may link
//! - `POST /v1/ai/compose/autocomplete` → the next few words of the draft
//!   (or how the next message starts); `warm` loads the thread ahead
//! - `POST /v1/ai/compose/learn` → the rep sent something else than suggested:
//!   update their writing notes
//!
//! Port of the CRM's `api.aiCompose.autocomplete` route
//! (`aiComposeAutocomplete.server.ts`, `repWritingNotes*.ts`).

use crate::ai::{AiError, Caller, Chat, MINI_MODEL};
use crate::auth::EmployeeUser;
use crate::auth::session::SessionUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::followups::gather::{
    FollowupRequest, customer_inventory_page_url, customer_quote_page_url, gather_followup_context,
};
use crate::followups::greeting::{conversation_recency, greeting_instruction};
use crate::followups::js::{js_len, js_slice, trim};
use crate::followups::prompt::FollowupTimelineMessage;
use crate::followups::send_window::FollowupChannel;
use crate::openapi::ToSchema;
use crate::routes::ai::conversation_reply::REPLY_FALLBACK_MODEL;
use crate::routes::ai::reply_context;
use crate::routes::sms::provider::valid_thread_phone;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use chrono::{DateTime, Utc};
use lambda_http::tracing;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

/// About 1,000 tokens of conversation.
const TIMELINE_MAX_CHARS: usize = 4_000;
const MAX_OUTPUT_TOKENS: u32 = 60;
/// A whole first message to a new lead.
const FIRST_MESSAGE_MAX_OUTPUT_TOKENS: u32 = 160;
/// The gathered thread is reused while the rep keeps typing.
const CONTEXT_TTL: Duration = Duration::from_secs(60);
const CONTEXT_MAX_ENTRIES: usize = 200;
const LEARN_MAX_CHARS: usize = 2000;
const COMPOSE_EDIT_PHASE: &str = "compose";
pub const REP_NOTES_MAX: usize = 10;
pub const REP_NOTE_MAX_CHARS: usize = 140;
/// Recent suggestion → sent pairs the note writer looks at.
const REP_NOTES_EVIDENCE_PAIRS: usize = 20;
/// Recent pairs read to find those evidence pairs among the rewrites.
const REP_NOTES_CANDIDATE_PAIRS: i64 = 60;
/// Each side of an evidence pair, so 20 long emails stay a small prompt.
const REP_NOTES_PAIR_MAX_CHARS: usize = 700;
/// Share of the suggestion's words the sent message must keep to count as
/// an edit of it; below that the rep wrote a different message.
const MIN_EDIT_OVERLAP: f64 = 0.4;

pub const COMPOSE_AUTOCOMPLETE_SYSTEM_PROMPT: &str = r#"You are inline autocomplete for a countertop sales rep typing a message to a customer.
Continue the rep's DRAFT from exactly where it stops. Predict only the next few words, at most the end of the current sentence (max ~15 words).
The rep has already typed the DRAFT. Your text is glued directly onto its end, so DRAFT + your text must read as one natural message.
Rules:
- Return ONLY the text to append. Never repeat, rewrite, or restart the draft or its current sentence.
- If the draft ends mid-word, finish THAT word first, keeping the letters already typed (no leading space). If it ends after a full word, start with a space.
- Stay consistent with the conversation and the rep's tone. Never invent prices, dates, or facts not in the context.
- When the rep is about to share the inventory / stones page, use the exact customer page URL from CUSTOMER / PROJECT INFO.
- When the rep asks the customer for their measurements or sends them a link to send us their sizes, use the exact Customer quote URL from CUSTOMER / PROJECT INFO.
- When the message needs the rep's name, the rep's phone number or the company address, use them from SALES REP and COMPANY.
- If the DRAFT is empty, suggest how the rep's next message should start: one short sentence (max ~20 words) that fits where the conversation is now. Follow the GREETING line: with GREETING: NONE (an SMS or email with them earlier today) never start with "Hi", "Hey" or "Hello" and never use the customer's name; start with the content.
- Never repeat or rephrase something the rep already said in the CONVERSATION. If the rep's last message is still unanswered, don't ask its question again: add something new (a different angle, an offer, a next step) or return an empty string.
- Use the customer's name at most once in a message. Write plainly like the rep: no "I hope this finds you well", "Just following up", "I wanted to reach out", no em dashes.
- Stop once the point is made. Never tack on an extra offer, a promise to follow up or send something, or "let me know if you have any questions" unless the rep's own messages end that way: reps delete these more than anything else.
- Sound like the rep's notes (HOW … WRITES) and RECENT MESSAGES BY THE REP: their length, wording, greeting, and the name they introduce themselves with (it can differ from SALES REP).
- If the DRAFT is empty and the customer has not written to us yet, suggest the whole message (1–3 short sentences) built from CUSTOMER / PROJECT INFO: who the rep is and the company (unless already said on this channel), what the customer asked for, and one question that moves the project forward.
- When the customer asked something, answer it with the facts in CUSTOMER / PROJECT INFO and QUOTES (the sink, color, edge, price, what is included). Only when a fact is not there may the rep say they'll check.
- If nothing useful fits, return an empty string.
Examples (DRAFT → your output):
"I'll be soo" → "n, see you Friday at 5:00."
"See you Fri" → "day at 5:00 PM."
"Thanks for " → "getting back to me!"
"Would you like me to bring" → " a few samples?""#;

pub const REP_NOTES_SYSTEM_PROMPT: &str = r#"You keep short notes on how one countertop sales rep writes to customers, so the AI that suggests their messages sounds like them.
You get the current notes and recent examples of what the AI suggested and what the rep actually sent instead.
Return JSON {"notes": [...]}: at most 10 notes, each one concrete habit under 140 characters, quoting the rep's own words when that helps (e.g. "Opens with 'Hey', not 'Hi'", "Signs off as 'Dema'", "Says 'template', never 'measure'").
Most useful first: the name the rep introduces themselves with (it can differ from REP), how they open and close, what they keep cutting from suggestions, typical length, recurring phrases.
Notes are about how the rep writes and what they choose to say, never facts about one customer or project, and never vague ("Keeps messages concise") when a concrete habit fits.
A habit seen only in texts starts with "SMS: ", only in emails with "Email: "; habits seen in both have no prefix.
Keep notes the examples still support, merge overlapping ones, drop ones the rep now contradicts. If the examples teach nothing new, return the current notes unchanged."#;

/// What the prompt is built from, cached per thread for a minute.
#[derive(Debug, Clone)]
pub struct ComposeContext {
    profile: String,
    timeline: Vec<FollowupTimelineMessage>,
    rep_phone: Option<String>,
    company_name: Option<String>,
    company_address: Option<String>,
    rep_notes: Vec<String>,
    /// The rep's own recent messages to any customer.
    rep_voice: Vec<String>,
    /// The customer's quotes, formatted (empty: none).
    quotes: String,
}

/// Recently gathered threads (`warm` fills it).
pub type ComposeContexts =
    std::sync::Mutex<std::collections::HashMap<String, (Instant, ComposeContext)>>;

/// The newest messages that fit in `max_chars` (UTF-16, like the CRM),
/// oldest first; the oldest kept one may be cut at its start.
pub fn trim_timeline(
    messages: &[FollowupTimelineMessage],
    max_chars: usize,
) -> Vec<FollowupTimelineMessage> {
    let mut total = 0;
    let mut kept = Vec::new();
    for message in messages.iter().rev() {
        let remaining = max_chars.saturating_sub(total);
        if remaining == 0 {
            break;
        }
        let length = js_len(&message.text);
        let text = if length > remaining {
            format!("…{}", js_slice(&message.text, length - remaining, length))
        } else {
            message.text.clone()
        };
        total += js_len(&text);
        kept.push(FollowupTimelineMessage {
            text,
            ..message.clone()
        });
    }
    kept.reverse();
    kept
}

/// The customer has not written to us yet.
pub fn customer_has_not_replied(timeline: &[FollowupTimelineMessage]) -> bool {
    !timeline
        .iter()
        .any(|m| m.direction == crate::followups::timeline::Direction::Inbound)
}

/// Same clock and format as the timeline (UTC, minutes).
pub fn autocomplete_now(now: DateTime<Utc>) -> String {
    now.format("%Y-%m-%d %H:%M").to_string()
}

static PARTIAL_WORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\p{L}']+$").expect("valid pattern"));
static FIRST_WORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[\p{L}']+").expect("valid pattern"));
static CODE_FENCE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^```[a-z]*\n?|\n?```$").expect("valid pattern"));
static QUOTED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^\s*"([\s\S]*)"\s*$"#).expect("valid pattern"));
/// A prompt label the model echoed ("GREETING: Hello Ann").
static PROMPT_LABEL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(?:GREETING|DRAFT)\s*:\s*").expect("valid pattern"));
static CHANNEL_NOTE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(sms|email)\s*:\s*").expect("valid pattern"));

fn draft_tail_note(draft: &str) -> String {
    if let Some(partial) = PARTIAL_WORD.find(draft) {
        let partial = partial.as_str();
        return format!(
            "\nThe draft ends mid-word or right after the word \"{partial}\". If it is unfinished, complete it (keep \"{partial}\", add only the missing letters)."
        );
    }
    if draft.ends_with(char::is_whitespace) {
        "\nThe draft ends with a space: start with the next word.".into()
    } else {
        String::new()
    }
}

/// The rep's notes block of the prompt (`formatRepNotesBlock`): shared notes
/// and those for this channel ("SMS: …", "Email: …").
pub fn format_rep_notes_block(
    rep_name: &str,
    notes: &[String],
    channel: FollowupChannel,
) -> String {
    let lines: Vec<String> = notes
        .iter()
        .filter_map(|note| {
            CHANNEL_NOTE.captures(note).map_or_else(
                || Some(note.clone()),
                |prefix| {
                    prefix[1]
                        .eq_ignore_ascii_case(channel.as_str())
                        .then(|| note[prefix[0].len()..].to_string())
                },
            )
        })
        .map(|note| format!("- {note}"))
        .collect();
    if lines.is_empty() {
        return String::new();
    }
    format!(
        "HOW {} WRITES (learned from what they change in suggestions — write like this):\n{}",
        rep_name.to_uppercase(),
        lines.join("\n")
    )
}

/// What the model is asked (`buildComposeAutocompleteUserPrompt`).
pub fn build_user_prompt(
    channel: FollowupChannel,
    user_name: &str,
    context: &ComposeContext,
    draft: &str,
    now: &str,
) -> String {
    let timeline = if context.timeline.is_empty() {
        "(no prior SMS or email)".to_string()
    } else {
        context
            .timeline
            .iter()
            .map(|m| {
                let who = if m.direction == crate::followups::timeline::Direction::Inbound {
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
    };
    let company = [&context.company_name, &context.company_address]
        .into_iter()
        .flatten()
        .map(|part| trim(part))
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(", ");
    let notes = [
        format_rep_notes_block(user_name, &context.rep_notes, channel),
        crate::followups::rep_voice::format_voice_block(&context.rep_voice),
    ]
    .into_iter()
    .filter(|block| !block.is_empty())
    .collect::<Vec<_>>()
    .join("\n\n");
    let phone = context
        .rep_phone
        .as_deref()
        .map(trim)
        .filter(|p| !p.is_empty())
        .map(|p| format!(", phone {p}"))
        .unwrap_or_default();
    let profile = trim(&context.profile);
    let quotes = if trim(&context.quotes).is_empty() {
        String::new()
    } else {
        format!(
            "\n\nQUOTES (this customer's quotes, newest first; times UTC):\n{}",
            trim(&context.quotes)
        )
    };
    // How the next message opens: only asked for with an empty draft.
    let greeting = chrono::NaiveDateTime::parse_from_str(now, "%Y-%m-%d %H:%M")
        .ok()
        .filter(|_| draft.is_empty() && !context.timeline.is_empty())
        .map(|at| greeting_instruction(conversation_recency(&context.timeline, at.and_utc())))
        .map(|line| format!("{line}\n"))
        .unwrap_or_default();
    let draft_part = if draft.is_empty() {
        if customer_has_not_replied(&context.timeline) {
            format!(
                "{greeting}DRAFT: (empty — the customer has not written to us yet: suggest the whole message)"
            )
        } else {
            format!("{greeting}DRAFT: (empty — suggest how the next message starts)")
        }
    } else {
        format!(
            "DRAFT (already typed — continue from its very end):\n\"{draft}\"{}",
            draft_tail_note(draft)
        )
    };
    format!(
        "CHANNEL: {}\nSALES REP: {user_name}{phone}\nCOMPANY: {}\n{}\nCUSTOMER / PROJECT INFO:\n{}{quotes}\n\nCONVERSATION — oldest first:\n{timeline}\n\nNOW: {now}\n\n{draft_part}",
        channel.as_str().to_uppercase(),
        if company.is_empty() {
            "(unknown)"
        } else {
            company.as_str()
        },
        if notes.is_empty() {
            String::new()
        } else {
            format!("\n{notes}\n")
        },
        if profile.is_empty() {
            "(none)"
        } else {
            profile
        },
    )
}

/// The sentence the rep is typing (after the last . ! ? or line break).
fn current_sentence(draft: &str) -> &str {
    draft
        .rsplit(['.', '!', '?', '\n'])
        .next()
        .unwrap_or("")
        .trim()
}

/// The model restarted the rep's sentence instead of continuing it.
pub fn completion_restarts_draft(completion: &str, draft: &str) -> bool {
    let words: Vec<String> = current_sentence(draft)
        .to_lowercase()
        .split_whitespace()
        .map(str::to_string)
        .collect();
    if words.len() < 2 {
        return false;
    }
    completion
        .trim_start()
        .to_lowercase()
        .starts_with(&format!("{} {}", words[0], words[1]))
}

/// Strips wrapper quotes and an echoed draft from the model's text.
pub fn normalize_completion(raw: &str, draft: &str) -> String {
    let mut text = CODE_FENCE
        .replace_all(&raw.replace('\r', ""), "")
        .into_owned();
    if let Some(inner) = QUOTED.captures(&text).and_then(|c| c.get(1)) {
        text = inner.as_str().to_string();
    }
    text = PROMPT_LABEL.replace(&text, "").into_owned();
    if !draft.is_empty() && text.to_lowercase().starts_with(&draft.to_lowercase()) {
        text = js_slice(&text, js_len(draft), js_len(&text));
    }
    let text = text.trim_end().to_string();
    if text.trim().is_empty() {
        return String::new();
    }
    if draft.is_empty() {
        return text.trim_start().to_string();
    }
    if completion_restarts_draft(&text, draft) {
        return String::new();
    }
    let mut text = text;
    // The model repeated the unfinished word: "soo" + "soon, see" → "n, see".
    if let Some(partial) = PARTIAL_WORD.find(draft).map(|m| m.as_str())
        && text.chars().next().is_some_and(char::is_alphabetic)
    {
        let first = FIRST_WORD.find(&text).map_or("", |m| m.as_str());
        if js_len(first) > js_len(partial)
            && first.to_lowercase().starts_with(&partial.to_lowercase())
        {
            text = js_slice(&text, js_len(partial), js_len(&text));
        }
    }
    if draft.ends_with(char::is_whitespace) {
        text = text.trim_start_matches([' ', '\t']).to_string();
    }
    text
}

/// Whose thread the composer is in.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ComposeTarget {
    pub channel: ComposeChannel,
    /// The customer.
    pub customer_id: Option<i32>,
    /// The email recipient, when there is no customer.
    pub customer_email: Option<String>,
    /// The SMS thread's phone digits.
    pub phone_digits: Option<String>,
    /// The deal the composer is on.
    pub deal_id: Option<i32>,
}

/// SMS or email.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ComposeChannel {
    Sms,
    Email,
}

impl From<ComposeChannel> for FollowupChannel {
    fn from(channel: ComposeChannel) -> Self {
        match channel {
            ComposeChannel::Sms => Self::Sms,
            ComposeChannel::Email => Self::Email,
        }
    }
}

struct Thread {
    channel: FollowupChannel,
    customer_id: Option<i32>,
    customer_email: Option<String>,
    phone_digits: Option<String>,
    deal_id: Option<i32>,
}

impl Thread {
    fn key(&self, user: &SessionUser) -> String {
        format!(
            "{}:{}:{}:{}:{}:{}:{}",
            user.company_id,
            user.id,
            self.channel.as_str(),
            self.customer_id
                .map(|id| id.to_string())
                .unwrap_or_default(),
            self.customer_email.as_deref().unwrap_or(""),
            self.phone_digits.as_deref().unwrap_or(""),
            self.deal_id.map(|id| id.to_string()).unwrap_or_default(),
        )
    }
}

fn checked(target: &ComposeTarget) -> ApiResult<Thread> {
    let email = target
        .customer_email
        .as_deref()
        .map(|e| e.trim().to_lowercase())
        .filter(|e| e.contains('@'));
    if email.as_ref().is_some_and(|e| e.len() > 320) {
        return Err(ApiError::BadRequest("Invalid request data".into()));
    }
    let phone = target
        .phone_digits
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty());
    if phone.is_some_and(|p| !valid_thread_phone(p)) {
        return Err(ApiError::BadRequest("Invalid phone number".into()));
    }
    let customer_id = target.customer_id.filter(|id| *id > 0);
    if customer_id.is_none() && email.is_none() && phone.is_none() {
        return Err(ApiError::BadRequest("Customer required".into()));
    }
    Ok(Thread {
        channel: target.channel.into(),
        customer_id,
        customer_email: email,
        phone_digits: phone.map(str::to_string),
        deal_id: target.deal_id.filter(|id| *id > 0),
    })
}

async fn load_context(
    state: &AppState,
    user: &SessionUser,
    thread: &Thread,
) -> ApiResult<ComposeContext> {
    let key = thread.key(user);
    {
        let contexts = state.extensions.get_or_default::<ComposeContexts>();
        let cache = contexts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((at, context)) = cache.get(&key)
            && at.elapsed() < CONTEXT_TTL
        {
            return Ok(context.clone());
        }
    }
    let mut request = FollowupRequest::new(user.company_id, user.id, thread.channel);
    request.customer_id = thread.customer_id;
    request.phone_digits.clone_from(&thread.phone_digits);
    request.customer_email.clone_from(&thread.customer_email);
    request.preferred_deal_id = thread.deal_id.map(i64::from);
    let gathered = gather_followup_context(&state.pool, &request, Utc::now()).await?;
    // A customer id of another company resolves to none.
    if thread.customer_id.is_some() && gathered.customer_id.is_none() {
        return Err(ApiError::BadRequest("Customer not found".into()));
    }
    let rep: Option<(Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT phone_number, cloudtalk_phone_number, ringcentral_phone_number FROM users WHERE id = ? LIMIT 1",
    )
    .bind(user.id)
    .fetch_optional(&state.pool)
    .await?;
    let company: Option<(Option<String>, Option<String>, Option<String>)> =
        sqlx::query_as("SELECT name, address, domain FROM company WHERE id = ? LIMIT 1")
            .bind(user.company_id)
            .fetch_optional(&state.pool)
            .await?;
    // Their own phone, else the Telnyx line they text from, else their
    // CloudTalk or RingCentral number.
    let telnyx_line = crate::phone::texting::user_line(&state.pool, user.company_id, user.id)
        .await?
        .map(|line| line.e164);
    let rep_phone = rep.and_then(|(own, cloudtalk, ringcentral)| {
        [own, telnyx_line, cloudtalk, ringcentral]
            .into_iter()
            .flatten()
            .map(|p| p.trim().to_string())
            .find(|p| !p.is_empty())
    });
    let (company_name, company_address, domain) = company.unwrap_or_default();
    let quote_page = format!(
        "Customer quote URL (they type their measurements there and send them to us): {}",
        customer_quote_page_url(user.company_id, domain.as_deref())
    );
    let deal_id = thread.deal_id.map(i64::from);
    // Best effort, like the reply drafter: without them the AI only can't
    // answer quote questions.
    let quotes = match gathered.customer_id {
        Some(customer_id) => {
            match reply_context::load_quotes(&state.pool, user.company_id, customer_id, deal_id)
                .await
            {
                Ok(quotes) => reply_context::format_quotes(&quotes, deal_id, Utc::now()),
                Err(err) => {
                    tracing::warn!(error = %err, customer_id, "compose: quotes unavailable");
                    String::new()
                }
            }
        }
        None => String::new(),
    };
    let context = ComposeContext {
        profile: format!("{}\n{quote_page}", gathered.profile),
        timeline: trim_timeline(&gathered.timeline, TIMELINE_MAX_CHARS),
        rep_phone,
        company_name,
        company_address,
        rep_notes: gathered
            .style_profile
            .and_then(|p| p.notes)
            .unwrap_or_default(),
        rep_voice: gathered.rep_voice.examples,
        quotes,
    };
    let contexts = state.extensions.get_or_default::<ComposeContexts>();
    let mut cache = contexts
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    cache.retain(|_, (at, _)| at.elapsed() < CONTEXT_TTL);
    if cache.len() >= CONTEXT_MAX_ENTRIES
        && let Some(oldest) = cache
            .iter()
            .min_by_key(|(_, (at, _))| *at)
            .map(|(k, _)| k.clone())
    {
        cache.remove(&oldest);
    }
    cache.insert(key, (Instant::now(), context.clone()));
    drop(cache);
    Ok(context)
}

/// The customer stones page.
#[derive(Debug, Serialize, ToSchema)]
pub struct InventoryUrl {
    /// `https://<subdomain>.granite-manager.com/customer/<company>/stones`.
    pub inventory_url: String,
}

/// The stones page link
///
/// The company's customer stones page, which the composer offers to insert.
#[crate::openapi::path(
    get,
    path = "/v1/ai/compose/inventory-url",
    operation_id = "compose_inventory_url",
    tag = "ai",
    responses(
        (status = 200, description = "The link", body = InventoryUrl),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn inventory_url(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<InventoryUrl>> {
    let domain: Option<Option<String>> =
        sqlx::query_scalar("SELECT domain FROM company WHERE id = ?")
            .bind(user.company_id)
            .fetch_optional(&state.pool)
            .await?;
    Ok(Json(InventoryUrl {
        inventory_url: customer_inventory_page_url(user.company_id, domain.flatten().as_deref()),
    }))
}

/// `POST /v1/ai/compose/autocomplete`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct AutocompleteRequest {
    #[serde(flatten)]
    pub target: ComposeTarget,
    /// What the rep typed so far (empty: suggest how the message starts).
    #[serde(default)]
    pub draft: String,
    /// Only load the thread, so the first suggestion waits on the model alone.
    #[serde(default)]
    pub warm: bool,
}

/// A suggestion.
#[derive(Debug, Serialize, ToSchema)]
pub struct Completion {
    /// The text to append to the draft (empty: nothing fits).
    pub completion: String,
}

/// Continue the draft
///
/// The next few words of the rep's draft (or, with an empty draft, how the
/// next message starts), from the conversation and the rep's notes.
#[crate::openapi::path(
    post,
    path = "/v1/ai/compose/autocomplete",
    operation_id = "compose_autocomplete",
    tag = "ai",
    request_body = AutocompleteRequest,
    responses(
        (status = 200, description = "The suggestion", body = Completion),
        (status = 400, description = "No customer, not a customer of the company, or an invalid phone", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 502, description = "The model failed", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn autocomplete(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(request): JsonBody<AutocompleteRequest>,
) -> ApiResult<Json<Completion>> {
    if js_len(&request.draft) > 8000 {
        return Err(ApiError::BadRequest("Invalid request data".into()));
    }
    let thread = checked(&request.target)?;
    let context = load_context(&state, &user, &thread).await?;
    if request.warm {
        return Ok(Json(Completion {
            completion: String::new(),
        }));
    }
    let prompt = build_user_prompt(
        thread.channel,
        user.name.as_deref().unwrap_or(""),
        &context,
        &request.draft,
        &autocomplete_now(Utc::now()),
    );
    let caller = Caller {
        feature: "quick_reply",
        user_id: Some(user.id),
        company_id: Some(user.company_id),
    };
    let max_tokens = if request.draft.is_empty() && customer_has_not_replied(&context.timeline) {
        FIRST_MESSAGE_MAX_OUTPUT_TOKENS
    } else {
        MAX_OUTPUT_TOKENS
    };
    let thread_key = thread
        .customer_id
        .map(|id| id.to_string())
        .or_else(|| thread.customer_email.clone())
        .or_else(|| thread.phone_digits.clone())
        .unwrap_or_default();
    let chat = |model| {
        let mut chat = Chat::new(model)
            .message("system", COMPOSE_AUTOCOMPLETE_SYSTEM_PROMPT)
            .message("user", &prompt)
            .max_tokens(Some(max_tokens));
        // Reasoning used up the cap and left the reply empty; none is needed.
        chat.reasoning_effort = Some("none");
        chat.prompt_cache_key = Some(format!(
            "compose-autocomplete:{}:{thread_key}",
            user.company_id
        ));
        chat
    };
    let raw = match state.ai.chat(&state.pool, caller, &chat(MINI_MODEL)).await {
        Err(err) if err.is_model_permission() => {
            state
                .ai
                .chat(&state.pool, caller, &chat(REPLY_FALLBACK_MODEL))
                .await
        }
        result => result,
    }
    .map_err(ai_failed)?;
    Ok(Json(Completion {
        completion: normalize_completion(&raw, &request.draft),
    }))
}

fn ai_failed(err: AiError) -> ApiError {
    match err {
        AiError::NotConfigured => ApiError::Unavailable(err.to_string()),
        other => ApiError::Upstream(format!("Failed to generate suggestion: {other}")),
    }
}

/// `POST /v1/ai/compose/learn`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct LearnRequest {
    #[serde(flatten)]
    pub target: ComposeTarget,
    /// The whole message the AI proposed.
    pub suggested: String,
    /// What the rep sent instead.
    pub sent: String,
}

/// Whether the notes changed.
#[derive(Debug, Serialize, ToSchema)]
pub struct Learned {
    /// The edit taught something (the notes were rewritten).
    pub learned: bool,
}

fn squash(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// The rep reworded the suggestion rather than writing a different message,
/// so the pair shows how they write, not what this customer needed.
pub fn is_edit_of(suggested: &str, sent: &str) -> bool {
    let suggested = words(suggested);
    if suggested.is_empty() {
        return false;
    }
    let sent: std::collections::HashSet<String> = words(sent).into_iter().collect();
    let kept = suggested.iter().filter(|w| sent.contains(*w)).count();
    #[allow(clippy::cast_precision_loss)]
    let overlap = kept as f64 / suggested.len() as f64;
    overlap >= MIN_EDIT_OVERLAP
}

/// The model's notes, cleaned (`normalizeRepNotes`); `None` when unusable.
pub fn parse_rep_notes_reply(raw: &str) -> Option<Vec<String>> {
    let parsed: Value = serde_json::from_str(raw).ok()?;
    let items = parsed.get("notes")?.as_array()?;
    let mut seen = std::collections::HashSet::new();
    let mut notes = Vec::new();
    for item in items {
        let Some(text) = item.as_str() else { continue };
        let squashed = squash(text);
        let stripped = squashed
            .strip_prefix(['-', '•', '*'])
            .map_or(squashed.as_str(), str::trim_start);
        let note = js_slice(stripped, 0, REP_NOTE_MAX_CHARS.min(js_len(stripped)));
        if note.is_empty() || !seen.insert(note.to_lowercase()) {
            continue;
        }
        notes.push(note);
        if notes.len() >= REP_NOTES_MAX {
            break;
        }
    }
    Some(notes)
}

/// What the note writer is asked.
pub fn build_rep_notes_prompt(
    rep_name: &str,
    notes: &[String],
    pairs: &[(String, String, String)],
) -> String {
    let notes = if notes.is_empty() {
        "(none yet)".to_string()
    } else {
        notes
            .iter()
            .map(|n| format!("- {n}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let pairs = pairs
        .iter()
        .enumerate()
        .map(|(index, (channel, suggested, sent))| {
            format!(
                "#{} {}\nAI suggested: {suggested}\nRep sent: {sent}",
                index + 1,
                channel.to_uppercase()
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    format!("REP: {rep_name}\n\nCURRENT NOTES:\n{notes}\n\nRECENT EXAMPLES — newest last:\n{pairs}")
}

/// Learn from a sent message
///
/// The rep sent something other than the AI's suggestion: keeps the pair and
/// rewrites the rep's writing notes from it and the few before it.
#[crate::openapi::path(
    post,
    path = "/v1/ai/compose/learn",
    operation_id = "compose_learn",
    tag = "ai",
    request_body = LearnRequest,
    responses(
        (status = 200, description = "Whether anything was learned", body = Learned),
        (status = 400, description = "No customer", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 502, description = "The model failed", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn learn(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(request): JsonBody<LearnRequest>,
) -> ApiResult<Json<Learned>> {
    let thread = checked(&request.target)?;
    let clip = |text: &str| {
        let text = trim(text);
        js_slice(text, 0, LEARN_MAX_CHARS.min(js_len(text)))
    };
    let (suggested, sent) = (clip(&request.suggested), clip(&request.sent));
    let (a, b) = (squash(&suggested), squash(&sent));
    if a.is_empty() || b.is_empty() || a == b {
        return Ok(Json(Learned { learned: false }));
    }
    let pool = &state.pool;
    sqlx::query(
        "INSERT INTO ai_followup_edit_pairs (user_id, company_id, deal_id, channel, generated_text, final_text, phase)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(user.id)
    .bind(user.company_id)
    .bind(thread.deal_id)
    .bind(thread.channel.as_str())
    .bind(&suggested)
    .bind(&sent)
    .bind(COMPOSE_EDIT_PHASE)
    .execute(pool)
    .await?;
    // A different message still shows the rep's voice (kept above), but
    // teaches no habit.
    if !is_edit_of(&suggested, &sent) {
        return Ok(Json(Learned { learned: false }));
    }
    let current = crate::followups::gather::load_style_profile(pool, user.id, user.company_id)
        .await?
        .and_then(|p| p.notes)
        .unwrap_or_default();
    let recent: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT CAST(channel AS CHAR), generated_text, final_text FROM ai_followup_edit_pairs
          WHERE user_id = ? AND company_id = ? AND phase = ? ORDER BY id DESC LIMIT ?",
    )
    .bind(user.id)
    .bind(user.company_id)
    .bind(COMPOSE_EDIT_PHASE)
    .bind(REP_NOTES_CANDIDATE_PAIRS)
    .fetch_all(pool)
    .await?;
    let mut pairs: Vec<_> = recent
        .into_iter()
        .filter(|(_, suggested, sent)| is_edit_of(suggested, sent))
        .take(REP_NOTES_EVIDENCE_PAIRS)
        .map(|(channel, suggested, sent)| {
            let cap = |text: &str| js_slice(text, 0, REP_NOTES_PAIR_MAX_CHARS.min(js_len(text)));
            (channel, cap(&suggested), cap(&sent))
        })
        .collect();
    pairs.reverse();
    let mut chat = Chat::new(MINI_MODEL)
        .message("system", REP_NOTES_SYSTEM_PROMPT)
        .message(
            "user",
            &build_rep_notes_prompt(user.name.as_deref().unwrap_or(""), &current, &pairs),
        )
        .max_tokens(Some(1200));
    chat.json = true;
    chat.reasoning_effort = Some("low");
    let caller = Caller {
        feature: "quick_reply",
        user_id: Some(user.id),
        company_id: Some(user.company_id),
    };
    let reply = state
        .ai
        .chat(pool, caller, &chat)
        .await
        .map_err(ai_failed)?;
    let Some(notes) = parse_rep_notes_reply(&reply) else {
        return Ok(Json(Learned { learned: false }));
    };
    let json = serde_json::to_string(&notes).unwrap_or_else(|_| "[]".into());
    // Same row as the follow-up style stats; only the notes key changes.
    sqlx::query(
        "INSERT INTO ai_followup_style_preferences (user_id, company_id, preferences_json, sample_count)
         VALUES (?, ?, JSON_OBJECT('notes', CAST(? AS JSON)), 0)
         ON DUPLICATE KEY UPDATE preferences_json = JSON_SET(preferences_json, '$.notes', CAST(? AS JSON)),
                                 updated_at = UTC_TIMESTAMP()",
    )
    .bind(user.id)
    .bind(user.company_id)
    .bind(&json)
    .bind(&json)
    .execute(pool)
    .await?;
    Ok(Json(Learned { learned: true }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completions_are_cleaned_like_the_crm() {
        assert_eq!(
            normalize_completion("soon, see you Friday.", "I'll be soo"),
            "n, see you Friday."
        );
        assert_eq!(
            normalize_completion("\"day at 5:00 PM.\"", "See you Fri"),
            "day at 5:00 PM."
        );
        assert_eq!(
            normalize_completion("Thanks for getting back!", "Thanks for "),
            "getting back!"
        );
        assert_eq!(
            normalize_completion("I'll be sure to bring it", "I'll be soo"),
            ""
        );
        assert_eq!(
            normalize_completion("```\n a few samples?\n```", "Would you like me to bring"),
            " a few samples?"
        );
        assert_eq!(normalize_completion("  Hi Ann!  ", ""), "Hi Ann!");
        assert_eq!(normalize_completion("   ", "Hi"), "");
    }

    #[test]
    fn the_crm_cases_clean_the_same() {
        let cases = [
            ("\" at 5pm\"", "See you Friday", " at 5pm"),
            ("See you Friday at 5pm", "See you Friday", " at 5pm"),
            (" at 5pm", "See you Friday ", "at 5pm"),
            ("   ", "x", ""),
            (
                "I'll be sure to bring a few samples. See you Friday at 5:00 PM!",
                "I'll be soo",
                "",
            ),
            ("soon, see you Friday.", "I'll be soo", "n, see you Friday."),
            ("n, see you Friday.", "I'll be soo", "n, see you Friday."),
            (" See you Friday!", "", "See you Friday!"),
        ];
        for (raw, draft, expected) in cases {
            assert_eq!(
                normalize_completion(raw, draft),
                expected,
                "{raw:?} after {draft:?}"
            );
        }
    }

    #[test]
    fn the_newest_messages_fit_the_budget() {
        use crate::followups::timeline::Direction;
        let message = |direction, text: String| FollowupTimelineMessage {
            at: "2026-10-07T10:00:00Z".into(),
            channel: FollowupChannel::Email,
            direction,
            text,
            from_email: None,
            from_phone: None,
        };
        let kept = trim_timeline(
            &[
                message(Direction::Inbound, "a".repeat(50)),
                message(Direction::Outbound, "b".repeat(50)),
            ],
            70,
        );
        let lengths: Vec<usize> = kept.iter().map(|m| js_len(&m.text)).collect();
        assert_eq!(lengths, [21, 50]);
        assert_eq!(kept[0].text, format!("…{}", "a".repeat(20)));
    }

    #[test]
    fn the_prompt_ends_with_the_draft_and_how_it_ends() {
        let context = ComposeContext {
            profile: " Name: Ann ".into(),
            timeline: Vec::new(),
            rep_phone: Some(" 317-555-0000 ".into()),
            company_name: Some("Acme Stone".into()),
            company_address: None,
            rep_notes: vec!["Opens with 'Hey'".into()],
            rep_voice: Vec::new(),
            quotes: String::new(),
        };
        let prompt = build_user_prompt(
            FollowupChannel::Sms,
            "Rita Rep",
            &context,
            "I'll be soo",
            "2026-10-13 15:00",
        );
        assert_eq!(
            prompt,
            "CHANNEL: SMS\nSALES REP: Rita Rep, phone 317-555-0000\nCOMPANY: Acme Stone\n\nHOW RITA REP WRITES (learned from what they change in suggestions — write like this):\n- Opens with 'Hey'\n\nCUSTOMER / PROJECT INFO:\nName: Ann\n\nCONVERSATION — oldest first:\n(no prior SMS or email)\n\nNOW: 2026-10-13 15:00\n\nDRAFT (already typed — continue from its very end):\n\"I'll be soo\"\nThe draft ends mid-word or right after the word \"soo\". If it is unfinished, complete it (keep \"soo\", add only the missing letters)."
        );
        let empty = build_user_prompt(
            FollowupChannel::Email,
            "Rita",
            &ComposeContext {
                rep_notes: Vec::new(),
                rep_voice: Vec::new(),
                quotes: String::new(),
                ..context
            },
            "",
            "x",
        );
        assert!(empty.contains("COMPANY: Acme Stone\n\nCUSTOMER / PROJECT INFO:"));
        assert!(empty.ends_with(
            "DRAFT: (empty — the customer has not written to us yet: suggest the whole message)"
        ));
    }

    #[test]
    fn a_rep_without_a_phone_and_an_unknown_company() {
        let context = ComposeContext {
            profile: String::new(),
            timeline: Vec::new(),
            rep_phone: None,
            company_name: None,
            company_address: Some("  ".into()),
            rep_notes: Vec::new(),
            rep_voice: Vec::new(),
            quotes: String::new(),
        };
        let prompt = build_user_prompt(FollowupChannel::Sms, "Dema", &context, "", "x");
        assert!(prompt.contains("SALES REP: Dema\n"));
        assert!(prompt.contains("COMPANY: (unknown)"));
        assert!(prompt.contains("CUSTOMER / PROJECT INFO:\n(none)"));
        let at = chrono::DateTime::parse_from_rfc3339("2026-10-09T13:22:59Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(autocomplete_now(at), "2026-10-09 13:22");
    }

    #[test]
    fn an_empty_draft_in_an_ongoing_thread_has_no_greeting() {
        use crate::followups::timeline::Direction;
        let message = |at: &str, channel, direction| FollowupTimelineMessage {
            at: at.into(),
            channel,
            direction,
            text: "Can you send the price?".into(),
            from_email: None,
            from_phone: None,
        };
        let context = ComposeContext {
            profile: String::new(),
            // The customer texted this morning; the rep now starts an email.
            timeline: vec![message(
                "2026-10-13 13:30",
                FollowupChannel::Sms,
                Direction::Inbound,
            )],
            rep_phone: None,
            company_name: None,
            company_address: None,
            rep_notes: Vec::new(),
            rep_voice: Vec::new(),
            quotes: String::new(),
        };
        let prompt = build_user_prompt(
            FollowupChannel::Email,
            "Dema",
            &context,
            "",
            "2026-10-13 15:00",
        );
        assert!(prompt.contains("GREETING: NONE"));
        assert!(prompt.ends_with("DRAFT: (empty — suggest how the next message starts)"));
        // The next day, greet by name. A typed draft gets no rule.
        let later = build_user_prompt(
            FollowupChannel::Sms,
            "Dema",
            &context,
            "",
            "2026-10-14 15:00",
        );
        assert!(later.contains("GREETING: BY NAME"));
        let typing = build_user_prompt(
            FollowupChannel::Sms,
            "Dema",
            &context,
            "Sure",
            "2026-10-13 15:00",
        );
        assert!(!typing.contains("GREETING:"));
    }

    #[test]
    fn rep_notes_are_cleaned_and_capped() {
        let notes = parse_rep_notes_reply(
            r#"{"notes": ["- Opens with 'Hey'", "opens with 'hey'", "  Signs   off as Dema ", 3]}"#,
        )
        .unwrap();
        assert_eq!(notes, ["Opens with 'Hey'", "Signs off as Dema"]);
        assert_eq!(parse_rep_notes_reply("nope"), None);
        assert_eq!(parse_rep_notes_reply(r#"{"x": []}"#), None);
    }

    #[test]
    fn channel_notes_reach_only_their_channel() {
        let notes = [
            "Opens with 'Hey'".to_string(),
            "SMS: Introduces self as 'Dima'".to_string(),
            "Email: Ends with 'Thanks!'".to_string(),
        ];
        let sms = format_rep_notes_block("Dema", &notes, FollowupChannel::Sms);
        assert!(sms.ends_with("- Opens with 'Hey'\n- Introduces self as 'Dima'"));
        let email = format_rep_notes_block("Dema", &notes, FollowupChannel::Email);
        assert!(email.ends_with("- Opens with 'Hey'\n- Ends with 'Thanks!'"));
        assert_eq!(
            format_rep_notes_block("Dema", &notes[2..], FollowupChannel::Sms),
            ""
        );
    }

    #[test]
    fn the_reps_own_messages_are_in_the_prompt() {
        let context = ComposeContext {
            profile: String::new(),
            timeline: Vec::new(),
            rep_phone: None,
            company_name: None,
            company_address: None,
            quotes: String::new(),
            rep_notes: Vec::new(),
            rep_voice: vec!["Hey Simon, this is Dima with Granite Depot.".into()],
        };
        let prompt = build_user_prompt(FollowupChannel::Sms, "Dema", &context, "", "x");
        assert!(
            prompt.contains("RECENT MESSAGES BY THE REP (to various customers, newest first).")
        );
        assert!(prompt.contains("- Hey Simon, this is Dima with Granite Depot.\n"));
    }

    #[test]
    fn an_echoed_prompt_label_is_dropped() {
        assert_eq!(
            normalize_completion("GREETING: Hello Elizabeth, I'm Cristina.", ""),
            "Hello Elizabeth, I'm Cristina."
        );
    }

    #[test]
    fn a_different_message_is_not_an_edit() {
        assert!(is_edit_of(
            "Hey Pete, could you resend the picture? That way I can check it.",
            "Hey Pete, could you please resend the picture?"
        ));
        assert!(!is_edit_of(
            "Hi Edward, I can send you a link to browse our stones.",
            "Hey Edward! How would you like to pay the deposit: card or Zelle?"
        ));
        assert!(!is_edit_of("", "Hi"));
    }
}
