//! AI promises: what a rep told customers they would do ("I'll text you our
//! inventory link"), read from their calls, texts and emails, for the reps
//! it is switched on for (`users.ai_promises`).
//!
//! [`run_job`] (`POST /v1/jobs/promises`; the `time-triggered` Lambda starts
//! it every minute on the long-running deployment without waiting):
//! 1. queues the rep's calls, texts and emails of the last
//!    [`LOOKBACK_DAYS`] days it has not read (`ai_promise_scans`): recorded
//!    calls of [`MIN_CALL_SECONDS`] seconds or more with a customer's number
//!    (`RingCentral`, `CloudTalk` and Granite Phone), and the texts and
//!    emails the rep wrote themselves (not automated ones);
//! 2. claims up to [`SCANS_PER_RUN`] of them, oldest first (a claim lasts
//!    [`CLAIM_TTL_MINUTES`] minutes, [`MAX_ATTEMPTS`] attempts in all), so
//!    overlapping runs take different ones;
//! 3. transcribes a call (speaker labels on; the transcript is kept on the
//!    scan) and has the AI list what the rep promised and by when (with what
//!    in the CRM finishes it), which of the customer's open promises it keeps
//!    (those are closed), and the quote work it gives the rep: one general
//!    task per customer (kind `quote`), what the customer asked for in their
//!    quote added to its details; the AI sees the open ones, so it never
//!    lists one twice;
//! 4. writes the message that keeps each promise a text or email can keep,
//!    as the AI Reply writes a custom draft (the rep's notes and voice, the
//!    whole conversation, the customer's quotes), and saves the promises
//!    (`ai_promises`) for the CRM's promise list and composers;
//! 5. reminds the rep of each promise once it is due ([`remind_due`]): a bell
//!    notification pushed to their phones, else sent to their Telegram;
//! 6. from 8 AM their time, once a day, sends the rep's morning list
//!    ([`send_digests`]): due today, overdue, without a time, quotes to make
//!    or update, what waits for the customer's approval, and the customers
//!    whose text waits for their reply;
//! 7. a day after the rep sent something for the customer's approval (kind
//!    `approval`), closes it when the customer has replied since, else
//!    reminds the rep once to check with them ([`check_approvals`]);
//! 8. closes the tasks the rep finished in the CRM ([`close_finished`]): a
//!    quote of the customer's saved or emailed, a document sent to sign, an
//!    event on the schedule, a call with them.

use crate::ai::{AiError, Caller, Chat, MINI_FALLBACK_MODEL, MINI_MODEL};
use crate::auth::session::{SessionUser, find_live_user};
use crate::followups::gather::{customer_inventory_page_url, customer_quote_page_url};
use crate::phone::company;
use crate::routes::ai::conversation_reply::{self, ConversationReplyInput, ReplyMode};
use crate::routes::calls::recording::{CallTranscriptError, transcribe_call};
use crate::sms_provider::not_a_copy;
use crate::state::AppState;
use crate::transcribe::TranscribeError;
use chrono::{NaiveDateTime, TimeZone, Timelike, Utc};
use chrono_tz::Tz;
use lambda_http::tracing;
use regex::Regex;
use serde_json::Value;
use sqlx::MySqlPool;
use std::sync::LazyLock;

/// The AI usage log's feature key (`app/utils/aiFeatures.ts`).
pub const FEATURE: &str = "ai_promises";
/// How far back a rep's calls and messages are read.
pub const LOOKBACK_DAYS: i64 = 3;
/// Shorter calls are attempts and voicemail greetings.
pub const MIN_CALL_SECONDS: i64 = 30;
/// Calls and messages read per run, all at once (each of another customer).
pub const SCANS_PER_RUN: i64 = 4;
/// Waiting scans looked at to find those of different customers.
const CANDIDATES_PER_RUN: i64 = 50;
/// A claimed scan is another run's after this long.
pub const CLAIM_TTL_MINUTES: i64 = 15;
pub const MAX_ATTEMPTS: i64 = 3;
/// The most of a transcript or an email the AI reads (about 30 minutes of
/// talk); a longer one keeps its start and its end, where promises are made.
const MAX_SOURCE_CHARS: usize = 30_000;
/// Texts before the rep's text that give it its meaning.
const SMS_CONTEXT_MESSAGES: i64 = 8;
/// The most of the messages an email quotes the AI reads.
const QUOTED_MAX_CHARS: usize = 3_000;
const MAX_PROMISES: usize = 5;
/// What a quote task's details keep of the customer's asks: a call's, and in all.
const MAX_ASKS: usize = 10;
const MAX_DETAIL_LINES: usize = 30;
const TITLE_MAX_CHARS: usize = 160;
const SAID_MAX_CHARS: usize = 500;
/// The text tables a scan's `source_key` may name.
const SMS_TABLES: [&str; 3] = ["ringcentral_sms", "cloudtalk_sms", "telnyx_sms"];
/// How long something sent for approval waits for the customer's reply
/// before the rep is reminded.
pub const APPROVAL_WAIT_HOURS: i64 = 24;

/// SQL condition on `ai_promise_scans`: waiting, with attempts left, and no
/// run holds it (binds: the most attempts, the claim's minutes).
const DUE: &str = "status = 'pending' AND attempts < ?
     AND (claimed_at IS NULL OR claimed_at < UTC_TIMESTAMP() - INTERVAL ? MINUTE)";

pub const PROMISES_SYSTEM_PROMPT: &str = r#"You read what a countertop sales rep said to a customer: a phone call transcript, a text the rep sent (with the texts before it), or an email the rep sent (with the messages it quotes). The rep keeps a short list of real tasks, not every phrase. Answer these things.
1. "promises": every promise the rep made to the customer: something the rep said THEY would do later ("I'll text you our inventory link", "I'll send you the price tonight", "I'll call you back Tuesday", "Let me check if that slab is in stock", "I'll come measure Friday at 10").
Only the rep's own promises to this customer. Not what the customer will do, and not something the rep only starts to say when the customer then takes the next step (the rep: "I can check it. I'll—", the customer: "I'll take a picture of the elevator and send it to you"): the rep waits for the customer, which is not a promise. Not what is already done in the same call or message (a link the text itself contains is sent, not promised), not closings like "let me know if you have questions" or "talk soon", and nothing already in OPEN PROMISES. In texts, promises come only from the LAST text (the rep's); in an email, only from the rep's new text, not the quoted messages. Messages and calls under AFTER THIS are context only: a promise they already keep is not listed (an email to another address counts when it is clearly to this same customer, e.g. "Contract - Cedarbrook" after "I'll send you the contract").
Only what THIS REP personally has to do. Not what another person or team will do ("our scheduling manager will call you", "the installers will…", "the office will send…", "we'll let you know the exact day and time once we have it" when another person or team sets it), and not the company's standard work that happens anyway as part of measuring, templating, fabrication or installation ("we will drill the faucet holes on site during installation", "we'll seal the stone", "we'll haul away the old tops"): those are how the job is done, not promises. Combine promises about the same thing into one ("I'll text Andrew" and "I'll let you know his answer" are one: "Ask Andrew and get back to her"). Making, changing or pricing a quote is never a promise: it is the quote task (3).
2. "kept": the numbers (#) of OPEN PROMISES this call or message keeps. A promise is there to solve the customer's issue, so judge by the outcome, not the exact step: it is kept when the rep did it here (sent the link or the price, called back, answered, came by), when the rep gives the result it was for (after "I'll call the shop manager about the cutout", telling what happened to the cutout; after "I'll try to expedite the installation", giving the installation date; after "I'll remind you next time", the reminder itself), or when the issue is settled another way (the customer approved what the rep was to remind them about). Not kept while the issue is still open. An OPEN APPROVAL is kept once the customer answered it (approved, signed, chose, or turned it down). An OPEN QUOTE TASK is kept once the rep sends the made or changed quote, or its new price. OPEN PROMISES TO CALLERS NOT ON FILE are the rep's promises to people whose number matched no customer: count one kept only when this message clearly goes to that same person (their name, what was promised, the timing).
3. "quote": the quote work this call or message gives the rep, as ONE general task, or null: the customer asks for a quote or for changes in theirs (a different sink, color or stone, edge, backsplash, sizes, adding or removing a piece or a cutout, a cheaper option), anywhere in what you read, or the rep promises to make, change or price one ("I'll redo the quote", "I'll send you the new price tonight"). Null for a question the rep answers right there, and for quote work the rep says is already done ("updated as you requested", "attached is the revised quote"). With an OPEN QUOTE TASK, give only the asks it does not have yet (null when there are none and no new time).
4. "approvals": what THIS call or message (the rep's part; in texts, the LAST text) sends or shows the customer for THEM to approve, sign or choose: a quote or a revised quote, a contract, a drawing or layout, slab photos to pick from, a deposit invoice. Only when it is sent here (attached, linked, "I just emailed you the quote") and waits on the customer's answer; not what the rep only promises to send later (that is a promise), not what the customer already answered in what you read, and nothing already in OPEN APPROVALS.
5. "customer_name": the name the customer goes by in what you read ("Donna"), or null.
In a transcript the speakers are labeled A, B...; the rep is the one from the countertop company.
Return JSON {"promises": [...], "kept": [numbers], "quote": {...} or null, "approvals": [...], "customer_name": ...}, with empty lists and a null quote when there is nothing. Each promise:
- "title": what the rep has to do, a short imperative of at most 8 words ("Send the inventory link", "Call back about the sink").
- "said": the rep's words that made the promise, as said (at most 200 characters).
- "due": when the rep promised it, "YYYY-MM-DD HH:MM" in the same local time as WHEN and NOW, or null when they named no time. "Tonight" is 19:00, "tomorrow" with no hour is 10:00, "this week" is Friday 17:00, "right after this call" or "in a minute" is 15 minutes after WHEN.
- "visit": true when it is an appointment at a set time: a visit, measuring or templating, a showroom meeting, an installation; else false.
- "action": what the rep does in the CRM that finishes it, or null: "contract" when it is sending a contract or another document to sign; "schedule" when it is putting a visit, measuring, templating or installation on the schedule (an appointment at a time set here too); "call" when it is calling the customer.
- "message": "sms" or "email" only when a text or an email TO THIS CUSTOMER, needing only words and a link, keeps the promise (a promise to contact someone else, or to answer once someone else replies, is null) (send the inventory link, send the online quote link, answer with facts the rep already has, confirm a time); "email" only when the rep said they would email. null when keeping it takes anything else: a call, a visit, measuring, ordering, checking stock, a price or a document the rep must prepare, photos.
- "link": "inventory" when the message is about our stones, colors or inventory; "quote" when it is about sending their measurements or the online quote form; else null.
The quote task:
- "title": the task, short and general, at most 8 words, without the details ("Make the quote for the Newport Bay house", "Update the kitchen quote").
- "asks": each thing the quote must have or change, at most 12 words each, with the sizes, colors and models said ("Add four pieces: 44x25, 70x25, 14.5x25, 45x35", "Switch the color to Calacatta Gold", "Replace the sink with a 33-inch undermount"); [] when the rep only promised to make or change it.
- "said": the words that asked for it or promised it, as said (at most 200 characters).
- "due": when the rep promised it, as for a promise, or null.
Each approval:
- "title": what waits for the customer, at most 8 words ("Approve the revised kitchen quote", "Sign the contract").
- "said": the rep's words that sent it (at most 200 characters)."#;

/// Where a scan came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Call,
    Sms,
    Email,
}

impl Source {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "call" => Some(Self::Call),
            "sms" => Some(Self::Sms),
            "email" => Some(Self::Email),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Call => "call",
            Self::Sms => "sms",
            Self::Email => "email",
        }
    }
}

/// The message that keeps a promise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Sms,
    Email,
}

impl Channel {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sms => "sms",
            Self::Email => "email",
        }
    }
}

/// What the rep does in the CRM that finishes a promise ([`close_finished`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrmAction {
    /// A contract or another document sent to sign.
    Contract,
    /// An event for the customer on the schedule.
    Schedule,
    /// A call with the customer.
    Call,
}

impl CrmAction {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "contract" => Some(Self::Contract),
            "schedule" => Some(Self::Schedule),
            "call" => Some(Self::Call),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Contract => "contract",
            Self::Schedule => "schedule",
            Self::Call => "call",
        }
    }
}

/// The page a promised message links to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Link {
    /// The customer stones page.
    Inventory,
    /// The online quote form, where customers send their measurements.
    Quote,
}

/// A promise the AI found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub title: String,
    pub said: String,
    /// In the company's time zone.
    pub due: Option<NaiveDateTime>,
    /// An appointment at a set time (a visit, measuring, a showroom meeting).
    pub visit: bool,
    /// Set when a text or an email keeps it.
    pub channel: Option<Channel>,
    pub link: Option<Link>,
    /// Set when doing something in the CRM finishes it.
    pub action: Option<CrmAction>,
}

/// The quote work a call or message gives the rep: one general task, with
/// what the customer asked for in their quote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuoteTask {
    pub title: String,
    /// Each thing the quote must have or change.
    pub asks: Vec<String>,
    pub said: String,
    /// In the company's time zone.
    pub due: Option<NaiveDateTime>,
}

/// Something the rep sent for the customer's approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Approval {
    pub title: String,
    pub said: String,
}

/// What the AI found in a call or message.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reading {
    pub promises: Vec<Found>,
    /// Open promises (their ids) this call or message keeps.
    pub kept: Vec<u64>,
    pub quote: Option<QuoteTask>,
    pub approvals: Vec<Approval>,
    /// The name the customer goes by in it.
    pub customer_name: Option<String>,
}

/// An open promise, quote task or approval of the customer, as the AI is
/// shown it.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct OpenItem {
    pub id: u64,
    /// `promise`, `quote` or `approval`.
    pub kind: String,
    pub title: String,
    /// A quote task's asks, one per line.
    pub details: Option<String>,
    /// It is this customer's; else a promise to a caller not on file.
    pub same_customer: bool,
    /// Whom it is for: a name, phone or email.
    pub who: String,
}

/// `text` within `max` characters: its first quarter and the rest from its end.
pub fn start_and_end(text: &str, max: usize) -> String {
    let text = text.trim();
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    let head = max / 4;
    let start: String = text.chars().take(head).collect();
    let end: String = text.chars().skip(count - (max - head)).collect();
    format!("{start}\n…\n{end}")
}

fn clip(text: &str, max: usize) -> String {
    text.trim()
        .chars()
        .take(max)
        .collect::<String>()
        .trim_end()
        .to_string()
}

/// The model's JSON reply; nothing found when it is not that JSON. `kept`
/// keeps only the open promises it was shown.
pub fn parse_reading(raw: &str, open: &[OpenItem]) -> Reading {
    let Ok(value) = serde_json::from_str::<Value>(raw.trim()) else {
        return Reading::default();
    };
    let list = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let mut kept: Vec<u64> = list("kept")
        .iter()
        .filter_map(|id| {
            id.as_u64()
                .or_else(|| id.as_str()?.trim().trim_start_matches('#').parse().ok())
        })
        .filter(|id| open.iter().any(|item| item.id == *id))
        .collect();
    kept.sort_unstable();
    kept.dedup();
    let approvals = list("approvals")
        .iter()
        .filter_map(|item| {
            let title = clip(item.get("title")?.as_str()?, TITLE_MAX_CHARS);
            (!title.is_empty()).then(|| Approval {
                title,
                said: clip(
                    item.get("said").and_then(Value::as_str).unwrap_or(""),
                    SAID_MAX_CHARS,
                ),
            })
        })
        .take(MAX_PROMISES)
        .collect();
    Reading {
        promises: parse_promises(&list("promises")),
        kept,
        quote: value.get("quote").and_then(parse_quote),
        approvals,
        customer_name: value
            .get("customer_name")
            .and_then(Value::as_str)
            .map(|name| clip(name, 120))
            .filter(|name| !name.is_empty()),
    }
}

/// The quote task of the model's reply; none without a title or an ask.
fn parse_quote(item: &Value) -> Option<QuoteTask> {
    let text = |key: &str| item.get(key).and_then(Value::as_str).map(str::trim);
    let mut asks: Vec<String> = Vec::new();
    for ask in item
        .get("asks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        // One ask per line of the details.
        let ask = clip(
            &ask.as_str().unwrap_or("").replace(['\r', '\n'], " "),
            TITLE_MAX_CHARS,
        );
        if !ask.is_empty()
            && !asks
                .iter()
                .any(|seen| seen.to_lowercase() == ask.to_lowercase())
        {
            asks.push(ask);
        }
    }
    asks.truncate(MAX_ASKS);
    let title = clip(text("title").unwrap_or(""), TITLE_MAX_CHARS);
    if title.is_empty() && asks.is_empty() {
        return None;
    }
    Some(QuoteTask {
        title: if title.is_empty() {
            "Update the quote".to_owned()
        } else {
            title
        },
        asks,
        said: clip(text("said").unwrap_or(""), SAID_MAX_CHARS),
        due: text("due").and_then(|due| NaiveDateTime::parse_from_str(due, "%Y-%m-%d %H:%M").ok()),
    })
}

/// What an open quote task has: its details, one ask per line.
fn asks_of(item: &OpenItem) -> Vec<String> {
    item.details
        .as_deref()
        .unwrap_or("")
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

/// A quote task's details with `asks` added: one per line, each once
/// (without case), the newest [`MAX_DETAIL_LINES`]; none when empty.
pub fn merged_details(details: Option<&str>, asks: &[String]) -> Option<String> {
    let mut lines: Vec<String> = details
        .unwrap_or("")
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();
    for ask in asks {
        if !lines
            .iter()
            .any(|line| line.to_lowercase() == ask.to_lowercase())
        {
            lines.push(ask.clone());
        }
    }
    let older = lines.len().saturating_sub(MAX_DETAIL_LINES);
    (lines.len() > older).then(|| lines[older..].join("\n"))
}

/// Drops what is already open, and what is listed twice (titles compared
/// without case); the quote task keeps only the asks the customer's open
/// quote task does not have.
pub fn without_repeats(reading: &mut Reading, open: &[OpenItem]) {
    let mut seen: Vec<String> = open
        .iter()
        .filter(|item| item.same_customer)
        .map(|item| format!("{}:{}", item.kind, item.title.to_lowercase()))
        .collect();
    let mut first = |key: String| {
        if seen.contains(&key) {
            false
        } else {
            seen.push(key);
            true
        }
    };
    reading
        .promises
        .retain(|promise| first(format!("promise:{}", promise.title.to_lowercase())));
    reading
        .approvals
        .retain(|approval| first(format!("approval:{}", approval.title.to_lowercase())));
    if let Some(quote) = reading.quote.as_mut() {
        let asked: Vec<String> = open
            .iter()
            .filter(|item| item.kind == "quote" && item.same_customer)
            .flat_map(asks_of)
            .map(|ask| ask.to_lowercase())
            .collect();
        quote
            .asks
            .retain(|ask| !asked.contains(&ask.to_lowercase()));
    }
}

fn parse_promises(items: &[Value]) -> Vec<Found> {
    items
        .iter()
        .filter_map(|item| {
            let title = clip(item.get("title")?.as_str()?, TITLE_MAX_CHARS);
            if title.is_empty() {
                return None;
            }
            let text = |key: &str| item.get(key).and_then(Value::as_str).map(str::trim);
            Some(Found {
                title,
                said: clip(text("said").unwrap_or(""), SAID_MAX_CHARS),
                due: text("due")
                    .and_then(|due| NaiveDateTime::parse_from_str(due, "%Y-%m-%d %H:%M").ok()),
                visit: item.get("visit").and_then(Value::as_bool).unwrap_or(false),
                channel: match text("message") {
                    Some("sms") => Some(Channel::Sms),
                    Some("email") => Some(Channel::Email),
                    _ => None,
                },
                link: match text("link") {
                    Some("inventory") => Some(Link::Inventory),
                    Some("quote") => Some(Link::Quote),
                    _ => None,
                },
                action: text("action").and_then(CrmAction::parse),
            })
        })
        .take(MAX_PROMISES)
        .collect()
}

/// `2026-10-28 15:06 (Wed)` in the company's time zone.
pub fn local_time(utc: NaiveDateTime, tz: Tz) -> String {
    tz.from_utc_datetime(&utc)
        .format("%Y-%m-%d %H:%M (%a)")
        .to_string()
}

/// A local due time in UTC (the earlier one when the clock was set back).
pub fn due_utc(local: NaiveDateTime, tz: Tz) -> Option<NaiveDateTime> {
    tz.from_local_datetime(&local)
        .earliest()
        .map(|at| at.naive_utc())
}

/// What the AI is asked to read.
pub fn extraction_prompt(
    source: Source,
    rep_name: &str,
    when: &str,
    now: &str,
    open: &[OpenItem],
    text: &str,
) -> String {
    let what = match source {
        Source::Call => "PHONE CALL TRANSCRIPT",
        Source::Sms => "TEXTS, oldest first (the last one is the rep's)",
        Source::Email => "EMAIL THE REP SENT",
    };
    let listed = |kind: &str| {
        let lines: Vec<String> = open
            .iter()
            .filter(|item| item.kind == kind && item.same_customer)
            .map(|item| match item.details.as_deref().map(str::trim) {
                Some(details) if !details.is_empty() => format!(
                    "- #{} {} (has: {})",
                    item.id,
                    item.title,
                    details.lines().collect::<Vec<_>>().join("; ")
                ),
                _ => format!("- #{} {}", item.id, item.title),
            })
            .collect();
        if lines.is_empty() {
            "(none)".to_string()
        } else {
            lines.join("\n")
        }
    };
    let others: Vec<String> = open
        .iter()
        .filter(|item| item.kind == "promise" && !item.same_customer)
        .map(|item| format!("- #{} {} — for {}", item.id, item.title, item.who))
        .collect();
    let not_on_file = if others.is_empty() {
        String::new()
    } else {
        format!(
            "OPEN PROMISES TO CALLERS NOT ON FILE (other people unless this is clearly to them):\n{}\n",
            others.join("\n")
        )
    };
    format!(
        "SALES REP: {rep_name}\nWHEN: {when}\nNOW: {now}\nOPEN PROMISES (already on the rep's list):\n{}\nOPEN QUOTE TASK (already on the list, with the asks it has):\n{}\nOPEN APPROVALS (sent, waiting for the customer's answer):\n{}\n{not_on_file}\n{what}:\n{text}",
        listed("promise"),
        listed("quote"),
        listed("approval"),
    )
}

/// The links a promised message may carry.
#[derive(Debug, Clone)]
pub struct Links {
    pub inventory: String,
    pub quote: String,
}

/// What the AI Reply is asked for a promise's message (its custom request).
pub fn draft_request(found: &Found, source: Source, when: &str, links: &Links) -> String {
    let place = match source {
        Source::Call => "on our call",
        Source::Sms => "in my text",
        Source::Email => "in my email",
    };
    let link = match found.link {
        Some(Link::Inventory) => format!("\nInclude this link to our stones: {}", links.inventory),
        Some(Link::Quote) => format!(
            "\nInclude this link to our online quote form, where they type their measurements and send them to us: {}",
            links.quote
        ),
        None => String::new(),
    };
    let said = if found.said.is_empty() {
        String::new()
    } else {
        format!(" My words: \"{}\".", found.said)
    };
    format!(
        "Write the message that keeps what I promised this customer {place} ({when}): {}.{said}{link}\nOnly this; bring up nothing else.",
        found.title
    )
}

static QUOTED_REPLY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^(?:>|On .{1,200}wrote:|-{2,} ?Original Message|From: )").expect("regex")
});

/// An email body without the earlier messages quoted under it.
pub fn without_quoted(body: &str) -> &str {
    split_quoted(body).0
}

/// An email body as the rep's new text and the earlier messages quoted under it.
pub fn split_quoted(body: &str) -> (&str, &str) {
    QUOTED_REPLY.find(body).map_or((body.trim(), ""), |found| {
        (body[..found.start()].trim(), body[found.start()..].trim())
    })
}

/// What a run did.
#[derive(Debug, Default)]
pub struct JobRun {
    /// Calls, texts and emails newly queued.
    pub queued: u64,
    /// Read (with or without promises).
    pub read: usize,
    /// Promises, quote tasks and approvals saved.
    pub promises: usize,
    /// Due promises, and approvals with no reply, the rep was reminded of.
    pub reminded: usize,
    /// Morning lists sent.
    pub digests: usize,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct Scan {
    id: u64,
    company_id: i32,
    user_id: i32,
    source: String,
    source_key: String,
    happened_at: NaiveDateTime,
    customer_digits: Option<String>,
    customer_email: Option<String>,
    recording_link: Option<String>,
    transcript: Option<String>,
}

impl Scan {
    /// Whose call or message it is, as `<user>:<phone, else email, else its
    /// own key>` (the `busy` query in [`run_job`] builds the same).
    fn customer_key(&self) -> String {
        let customer = self
            .customer_digits
            .as_deref()
            .or(self.customer_email.as_deref())
            .unwrap_or(&self.source_key);
        format!("{}:{customer}", self.user_id)
    }
}

/// How one scan went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// Read; this many promises saved.
    Read(usize),
    /// Left for a later run (a claim lost, a failure with attempts left, or
    /// AI not set up here).
    Later,
}

/// What a scan gives the AI to read.
enum Text {
    Read(String),
    /// Nothing to read (no recording, no speech, an empty message).
    Nothing,
    Later,
}

pub async fn run_job(state: &AppState) -> sqlx::Result<JobRun> {
    let pool = &state.pool;
    // Reminders and the morning list first: reading calls takes longer.
    let reminded = remind_due(state).await.unwrap_or_else(|err| {
        tracing::error!(error = %err, "promise reminders failed; continuing");
        0
    });
    let reminded = reminded
        + check_approvals(state).await.unwrap_or_else(|err| {
            tracing::error!(error = %err, "approval checks failed; continuing");
            0
        });
    if let Err(err) = close_finished(pool).await {
        tracing::error!(error = %err, "finished tasks not closed; continuing");
    }
    let digests = send_digests(state).await.unwrap_or_else(|err| {
        tracing::error!(error = %err, "morning lists failed; continuing");
        0
    });
    let mut run = JobRun {
        queued: queue(pool).await?,
        reminded,
        digests,
        ..JobRun::default()
    };
    sqlx::query(
        "UPDATE ai_promise_scans SET status = 'failed', claimed_at = NULL
          WHERE status = 'pending' AND attempts >= ?
            AND (claimed_at IS NULL OR claimed_at < UTC_TIMESTAMP() - INTERVAL ? MINUTE)",
    )
    .bind(MAX_ATTEMPTS)
    .bind(CLAIM_TTL_MINUTES)
    .execute(pool)
    .await?;
    let waiting: Vec<Scan> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT id, company_id, user_id, CAST(source AS CHAR) AS source, source_key, happened_at,
                customer_digits, customer_email, recording_link, transcript
           FROM ai_promise_scans
          WHERE {DUE}
            AND user_id IN (SELECT id FROM users WHERE ai_promises = 1 AND is_deleted = 0)
          ORDER BY happened_at, id LIMIT ?"
    )))
    .bind(MAX_ATTEMPTS)
    .bind(CLAIM_TTL_MINUTES)
    .bind(CANDIDATES_PER_RUN)
    .fetch_all(pool)
    .await?;
    // One customer's calls and messages are read one at a time, oldest
    // first, so each sees the promises the one before it saved or kept: a
    // customer with a scan held by a run (or waiting out a failed attempt)
    // waits too.
    let busy: Vec<String> = sqlx::query_scalar(
        "SELECT CONCAT(user_id, ':', COALESCE(customer_digits, customer_email, source_key))
           FROM ai_promise_scans
          WHERE status = 'pending' AND claimed_at >= UTC_TIMESTAMP() - INTERVAL ? MINUTE",
    )
    .bind(CLAIM_TTL_MINUTES)
    .fetch_all(pool)
    .await?;
    let mut taken: std::collections::HashSet<String> = busy.into_iter().collect();
    let due: Vec<Scan> = waiting
        .into_iter()
        .filter(|scan| taken.insert(scan.customer_key()))
        .take(usize::try_from(SCANS_PER_RUN).unwrap_or(4))
        .collect();
    let tasks: Vec<_> = due
        .into_iter()
        .map(|scan| {
            let state = state.clone();
            tokio::spawn(async move { scan_one(&state, &scan).await })
        })
        .collect();
    // Every scan finishes before the run answers (the Lambda freezes after),
    // even when one failed.
    let mut failed = None;
    for task in tasks {
        match task.await {
            Ok(Ok(Outcome::Read(saved))) => {
                run.read += 1;
                run.promises += saved;
            }
            Ok(Ok(Outcome::Later)) => {}
            Ok(Err(err)) => {
                failed.get_or_insert(err);
            }
            Err(err) => tracing::error!(error = %err, "promise scan task panicked"),
        }
    }
    failed.map_or(Ok(run), Err)
}

/// SQL condition: `column` (a customer id) is task `p`'s customer: its
/// `customer_id`, else (a caller or address on no customer then) a customer
/// with its phone or email now.
fn of_task_customer(column: &str) -> String {
    format!(
        "({column} = p.customer_id
          OR (p.customer_id IS NULL
              AND ({column} IN (SELECT i.customer_id FROM customer_phone_index i
                                 WHERE i.company_id = p.company_id AND i.last10 = p.customer_digits)
                   OR {column} IN (SELECT ce.customer_id FROM customers_emails ce
                                    WHERE LOWER(TRIM(ce.email)) = p.customer_email))))"
    )
}

/// SQL: task `p`'s phone (10 digits), else its customer `c`'s.
const TASK_DIGITS: &str =
    "COALESCE(p.customer_digits, RIGHT(REGEXP_REPLACE(COALESCE(c.phone, ''), '[^0-9]', ''), 10))";

/// Step 8: the open tasks the rep finished in the CRM after they were said
/// (for the reps AI promises is on for), closed:
/// - a quote task once one of the customer's quotes is saved (a new
///   version) or emailed after its last ask;
/// - a promise to send a contract once a document is sent them to sign;
/// - a promise to put something on the schedule once an event for the
///   customer is made, or one is on the schedule within 12 hours of the
///   appointment's time;
/// - a promise to call once a call of [`MIN_CALL_SECONDS`] or more with them
///   starts 5 minutes or more after it was said (not the call it was made on).
///
/// Plain reads first, then updates by id: an `UPDATE` with these subqueries
/// would hold shared locks on the quotes, events and calls it reads.
pub async fn close_finished(pool: &MySqlPool) -> sqlx::Result<u64> {
    let quotes = of_task_customer("q.customer_id");
    let signed = of_task_customer("s.customer_id");
    let events = of_task_customer("e.customer_id");
    let finished: Vec<u64> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT p.id FROM ai_promises p
           JOIN users u ON u.id = p.user_id AND u.ai_promises = 1 AND u.is_deleted = 0
           LEFT JOIN customers c ON c.id = p.customer_id
          WHERE p.status = 'open'
            AND ((p.kind = 'quote'
                  AND EXISTS (SELECT 1 FROM quotes q
                               WHERE q.company_id = p.company_id AND q.deleted_at IS NULL AND {quotes}
                                 AND (EXISTS (SELECT 1 FROM quote_revisions r
                                               WHERE r.quote_id = q.id AND r.created_at > p.promised_at)
                                      OR EXISTS (SELECT 1 FROM quote_emails m
                                                  WHERE m.quote_id = q.id AND m.sent_at > p.promised_at))))
              OR (p.crm_action = 'contract'
                  AND EXISTS (SELECT 1 FROM signature_requests s
                               WHERE s.company_id = p.company_id AND {signed}
                                 AND s.status <> 'draft' AND s.created_at > p.promised_at))
              OR (p.crm_action = 'schedule'
                  AND EXISTS (SELECT 1 FROM events e
                               WHERE {events} AND e.deleted_date IS NULL
                                 AND COALESCE(e.unscheduled, 0) = 0
                                 AND (e.created_date > p.promised_at
                                      OR e.start_date BETWEEN p.due_at - INTERVAL 12 HOUR
                                                          AND p.due_at + INTERVAL 12 HOUR)))
              OR (p.crm_action = 'call'
                  AND (EXISTS (SELECT 1 FROM provider_calls k
                                WHERE k.company_id = p.company_id AND k.customer_digits = {TASK_DIGITS}
                                  AND k.started_at >= p.promised_at + INTERVAL 5 MINUTE
                                  AND CAST(COALESCE(k.item->>'$.Cdr.talking_time', '0') AS UNSIGNED)
                                      >= {MIN_CALL_SECONDS})
                       OR EXISTS (SELECT 1 FROM phone_calls k
                                   WHERE k.company_id = p.company_id AND k.other_last10 = {TASK_DIGITS}
                                     AND k.started_at >= p.promised_at + INTERVAL 5 MINUTE
                                     AND k.duration_secs >= {MIN_CALL_SECONDS}))))
          LIMIT 50"
    )))
    .fetch_all(pool)
    .await?;
    let mut closed = 0;
    for id in finished {
        closed += sqlx::query(
            "UPDATE ai_promises SET status = 'done', closed_at = UTC_TIMESTAMP()
              WHERE id = ? AND status = 'open'",
        )
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    }
    Ok(closed)
}

/// A due promise to remind the rep of.
#[derive(Debug, sqlx::FromRow)]
struct DuePromise {
    id: u64,
    user_id: i32,
    customer_name: Option<String>,
    customer_digits: Option<String>,
    title: String,
    said: String,
    /// The rep gets activity reminders on Telegram.
    telegram: bool,
}

/// Step 5: the open promises and quote tasks that came due, each once: a
/// bell notification (`ai_promise_due`) pushed to the rep's phones, else sent
/// to their Telegram when they get activity reminders there (as deal
/// activity reminders are).
/// One more than a day past due when first seen (the job was off) is not.
/// The notification names the customer in its text, not by `customer_id`: an
/// open notification on a customer holds back the CRM's own reminders for
/// them ("Please text …").
pub async fn remind_due(state: &AppState) -> sqlx::Result<usize> {
    let pool = &state.pool;
    let due: Vec<DuePromise> = sqlx::query_as(
        "SELECT p.id, p.user_id, c.name AS customer_name, p.customer_digits, p.title, p.said,
                COALESCE(u.telegram_activity_notifications, 0) <> 0 AS telegram
           FROM ai_promises p
           JOIN users u ON u.id = p.user_id AND u.ai_promises = 1 AND u.is_deleted = 0
           LEFT JOIN customers c ON c.id = p.customer_id
          WHERE p.status = 'open' AND p.kind IN ('promise', 'quote') AND p.reminded_at IS NULL
            AND p.due_at <= UTC_TIMESTAMP() AND p.due_at > UTC_TIMESTAMP() - INTERVAL 1 DAY
          ORDER BY p.due_at LIMIT 20",
    )
    .fetch_all(pool)
    .await?;
    let mut sent = 0;
    for promise in due {
        let claimed = sqlx::query(
            "UPDATE ai_promises SET reminded_at = UTC_TIMESTAMP() WHERE id = ? AND reminded_at IS NULL",
        )
        .bind(promise.id)
        .execute(pool)
        .await?
        .rows_affected();
        if claimed == 0 {
            continue;
        }
        let customer = who(
            promise.customer_name.as_deref(),
            promise.customer_digits.as_deref(),
        );
        let message = clip(&format!("Promise due: {} — {customer}", promise.title), 255);
        let mut text = format!("⏰ {message}");
        if !promise.said.is_empty() {
            text.push_str(&format!("\nYou said: \"{}\"", promise.said));
        }
        notify(
            state,
            promise.user_id,
            "ai_promise_due",
            &message,
            &text,
            promise.telegram,
        )
        .await?;
        sent += 1;
    }
    Ok(sent)
}

/// Something sent for the customer's approval whose wait is over.
#[derive(Debug, sqlx::FromRow)]
struct SentForApproval {
    id: u64,
    user_id: i32,
    company_id: i32,
    customer_id: Option<i32>,
    customer_name: Option<String>,
    customer_digits: Option<String>,
    customer_email: Option<String>,
    promised_at: NaiveDateTime,
    title: String,
    /// The rep gets activity reminders on Telegram.
    telegram: bool,
}

/// Step 7: what was sent for the customer's approval [`APPROVAL_WAIT_HOURS`]
/// ago, each once: closed when the customer has replied since (a text, an
/// email, a call of [`MIN_CALL_SECONDS`] or more), else a bell notification
/// (`ai_promise_approval`) reminds the rep to check with them, as
/// [`remind_due`] does; it stays on the list until done or dismissed.
pub async fn check_approvals(state: &AppState) -> sqlx::Result<usize> {
    let pool = &state.pool;
    let due: Vec<SentForApproval> = sqlx::query_as(
        "SELECT p.id, p.user_id, p.company_id, p.customer_id, c.name AS customer_name,
                p.customer_digits, p.customer_email, p.promised_at, p.title,
                COALESCE(u.telegram_activity_notifications, 0) <> 0 AS telegram
           FROM ai_promises p
           JOIN users u ON u.id = p.user_id AND u.ai_promises = 1 AND u.is_deleted = 0
           LEFT JOIN customers c ON c.id = p.customer_id
          WHERE p.status = 'open' AND p.kind = 'approval' AND p.reminded_at IS NULL
            AND p.due_at <= UTC_TIMESTAMP() AND p.due_at > UTC_TIMESTAMP() - INTERVAL 1 DAY
          ORDER BY p.due_at LIMIT 20",
    )
    .fetch_all(pool)
    .await?;
    let mut sent = 0;
    for approval in due {
        let claimed = sqlx::query(
            "UPDATE ai_promises SET reminded_at = UTC_TIMESTAMP() WHERE id = ? AND reminded_at IS NULL",
        )
        .bind(approval.id)
        .execute(pool)
        .await?
        .rows_affected();
        if claimed == 0 {
            continue;
        }
        if customer_replied(pool, &approval).await? {
            sqlx::query(
                "UPDATE ai_promises SET status = 'done', closed_at = UTC_TIMESTAMP()
                  WHERE id = ? AND status = 'open'",
            )
            .bind(approval.id)
            .execute(pool)
            .await?;
            continue;
        }
        let customer = who(
            approval.customer_name.as_deref(),
            approval.customer_digits.as_deref(),
        );
        let message = clip(
            &format!("No reply yet: {} — {customer}", approval.title),
            255,
        );
        let text = format!("⏳ {message}\nSent a day ago. Check with them.");
        notify(
            state,
            approval.user_id,
            "ai_promise_approval",
            &message,
            &text,
            approval.telegram,
        )
        .await?;
        sent += 1;
    }
    Ok(sent)
}

/// The customer texted, emailed or talked on the phone with the company
/// since the approval was sent: by its phone (else their customer's) and its
/// email (and their customer's addresses).
async fn customer_replied(pool: &MySqlPool, approval: &SentForApproval) -> sqlx::Result<bool> {
    let digits = match approval.customer_digits.clone() {
        Some(digits) => Some(digits),
        None => match approval.customer_id {
            Some(id) => sqlx::query_scalar::<_, Option<String>>(
                "SELECT RIGHT(REGEXP_REPLACE(COALESCE(phone, ''), '[^0-9]', ''), 10)
                   FROM customers WHERE id = ?",
            )
            .bind(id)
            .fetch_optional(pool)
            .await?
            .flatten(),
            None => None,
        },
    }
    .filter(|digits| digits.len() == 10);
    if let Some(digits) = digits.as_deref() {
        for table in SMS_TABLES {
            let texted: Option<i64> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT 1 FROM {table}
                  WHERE company_id = ? AND phone_digits IN (?, CONCAT('1', ?))
                    AND direction = 'inbound' AND COALESCE(is_echo, 0) = 0
                    AND created_date > ? LIMIT 1"
            )))
            .bind(approval.company_id)
            .bind(digits)
            .bind(digits)
            .bind(approval.promised_at)
            .fetch_optional(pool)
            .await?;
            if texted.is_some() {
                return Ok(true);
            }
        }
        let called: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM provider_calls
              WHERE company_id = ? AND customer_digits = ? AND started_at > ?
                AND CAST(COALESCE(item->>'$.Cdr.talking_time', '0') AS UNSIGNED) >= ?
             UNION ALL
             SELECT 1 FROM phone_calls
              WHERE company_id = ? AND RIGHT(REGEXP_REPLACE(other_number, '[^0-9]', ''), 10) = ?
                AND started_at > ? AND duration_secs >= ?
             LIMIT 1",
        )
        .bind(approval.company_id)
        .bind(digits)
        .bind(approval.promised_at)
        .bind(MIN_CALL_SECONDS)
        .bind(approval.company_id)
        .bind(digits)
        .bind(approval.promised_at)
        .bind(MIN_CALL_SECONDS)
        .fetch_optional(pool)
        .await?;
        if called.is_some() {
            return Ok(true);
        }
    }
    let emailed: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM emails e
          WHERE e.company_id = ? AND e.sender_user_id IS NULL AND e.deleted_at IS NULL
            AND COALESCE(e.is_draft, 0) = 0 AND e.sent_at > ?
            AND (LOWER(TRIM(e.sender_email)) = ?
                 OR LOWER(TRIM(e.sender_email)) IN
                    (SELECT LOWER(TRIM(ce.email)) FROM customers_emails ce WHERE ce.customer_id = ?))
          LIMIT 1",
    )
    .bind(approval.company_id)
    .bind(approval.promised_at)
    .bind(approval.customer_email.as_deref())
    .bind(approval.customer_id)
    .fetch_optional(pool)
    .await?;
    Ok(emailed.is_some())
}

/// A bell notification of `kind` pushed to the user's phones; when no phone
/// took it, `text` goes to their Telegram if `telegram`.
async fn notify(
    state: &AppState,
    user_id: i32,
    kind: &str,
    message: &str,
    text: &str,
    telegram: bool,
) -> sqlx::Result<()> {
    let id = sqlx::query(
        "INSERT INTO notifications (user_id, message, notification_type, due_at, is_done, created_at)
         VALUES (?, ?, ?, NOW(), 0, NOW())",
    )
    .bind(user_id)
    .bind(message)
    .bind(kind)
    .execute(&state.pool)
    .await?
    .last_insert_id();
    let phones =
        crate::phone::crm_push::push_notification(state, i64::try_from(id).unwrap_or(i64::MAX))
            .await
            .unwrap_or_else(|err| {
                tracing::warn!(user_id, kind, error = %err, "promise notice not pushed");
                0
            });
    if phones == 0 && telegram && !state.webhooks.notify_telegram(user_id, text).await {
        tracing::warn!(user_id, kind, "promise notice not sent to Telegram");
    }
    Ok(())
}

/// The morning list goes out from this hour, the rep's time (until 4 hours
/// later, so a late run still sends it, but never in the evening).
pub const DIGEST_HOUR: u32 = 8;

/// An open promise, quote task or approval for the morning list.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ListedPromise {
    pub kind: String,
    pub title: String,
    pub customer_name: Option<String>,
    pub customer_digits: Option<String>,
    pub due_at: Option<NaiveDateTime>,
}

/// A customer whose text waits for the rep's reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Waiting {
    pub phone_digits: String,
    pub customer_id: Option<i32>,
    pub customer_name: Option<String>,
    /// Their last text.
    pub text: String,
    /// When they sent it (UTC).
    pub at: NaiveDateTime,
}

/// Step 6: each rep's morning list, once a day from [`DIGEST_HOUR`] their
/// time, as a bell notification (`ai_promise_digest`) pushed to their phones,
/// else the whole list to their Telegram. Nothing on it: nothing is sent.
pub async fn send_digests(state: &AppState) -> sqlx::Result<usize> {
    let pool = &state.pool;
    let reps: Vec<(i32, i32, bool)> = sqlx::query_as(
        "SELECT id, company_id, COALESCE(telegram_activity_notifications, 0) <> 0
           FROM users WHERE ai_promises = 1 AND is_deleted = 0",
    )
    .fetch_all(pool)
    .await?;
    let mut sent = 0;
    for (user_id, company_id, telegram) in reps {
        let config = company::load(pool, company_id).await.unwrap_or_else(|err| {
            tracing::warn!(company_id, error = %err, "morning list: company time zone unreadable");
            None
        });
        let tz = company::time_zone_of(config.as_ref());
        let now = Utc::now().with_timezone(&tz);
        if !(DIGEST_HOUR..DIGEST_HOUR + 4).contains(&now.hour()) {
            continue;
        }
        let claimed = sqlx::query(
            "INSERT IGNORE INTO ai_promise_digests (user_id, digest_date) VALUES (?, ?)",
        )
        .bind(user_id)
        .bind(now.date_naive())
        .execute(pool)
        .await?
        .rows_affected();
        if claimed == 0 {
            continue;
        }
        let promises: Vec<ListedPromise> = sqlx::query_as(
            "SELECT CAST(p.kind AS CHAR) AS kind, p.title, c.name AS customer_name,
                    p.customer_digits, p.due_at
               FROM ai_promises p
               LEFT JOIN customers c ON c.id = p.customer_id
              WHERE p.user_id = ? AND p.company_id = ? AND p.status = 'open'
              ORDER BY p.due_at IS NULL, p.due_at, p.promised_at DESC
              LIMIT 100",
        )
        .bind(user_id)
        .bind(company_id)
        .fetch_all(pool)
        .await?;
        let waiting = waiting_customers(pool, company_id, user_id).await?;
        let Some((summary, text)) = morning_list(&promises, &waiting, now.naive_utc(), tz) else {
            continue;
        };
        notify(
            state,
            user_id,
            "ai_promise_digest",
            &summary,
            &text,
            telegram,
        )
        .await?;
        sent += 1;
    }
    Ok(sent)
}

/// The customer's name, else their phone, else "a customer".
fn who(name: Option<&str>, digits: Option<&str>) -> String {
    if let Some(name) = name.map(str::trim).filter(|name| !name.is_empty()) {
        return name.to_string();
    }
    match digits {
        Some(d) if d.len() == 10 && d.bytes().all(|b| b.is_ascii_digit()) => {
            format!("({}) {}-{}", &d[..3], &d[3..6], &d[6..])
        }
        Some(d) if !d.is_empty() => d.to_string(),
        _ => "a customer".to_string(),
    }
}

/// The morning list: its one-line summary (the bell) and the whole list (for
/// Telegram); `None` when there is nothing on it. `now` is UTC.
pub fn morning_list(
    promises: &[ListedPromise],
    waiting: &[Waiting],
    now: NaiveDateTime,
    tz: Tz,
) -> Option<(String, String)> {
    let local = |at: NaiveDateTime| tz.from_utc_datetime(&at);
    let today = local(now).date_naive();
    let mut overdue = Vec::new();
    let mut due_today = Vec::new();
    let mut undated = Vec::new();
    let mut quotes = Vec::new();
    let mut approvals = Vec::new();
    for promise in promises {
        let line = format!(
            "{} — {}",
            promise.title,
            who(
                promise.customer_name.as_deref(),
                promise.customer_digits.as_deref()
            )
        );
        if promise.kind == "quote" {
            quotes.push(line);
            continue;
        }
        if promise.kind == "approval" {
            approvals.push(line);
            continue;
        }
        match promise.due_at {
            Some(due) if due < now => {
                overdue.push(format!(
                    "{line} (due {})",
                    local(due).format("%b %-d, %-I:%M %p")
                ));
            }
            Some(due) if local(due).date_naive() == today => {
                due_today.push(format!("{} {line}", local(due).format("%-I:%M %p")));
            }
            Some(_) => {}
            None => undated.push(line),
        }
    }
    let waiting_lines: Vec<String> = waiting
        .iter()
        .map(|customer| {
            let hours = (now - customer.at).num_hours();
            let ago = if hours < 24 {
                format!("{} h", hours.max(1))
            } else {
                format!("{} days", hours / 24)
            };
            format!(
                "{} — \"{}\" ({ago})",
                who(
                    customer.customer_name.as_deref(),
                    Some(&customer.phone_digits)
                ),
                clip(&customer.text, 80)
            )
        })
        .collect();
    let sections = [
        ("Overdue", &overdue),
        ("Due today", &due_today),
        ("No time set", &undated),
        ("Quotes to make or update", &quotes),
        ("Waiting for the customer's approval", &approvals),
        ("Waiting for your reply", &waiting_lines),
    ];
    if sections.iter().all(|(_, lines)| lines.is_empty()) {
        return None;
    }
    let counts: Vec<String> = [
        (overdue.len(), "overdue"),
        (due_today.len(), "due today"),
        (undated.len(), "without a time"),
        (quotes.len(), "quotes to update"),
        (approvals.len(), "waiting for approval"),
        (waiting_lines.len(), "waiting for your reply"),
    ]
    .iter()
    .filter(|(count, _)| *count > 0)
    .map(|(count, what)| format!("{count} {what}"))
    .collect();
    let summary = clip(&format!("Morning list: {}", counts.join(", ")), 255);
    let mut text = format!(
        "☀️ Good morning! Your list for {}:",
        local(now).format("%a, %b %-d")
    );
    for (heading, lines) in sections {
        if lines.is_empty() {
            continue;
        }
        text.push_str(&format!("\n\n{heading} ({}):", lines.len()));
        for line in lines.iter().take(8) {
            text.push_str(&format!("\n• {line}"));
        }
        if lines.len() > 8 {
            text.push_str(&format!("\n• and {} more", lines.len() - 8));
        }
    }
    Some((summary, text))
}

/// A short text that ends a conversation ("Thank you!", "Ok", "Sounds good",
/// a lone emoji): no reply is owed.
pub fn is_closing(text: &str) -> bool {
    const CLOSERS: [&str; 22] = [
        "thanks",
        "thank you",
        "thank u",
        "thx",
        "ty",
        "ok",
        "okay",
        "k",
        "kk",
        "sounds good",
        "great",
        "perfect",
        "awesome",
        "cool",
        "got it",
        "will do",
        "you too",
        "bye",
        "ok thanks",
        "ok thank you",
        "have a good day",
        "have a great day",
    ];
    let words = text
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .to_lowercase();
    let words = words.split_whitespace().collect::<Vec<_>>().join(" ");
    words.is_empty()
        || (words.len() <= 30 && (CLOSERS.contains(&words.as_str()) || words.starts_with("thank")))
}

/// The customers whose last text (in the last 3 days, over 30 minutes ago)
/// waits for the rep's reply, oldest wait first, at most 10: threads the rep
/// has texted in the last 60 days whose last row (echoes aside, whichever
/// rep's) is the customer's, sent to the rep or untagged — tagged with an
/// agent id the rep's own texts carry, as their "Mine" texts show it. A
/// closing text ([`is_closing`]) waits for nothing.
pub async fn waiting_customers(
    pool: &MySqlPool,
    company_id: i32,
    user_id: i32,
) -> sqlx::Result<Vec<Waiting>> {
    let mut waiting = Vec::new();
    // Copies of a rep's texts on their new provider (`moves`) are not new.
    let original = not_a_copy("");
    for table in SMS_TABLES {
        // Granite Phone texts carry no agent: a thread is its line's.
        let to_rep = if table == "telnyx_sms" {
            String::new()
        } else {
            format!(
                "AND (COALESCE(m.agent, '') = ''
                      OR m.agent IN (SELECT a.agent FROM {table} a
                                      WHERE a.company_id = m.company_id AND a.sender_user_id = ?
                                        AND a.agent IS NOT NULL
                                        AND a.created_date > UTC_TIMESTAMP() - INTERVAL 60 DAY))"
            )
        };
        let mut query = sqlx::query_as::<_, (String, Option<String>, NaiveDateTime)>(
            sqlx::AssertSqlSafe(format!(
                "SELECT COALESCE(RIGHT(m.phone_digits, 10), ''), m.text, CAST(m.created_date AS DATETIME)
                   FROM {table} m
                   JOIN (SELECT phone_digits, MAX(id) AS last_id FROM {table}
                          WHERE company_id = ? AND COALESCE(is_echo, 0) = 0 AND {original}
                            AND created_date > UTC_TIMESTAMP() - INTERVAL 3 DAY
                          GROUP BY phone_digits) last ON last.last_id = m.id
                  WHERE m.direction = 'inbound'
                    AND m.created_date < UTC_TIMESTAMP() - INTERVAL 30 MINUTE
                    {to_rep}
                    AND EXISTS (SELECT 1 FROM {table} r
                                 WHERE r.company_id = m.company_id AND r.phone_digits = m.phone_digits
                                   AND r.sender_user_id = ? AND r.direction = 'outbound'
                                   AND COALESCE(r.is_echo, 0) = 0
                                   AND r.created_date > UTC_TIMESTAMP() - INTERVAL 60 DAY)
                  ORDER BY m.created_date LIMIT 40"
            )),
        )
        .bind(company_id);
        if !to_rep.is_empty() {
            query = query.bind(user_id);
        }
        let rows = query.bind(user_id).fetch_all(pool).await?;
        for (digits, text, at) in rows {
            let text = text.unwrap_or_default();
            if digits.len() != 10
                || is_closing(&text)
                || waiting.iter().any(|w: &Waiting| w.phone_digits == digits)
            {
                continue;
            }
            let customer = customer_by_digits(pool, company_id, &digits).await?;
            waiting.push(Waiting {
                phone_digits: digits,
                customer_id: customer.as_ref().map(|(id, _)| *id),
                customer_name: customer.and_then(|(_, name)| name),
                text,
                at,
            });
        }
    }
    waiting.sort_by_key(|w| w.at);
    waiting.truncate(10);
    Ok(waiting)
}

/// A call or message to queue.
#[derive(Debug, sqlx::FromRow)]
struct Candidate {
    company_id: i32,
    user_id: i32,
    source: String,
    source_key: String,
    happened_at: NaiveDateTime,
    customer_digits: Option<String>,
    customer_email: Option<String>,
    recording_link: Option<String>,
}

/// SQL condition: text `m` in `table` was sent by user `u`. A text sent in
/// the RingCentral or `CloudTalk` app carries no `sender_user_id`, only the
/// rep's agent.
fn sent_by(table: &str) -> String {
    let agent = match table {
        "ringcentral_sms" => "u.ringcentral_extension_id",
        "cloudtalk_sms" => "u.cloudtalk_agent_id",
        _ => return "m.sender_user_id = u.id".to_owned(),
    };
    format!("(m.sender_user_id = u.id OR (m.sender_user_id IS NULL AND m.agent = {agent}))")
}

/// Step 1: the reps' calls, texts and emails not queued yet. Plain reads
/// first, then one insert: an `INSERT … SELECT` would hold shared locks on
/// the calls and messages it reads, against their webhooks' writes.
async fn queue(pool: &MySqlPool) -> sqlx::Result<u64> {
    // `RingCentral`: the rep's extension; a call without one (an incoming
    // call to their number) by the number, only when no one else has it.
    // A talk time that is not a number counts as 0.
    let mut found: Vec<Candidate> = sqlx::query_as(
        "SELECT pc.company_id, u.id AS user_id, 'call' AS source,
                CONCAT(pc.provider, ':', pc.call_id) AS source_key, pc.started_at AS happened_at,
                pc.customer_digits, CAST(NULL AS CHAR) AS customer_email,
                pc.item->>'$.Cdr.recording_link' AS recording_link
           FROM users u
           JOIN provider_calls pc ON pc.company_id = u.company_id
          WHERE u.ai_promises = 1 AND u.is_deleted = 0
            AND pc.started_at > UTC_TIMESTAMP() - INTERVAL ? DAY
            AND pc.customer_digits IS NOT NULL
            AND COALESCE(pc.item->>'$.Cdr.recording_link', '') NOT IN ('', 'null')
            AND CAST(COALESCE(pc.item->>'$.Cdr.talking_time', '0') AS UNSIGNED) >= ?
            AND ((pc.provider = 'ringcentral'
                  AND (pc.item->>'$.Cdr.user_id' = u.ringcentral_extension_id
                       OR (COALESCE(pc.item->>'$.Cdr.user_id', '') IN ('', '0', 'null')
                           AND RIGHT(pc.item->>'$.Cdr.public_internal', 10) = RIGHT(u.ringcentral_phone_number, 10)
                           AND (SELECT COUNT(*) FROM users o
                                 WHERE o.company_id = u.company_id AND o.is_deleted = 0
                                   AND RIGHT(o.ringcentral_phone_number, 10) = RIGHT(u.ringcentral_phone_number, 10)) = 1)))
              OR (pc.provider = 'cloudtalk' AND pc.item->>'$.Agent.id' = u.cloudtalk_agent_id))
            AND NOT EXISTS (SELECT 1 FROM ai_promise_scans s
                             WHERE s.user_id = u.id AND s.source_key = CONCAT(pc.provider, ':', pc.call_id))",
    )
    .bind(LOOKBACK_DAYS)
    .bind(MIN_CALL_SECONDS)
    .fetch_all(pool)
    .await?;
    // Granite Phone: once its own job is not transcribing the call, whose
    // transcript is then reused.
    found.extend(
        sqlx::query_as::<_, Candidate>(
            "SELECT c.company_id, u.id AS user_id, 'call' AS source,
                    CONCAT('telnyx:', c.id) AS source_key, CAST(c.started_at AS DATETIME) AS happened_at,
                    RIGHT(REGEXP_REPLACE(c.other_number, '[^0-9]', ''), 10) AS customer_digits,
                    CAST(NULL AS CHAR) AS customer_email, CONCAT('tx_', c.id) AS recording_link
               FROM users u
               JOIN phone_calls c ON c.user_id = u.id AND c.company_id = u.company_id
              WHERE u.ai_promises = 1 AND u.is_deleted = 0
                AND c.started_at > UTC_TIMESTAMP() - INTERVAL ? DAY
                AND c.duration_secs >= ? AND c.recording_key IS NOT NULL
                AND c.transcript_status NOT IN ('pending', 'processing')
                AND NOT EXISTS (SELECT 1 FROM ai_promise_scans s
                                 WHERE s.user_id = u.id AND s.source_key = CONCAT('telnyx:', c.id))",
        )
        .bind(LOOKBACK_DAYS)
        .bind(MIN_CALL_SECONDS)
        .fetch_all(pool)
        .await?,
    );
    // A copy made when its rep changed provider (`moves`) was scanned as
    // its original.
    let original = not_a_copy("m");
    for table in SMS_TABLES {
        let sent_by = sent_by(table);
        found.extend(
            sqlx::query_as::<_, Candidate>(sqlx::AssertSqlSafe(format!(
                "SELECT m.company_id, u.id AS user_id, 'sms' AS source,
                        CONCAT('{table}:', m.id) AS source_key,
                        CAST(m.created_date AS DATETIME) AS happened_at,
                        RIGHT(m.phone_digits, 10) AS customer_digits,
                        CAST(NULL AS CHAR) AS customer_email, CAST(NULL AS CHAR) AS recording_link
                   FROM users u
                   JOIN {table} m ON {sent_by} AND m.company_id = u.company_id
                  WHERE u.ai_promises = 1 AND u.is_deleted = 0
                    AND m.direction = 'outbound' AND COALESCE(m.is_automated, 0) = 0
                    AND COALESCE(m.is_echo, 0) = 0 AND m.status <> 'failed' AND {original}
                    AND m.created_date > UTC_TIMESTAMP() - INTERVAL ? DAY
                    AND NOT EXISTS (SELECT 1 FROM ai_promise_scans s
                                     WHERE s.user_id = u.id AND s.source_key = CONCAT('{table}:', m.id))"
            )))
            .bind(LOOKBACK_DAYS)
            .fetch_all(pool)
            .await?,
        );
    }
    found.extend(
        sqlx::query_as::<_, Candidate>(
            "SELECT e.company_id, u.id AS user_id, 'email' AS source,
                    CONCAT('email:', e.id) AS source_key, e.sent_at AS happened_at,
                    CAST(NULL AS CHAR) AS customer_digits,
                    LOWER(TRIM(e.receiver_email)) AS customer_email,
                    CAST(NULL AS CHAR) AS recording_link
               FROM users u
               JOIN emails e ON e.sender_user_id = u.id AND e.company_id = u.company_id
              WHERE u.ai_promises = 1 AND u.is_deleted = 0
                AND COALESCE(e.is_draft, 0) = 0 AND COALESCE(e.is_automated, 0) = 0
                AND e.deleted_at IS NULL AND e.sent_at > UTC_TIMESTAMP() - INTERVAL ? DAY
                AND NOT EXISTS (SELECT 1 FROM ai_promise_scans s
                                 WHERE s.user_id = u.id AND s.source_key = CONCAT('email:', e.id))",
        )
        .bind(LOOKBACK_DAYS)
        .fetch_all(pool)
        .await?,
    );
    if found.is_empty() {
        return Ok(0);
    }
    // IGNORE: an overlapping run queued it first. A recording link or an
    // address too long for its column is cut, not refused.
    let mut insert = sqlx::QueryBuilder::new(
        "INSERT IGNORE INTO ai_promise_scans
                (company_id, user_id, source, source_key, happened_at, customer_digits,
                 customer_email, recording_link) ",
    );
    insert.push_values(found, |mut row, call| {
        row.push_bind(call.company_id)
            .push_bind(call.user_id)
            .push_bind(call.source)
            .push_bind(call.source_key)
            .push_bind(call.happened_at)
            .push_bind(call.customer_digits)
            .push_bind(call.customer_email)
            .push_bind(call.recording_link);
    });
    Ok(insert.build().execute(pool).await?.rows_affected())
}

/// Claims, reads and saves one scan's promises.
async fn scan_one(state: &AppState, scan: &Scan) -> sqlx::Result<Outcome> {
    let pool = &state.pool;
    let claimed = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE ai_promise_scans SET claimed_at = UTC_TIMESTAMP(), attempts = attempts + 1
          WHERE id = ? AND {DUE}"
    )))
    .bind(scan.id)
    .bind(MAX_ATTEMPTS)
    .bind(CLAIM_TTL_MINUTES)
    .execute(pool)
    .await?
    .rows_affected();
    if claimed == 0 {
        return Ok(Outcome::Later);
    }
    let (Some(user), Some(source)) = (
        find_live_user(pool, scan.user_id).await?,
        Source::parse(&scan.source),
    ) else {
        finish(pool, scan.id, "skipped").await?;
        return Ok(Outcome::Read(0));
    };
    let caller = Caller {
        feature: FEATURE,
        user_id: Some(user.id),
        company_id: Some(scan.company_id),
    };
    let text = match source {
        Source::Call => call_text(state, scan, caller).await?,
        Source::Sms => sms_text(pool, scan).await?,
        Source::Email => email_text(pool, scan).await?,
    };
    let text = match text {
        Text::Read(text) => text,
        Text::Nothing => {
            finish(pool, scan.id, "skipped").await?;
            return Ok(Outcome::Read(0));
        }
        Text::Later => return Ok(Outcome::Later),
    };
    let config = company::load(pool, scan.company_id)
        .await
        .unwrap_or_else(|err| {
            tracing::warn!(company_id = scan.company_id, error = %err, "promises: company time zone unreadable");
            None
        });
    let tz = company::time_zone_of(config.as_ref());
    let when = local_time(scan.happened_at, tz);
    let customer_id = customer_of(pool, scan).await?;
    let open: Vec<OpenItem> = sqlx::query_as(
        // The customer's, and the rep's recent promises to callers whose
        // number matched no customer (this message may be to one of them).
        "SELECT id, CAST(kind AS CHAR) AS kind, title, details,
                (customer_id = ? OR customer_digits = ? OR customer_email = ?) IS TRUE AS same_customer,
                COALESCE(customer_label, customer_digits, customer_email, '') AS who
           FROM ai_promises
          WHERE user_id = ? AND status = 'open'
            AND ((customer_id = ? OR customer_digits = ? OR customer_email = ?)
                 OR (kind = 'promise' AND promised_at > UTC_TIMESTAMP() - INTERVAL 3 DAY
                     AND (customer_id IS NULL OR ? IS NULL)))
          ORDER BY id DESC LIMIT 40",
    )
    .bind(customer_id)
    .bind(scan.customer_digits.as_deref())
    .bind(scan.customer_email.as_deref())
    .bind(user.id)
    .bind(customer_id)
    .bind(scan.customer_digits.as_deref())
    .bind(scan.customer_email.as_deref())
    .bind(customer_id)
    .fetch_all(pool)
    .await?;
    let mut text = start_and_end(&text, MAX_SOURCE_CHARS);
    // What the rep did next is often read first (or never tied to this
    // customer): a promise it already keeps is not listed.
    let later = later_messages(pool, scan, customer_id, tz).await?;
    if !later.is_empty() {
        text.push_str(&format!(
            "\n\nAFTER THIS (a promise these already keep is done: don't list it):\n{}",
            later.join("\n")
        ));
    }
    let prompt = extraction_prompt(
        source,
        user.name.as_deref().unwrap_or("the rep"),
        &when,
        &local_time(Utc::now().naive_utc(), tz),
        &open,
        &text,
    );
    let raw = match extract(state, caller, &prompt).await {
        Ok(raw) => raw,
        Err(AiError::NotConfigured) => {
            give_back(pool, scan.id).await?;
            return Ok(Outcome::Later);
        }
        Err(err) => {
            tracing::warn!(scan_id = scan.id, error = %err, "promises not read; retried later");
            return Ok(Outcome::Later);
        }
    };
    let mut reading = parse_reading(&raw, &open);
    without_repeats(&mut reading, &open);
    let found = &reading.promises;
    let links = links(pool, scan.company_id).await?;
    let mut drafts = Vec::with_capacity(found.len());
    for promise in found {
        let draft = match promise.channel {
            Some(channel) => {
                let request = draft_request(promise, source, &when, &links);
                draft_message(state, &user, scan, customer_id, channel, request).await
            }
            None => None,
        };
        drafts.push(draft);
    }
    // The customer's open quote task takes what was asked here (with nothing
    // new, it stays as it is); else it is a new task.
    let open_quote = open
        .iter()
        .filter(|item| item.kind == "quote" && item.same_customer)
        .max_by_key(|item| item.id);
    let quote = reading
        .quote
        .as_ref()
        .filter(|quote| open_quote.is_none() || !quote.asks.is_empty() || quote.due.is_some());
    let quote_id = match (customer_id, quote, open_quote) {
        (Some(customer_id), Some(_), None) => {
            newest_quote(pool, scan.company_id, customer_id).await?
        }
        _ => None,
    };
    let mut tx = pool.begin().await?;
    for (promise, draft) in found.iter().zip(&drafts) {
        sqlx::query(
            "INSERT INTO ai_promises
                    (company_id, user_id, scan_id, source, crm_action, promised_at, customer_id,
                     customer_digits, customer_email, customer_label, title, said, due_at, is_appointment,
                     channel, draft)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(scan.company_id)
        .bind(user.id)
        .bind(scan.id)
        .bind(source.as_str())
        .bind(promise.action.map(CrmAction::as_str))
        .bind(scan.happened_at)
        .bind(customer_id)
        .bind(scan.customer_digits.as_deref())
        .bind(scan.customer_email.as_deref())
        .bind(reading.customer_name.as_deref())
        .bind(&promise.title)
        .bind(&promise.said)
        .bind(promise.due.and_then(|due| due_utc(due, tz)))
        .bind(promise.visit)
        .bind(promise.channel.map(Channel::as_str))
        .bind(draft.as_deref())
        .execute(&mut *tx)
        .await?;
    }
    if let Some(quote) = quote {
        let due = quote.due.and_then(|due| due_utc(due, tz));
        match open_quote {
            Some(task) => {
                // The words and the time of the latest ask (assignments run
                // left to right: `promised_at` changes last).
                sqlx::query(
                    "UPDATE ai_promises
                        SET details = ?,
                            said = IF(? >= promised_at, ?, said),
                            source = IF(? >= promised_at, ?, source),
                            promised_at = GREATEST(promised_at, ?),
                            due_at = COALESCE(?, due_at),
                            reminded_at = IF(? IS NULL, reminded_at, NULL)
                      WHERE id = ? AND status = 'open'",
                )
                .bind(merged_details(task.details.as_deref(), &quote.asks))
                .bind(scan.happened_at)
                .bind(&quote.said)
                .bind(scan.happened_at)
                .bind(source.as_str())
                .bind(scan.happened_at)
                .bind(due)
                .bind(due)
                .bind(task.id)
                .execute(&mut *tx)
                .await?;
            }
            None => {
                sqlx::query(
                    "INSERT INTO ai_promises
                            (company_id, user_id, scan_id, source, kind, promised_at, customer_id,
                             customer_digits, customer_email, customer_label, quote_id, title, said,
                             details, due_at)
                     VALUES (?, ?, ?, ?, 'quote', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(scan.company_id)
                .bind(user.id)
                .bind(scan.id)
                .bind(source.as_str())
                .bind(scan.happened_at)
                .bind(customer_id)
                .bind(scan.customer_digits.as_deref())
                .bind(scan.customer_email.as_deref())
                .bind(reading.customer_name.as_deref())
                .bind(quote_id)
                .bind(&quote.title)
                .bind(&quote.said)
                .bind(merged_details(None, &quote.asks))
                .bind(due)
                .execute(&mut *tx)
                .await?;
            }
        }
    }
    for approval in &reading.approvals {
        sqlx::query(
            "INSERT INTO ai_promises
                    (company_id, user_id, scan_id, source, kind, promised_at, customer_id,
                     customer_digits, customer_email, customer_label, title, said, due_at)
             VALUES (?, ?, ?, ?, 'approval', ?, ?, ?, ?, ?, ?, ?, ? + INTERVAL ? HOUR)",
        )
        .bind(scan.company_id)
        .bind(user.id)
        .bind(scan.id)
        .bind(source.as_str())
        .bind(scan.happened_at)
        .bind(customer_id)
        .bind(scan.customer_digits.as_deref())
        .bind(scan.customer_email.as_deref())
        .bind(reading.customer_name.as_deref())
        .bind(&approval.title)
        .bind(&approval.said)
        .bind(scan.happened_at)
        .bind(APPROVAL_WAIT_HOURS)
        .execute(&mut *tx)
        .await?;
    }
    for id in &reading.kept {
        sqlx::query(
            "UPDATE ai_promises
                SET status = 'done', closed_at = UTC_TIMESTAMP(), closed_by_scan_id = ?
              WHERE id = ? AND user_id = ? AND status = 'open'",
        )
        .bind(scan.id)
        .bind(id)
        .bind(user.id)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("UPDATE ai_promise_scans SET status = 'done', claimed_at = NULL WHERE id = ?")
        .bind(scan.id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Outcome::Read(
        found.len() + usize::from(quote.is_some()) + reading.approvals.len(),
    ))
}

/// The customer's newest quote, the one their quote task is about.
async fn newest_quote(
    pool: &MySqlPool,
    company_id: i32,
    customer_id: i32,
) -> sqlx::Result<Option<i32>> {
    sqlx::query_scalar(
        "SELECT id FROM quotes
          WHERE company_id = ? AND customer_id = ? AND deleted_at IS NULL
          ORDER BY COALESCE(updated_at, created_date) DESC, id DESC LIMIT 1",
    )
    .bind(company_id)
    .bind(customer_id)
    .fetch_optional(pool)
    .await
}

/// The promises the AI finds, as its JSON reply.
async fn extract(state: &AppState, caller: Caller<'_>, prompt: &str) -> Result<String, AiError> {
    let chat = |model| {
        let mut chat = Chat::new(model)
            .message("system", PROMISES_SYSTEM_PROMPT)
            .message("user", prompt);
        chat.json = true;
        chat
    };
    match state.ai.chat(&state.pool, caller, &chat(MINI_MODEL)).await {
        Err(err) if err.is_model_permission() => {
            state
                .ai
                .chat(&state.pool, caller, &chat(MINI_FALLBACK_MODEL))
                .await
        }
        result => result,
    }
}

/// A call's transcript: the one kept on the scan, else its recording
/// transcribed (and kept).
async fn call_text(state: &AppState, scan: &Scan, caller: Caller<'_>) -> sqlx::Result<Text> {
    if let Some(text) = scan
        .transcript
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        return Ok(Text::Read(text.to_string()));
    }
    let Some((_, call_id)) = scan.source_key.split_once(':') else {
        return Ok(Text::Nothing);
    };
    match transcribe_call(state, caller, call_id, scan.recording_link.as_deref(), true).await {
        Ok(transcript) => {
            let text = transcript.labeled_text();
            if text.is_empty() {
                return Ok(Text::Nothing);
            }
            sqlx::query("UPDATE ai_promise_scans SET transcript = ? WHERE id = ?")
                .bind(&text)
                .bind(scan.id)
                .execute(&state.pool)
                .await?;
            Ok(Text::Read(text))
        }
        Err(
            CallTranscriptError::Empty
            | CallTranscriptError::NoSpeech
            | CallTranscriptError::NotAvailable,
        ) => Ok(Text::Nothing),
        Err(CallTranscriptError::Transcribe(TranscribeError::NotConfigured)) => {
            give_back(&state.pool, scan.id).await?;
            Ok(Text::Later)
        }
        Err(err) => {
            tracing::warn!(scan_id = scan.id, error = %err, "call not transcribed; retried later");
            Ok(Text::Later)
        }
    }
}

/// The rep's text with the texts before it, oldest first.
async fn sms_text(pool: &MySqlPool, scan: &Scan) -> sqlx::Result<Text> {
    let Some((table, id)) = scan.source_key.split_once(':') else {
        return Ok(Text::Nothing);
    };
    let (Some(table), Ok(id)) = (
        SMS_TABLES.iter().find(|known| **known == table),
        id.parse::<i64>(),
    ) else {
        return Ok(Text::Nothing);
    };
    let mut rows: Vec<(String, Option<String>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT CAST(o.direction AS CHAR), o.text
           FROM {table} m
           JOIN {table} o ON o.company_id = m.company_id AND o.phone_digits = m.phone_digits
                         AND (o.created_date < m.created_date OR o.id = m.id)
                         AND COALESCE(o.is_echo, 0) = 0
          WHERE m.id = ? AND m.company_id = ?
          ORDER BY o.created_date DESC, o.id DESC
          LIMIT ?"
    )))
    .bind(id)
    .bind(scan.company_id)
    .bind(SMS_CONTEXT_MESSAGES + 1)
    .fetch_all(pool)
    .await?;
    let own = rows
        .first()
        .and_then(|(_, text)| text.as_deref())
        .map_or("", str::trim);
    if own.is_empty() {
        return Ok(Text::Nothing);
    }
    rows.reverse();
    let lines: Vec<String> = rows
        .iter()
        .filter_map(|(direction, text)| {
            let text = text.as_deref().map(str::trim).filter(|t| !t.is_empty())?;
            let who = if direction == "inbound" {
                "Customer"
            } else {
                "Sales rep"
            };
            Some(format!("{who}: {text}"))
        })
        .collect();
    Ok(Text::Read(lines.join("\n")))
}

/// The email the rep wrote, without the messages it quotes.
async fn email_text(pool: &MySqlPool, scan: &Scan) -> sqlx::Result<Text> {
    let Some(id) = scan
        .source_key
        .strip_prefix("email:")
        .and_then(|id| id.parse::<i64>().ok())
    else {
        return Ok(Text::Nothing);
    };
    let email: Option<(Option<String>, Option<String>)> =
        sqlx::query_as("SELECT subject, body FROM emails WHERE id = ? AND company_id = ?")
            .bind(id)
            .bind(scan.company_id)
            .fetch_optional(pool)
            .await?;
    let Some((subject, body)) = email else {
        return Ok(Text::Nothing);
    };
    let (new_text, quoted) = split_quoted(body.as_deref().unwrap_or(""));
    if new_text.is_empty() {
        return Ok(Text::Nothing);
    }
    // What the rep answered (the customer's quote changes are often in it).
    let quoted = clip(quoted, QUOTED_MAX_CHARS);
    let quoted = if quoted.is_empty() {
        String::new()
    } else {
        format!("\n\nQUOTED BELOW IT (earlier messages, context only):\n{quoted}")
    };
    Ok(Text::Read(format!(
        "Subject: {}\n\n{new_text}{quoted}",
        subject.as_deref().map_or("", str::trim)
    )))
}

/// The customer with the scan's phone or email. An email whose address is
/// on no customer is the customer's of another recipient (To, then Cc, then
/// Bcc): a reply to the customer's husband with the customer copied keeps
/// her promises.
async fn customer_of(pool: &MySqlPool, scan: &Scan) -> sqlx::Result<Option<i32>> {
    if let Some(digits) = scan.customer_digits.as_deref().filter(|d| d.len() == 10) {
        return Ok(customer_by_digits(pool, scan.company_id, digits)
            .await?
            .map(|(id, _)| id));
    }
    if let Some(email) = scan.customer_email.as_deref().filter(|e| e.contains('@')) {
        let found: Option<i32> = sqlx::query_scalar(
            "SELECT c.id FROM customers c
               JOIN customers_emails ce ON ce.customer_id = c.id
              WHERE c.company_id = ? AND c.deleted_at IS NULL AND LOWER(TRIM(ce.email)) = ?
              ORDER BY c.id DESC LIMIT 1",
        )
        .bind(scan.company_id)
        .bind(email)
        .fetch_optional(pool)
        .await?;
        if found.is_some() {
            return Ok(found);
        }
    }
    let Some(email_id) = scan
        .source_key
        .strip_prefix("email:")
        .and_then(|id| id.parse::<i64>().ok())
    else {
        return Ok(None);
    };
    sqlx::query_scalar(
        "SELECT c.id FROM email_participants ep
           JOIN customers c ON c.company_id = ? AND c.deleted_at IS NULL
                AND (c.id = ep.customer_id
                     OR c.id IN (SELECT ce.customer_id FROM customers_emails ce
                                  WHERE LOWER(TRIM(ce.email)) = LOWER(TRIM(ep.email))))
          WHERE ep.email_id = ? AND ep.type IN ('to', 'cc', 'bcc')
          ORDER BY FIELD(ep.type, 'to', 'cc', 'bcc'), ep.position, c.id DESC
          LIMIT 1",
    )
    .bind(scan.company_id)
    .bind(email_id)
    .fetch_optional(pool)
    .await
}

/// The customer (id, name) with this phone's last 10 digits: through the
/// phone index Granite Phone keeps (`customer_phone_index`), else, for a
/// company it has not indexed yet, the customer records.
pub async fn customer_by_digits(
    pool: &MySqlPool,
    company_id: i32,
    digits: &str,
) -> sqlx::Result<Option<(i32, Option<String>)>> {
    let indexed: Option<(i32, Option<String>)> = sqlx::query_as(
        "SELECT c.id, c.name FROM customer_phone_index i
           JOIN customers c ON c.id = i.customer_id AND c.deleted_at IS NULL
          WHERE i.company_id = ? AND i.last10 = ?
          ORDER BY c.id DESC LIMIT 1",
    )
    .bind(company_id)
    .bind(digits)
    .fetch_optional(pool)
    .await?;
    if indexed.is_some() {
        return Ok(indexed);
    }
    let has_index: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM (SELECT 1 FROM customer_phone_index WHERE company_id = ? LIMIT 1) i",
    )
    .bind(company_id)
    .fetch_one(pool)
    .await?;
    if has_index > 0 {
        return Ok(None);
    }
    sqlx::query_as(
        "SELECT id, name FROM customers
          WHERE company_id = ? AND deleted_at IS NULL
            AND (RIGHT(REGEXP_REPLACE(COALESCE(phone, ''), '[^0-9]', ''), 10) = ?
              OR RIGHT(REGEXP_REPLACE(COALESCE(phone_2, ''), '[^0-9]', ''), 10) = ?)
          ORDER BY id DESC LIMIT 1",
    )
    .bind(company_id)
    .bind(digits)
    .bind(digits)
    .fetch_optional(pool)
    .await
}

/// What the rep sent the customer after a call, oldest first: texts to their
/// number and emails to their addresses (up to 5 of each), and later calls
/// with them (up to 3), as
/// `[2026-10-28 15:20 (Wed)] TEXT: …`.
async fn later_messages(
    pool: &MySqlPool,
    scan: &Scan,
    customer_id: Option<i32>,
    tz: Tz,
) -> sqlx::Result<Vec<String>> {
    let mut sent: Vec<(NaiveDateTime, String)> = Vec::new();
    if let Some(digits) = scan.customer_digits.as_deref() {
        let original = not_a_copy("m");
        for table in SMS_TABLES {
            let sent_by = sent_by(table);
            let rows: Vec<(Option<String>, NaiveDateTime)> =
                sqlx::query_as(sqlx::AssertSqlSafe(format!(
                    "SELECT m.text, CAST(m.created_date AS DATETIME) FROM {table} m
                       JOIN users u ON u.id = ?
                      WHERE m.company_id = ? AND m.phone_digits IN (?, CONCAT('1', ?))
                        AND {sent_by} AND m.direction = 'outbound'
                        AND COALESCE(m.is_echo, 0) = 0 AND m.created_date > ? AND {original}
                      ORDER BY m.created_date LIMIT 5"
                )))
                .bind(scan.user_id)
                .bind(scan.company_id)
                .bind(digits)
                .bind(digits)
                .bind(scan.happened_at)
                .fetch_all(pool)
                .await?;
            for (text, at) in rows {
                let text = text.unwrap_or_default();
                if !text.trim().is_empty() {
                    sent.push((
                        at,
                        format!("[{}] TEXT: {}", local_time(at, tz), clip(&text, 500)),
                    ));
                }
            }
        }
        // Later calls with them, already transcribed (a text read late can
        // be kept by a call read before it).
        let calls: Vec<(String, NaiveDateTime)> = sqlx::query_as(
            "SELECT transcript, happened_at FROM ai_promise_scans
              WHERE company_id = ? AND user_id = ? AND source = 'call'
                AND customer_digits = ? AND happened_at > ? AND transcript IS NOT NULL
              ORDER BY happened_at LIMIT 3",
        )
        .bind(scan.company_id)
        .bind(scan.user_id)
        .bind(digits)
        .bind(scan.happened_at)
        .fetch_all(pool)
        .await?;
        for (transcript, at) in calls {
            sent.push((
                at,
                format!(
                    "[{}] CALL: {}",
                    local_time(at, tz),
                    start_and_end(&transcript, 2_000)
                ),
            ));
        }
    }
    // Emails to the customer's addresses, and every email the rep wrote in
    // the 12 hours after (a customer often gets it at another address, like
    // their work one), with whom it went to, for the AI to tell.
    let rows: Vec<(i64, Option<String>, Option<String>, Option<String>, NaiveDateTime)> =
        sqlx::query_as(
            "SELECT e.id, e.receiver_email, e.subject, e.body, e.sent_at FROM emails e
              WHERE e.company_id = ? AND e.sender_user_id = ? AND e.deleted_at IS NULL
                AND COALESCE(e.is_draft, 0) = 0 AND COALESCE(e.is_automated, 0) = 0
                AND e.sent_at > ?
                AND (e.sent_at < ? + INTERVAL 12 HOUR
                     OR LOWER(TRIM(e.receiver_email)) IN
                        (SELECT LOWER(TRIM(ce.email)) FROM customers_emails ce WHERE ce.customer_id = ?))
              ORDER BY e.sent_at LIMIT 10",
        )
        .bind(scan.company_id)
        .bind(scan.user_id)
        .bind(scan.happened_at)
        .bind(scan.happened_at)
        .bind(customer_id)
        .fetch_all(pool)
        .await?;
    for (id, to, subject, body, at) in rows {
        // The scan's own email is what is being read.
        if scan.source_key == format!("email:{id}") {
            continue;
        }
        sent.push((
            at,
            format!(
                "[{}] EMAIL to {}: {}: {}",
                local_time(at, tz),
                to.as_deref().map_or("?", str::trim),
                subject.as_deref().map_or("", str::trim),
                clip(without_quoted(body.as_deref().unwrap_or("")), 500)
            ),
        ));
    }
    sent.sort_by_key(|(at, _)| *at);
    Ok(sent.into_iter().map(|(_, line)| line).collect())
}

async fn links(pool: &MySqlPool, company_id: i32) -> sqlx::Result<Links> {
    let domain: Option<Option<String>> =
        sqlx::query_scalar("SELECT domain FROM company WHERE id = ?")
            .bind(company_id)
            .fetch_optional(pool)
            .await?;
    let domain = domain.flatten();
    Ok(Links {
        inventory: customer_inventory_page_url(company_id, domain.as_deref()),
        quote: customer_quote_page_url(company_id, domain.as_deref()),
    })
}

/// The message that keeps a promise; none when the AI could not write it
/// (the promise is saved without one).
async fn draft_message(
    state: &AppState,
    user: &SessionUser,
    scan: &Scan,
    customer_id: Option<i32>,
    channel: Channel,
    request: String,
) -> Option<String> {
    let input = ConversationReplyInput {
        channel: channel.as_str().to_string(),
        customer_id,
        customer_email: scan.customer_email.clone(),
        phone_digits: scan.customer_digits.clone(),
        deal_id: None,
        mode: ReplyMode::Custom,
        custom_request: Some(request),
        previous_draft: None,
        revision_instruction: None,
        instruction_history: Vec::new(),
        include_instructions: None,
    };
    match conversation_reply::draft(state, user, &input, FEATURE).await {
        Ok(body) => Some(body).filter(|body| !body.trim().is_empty()),
        Err(err) => {
            tracing::warn!(scan_id = scan.id, error = %err, "promise message not written");
            None
        }
    }
}

async fn finish(pool: &MySqlPool, id: u64, status: &str) -> sqlx::Result<()> {
    sqlx::query("UPDATE ai_promise_scans SET status = ?, claimed_at = NULL WHERE id = ?")
        .bind(status)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Gives the claim back without counting the attempt (a service is not set
/// up here).
async fn give_back(pool: &MySqlPool, id: u64) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE ai_promise_scans
            SET claimed_at = NULL, attempts = GREATEST(attempts, 1) - 1
          WHERE id = ? AND status = 'pending'",
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M").unwrap()
    }

    fn open(id: u64, kind: &str, title: &str) -> OpenItem {
        OpenItem {
            id,
            kind: kind.into(),
            title: title.into(),
            details: None,
            same_customer: true,
            who: String::new(),
        }
    }

    /// An open quote task with these asks.
    fn quote_task(id: u64, details: &str) -> OpenItem {
        OpenItem {
            details: Some(details.into()),
            ..open(id, "quote", "Update the quote")
        }
    }

    #[test]
    fn the_model_json_is_read() {
        let shown = [
            open(7, "promise", "Send the price"),
            quote_task(9, "Swap the sink"),
        ];
        let reading = parse_reading(
            r##"{"promises": [
                {"title": " Send the inventory link ", "said": "I'll text you our inventory link", "due": "2026-10-28 17:00", "visit": false, "message": "sms", "link": "inventory"},
                {"title": "Measure the kitchen", "said": "I'll come measure Friday at 10", "due": "2026-10-30 10:00", "visit": true, "action": "schedule", "message": null, "link": null},
                {"title": "", "said": "nothing"}
            ],
            "kept": [7, "#7", 9, 12],
            "quote": {"title": " Update the kitchen quote ", "asks": ["Replace the sink with a 33-inch undermount", "replace the sink with a 33-inch UNDERMOUNT", " ", "Add a\nbacksplash"], "said": "Can we do a bigger sink?", "due": "2026-10-29 19:00"},
            "approvals": [{"title": "Approve the revised quote", "said": "Here is the revised quote"}, {"title": ""}]}"##,
            &shown,
        );
        assert_eq!(
            reading.promises,
            vec![
                Found {
                    title: "Send the inventory link".into(),
                    said: "I'll text you our inventory link".into(),
                    due: Some(at("2026-10-28 17:00")),
                    visit: false,
                    channel: Some(Channel::Sms),
                    link: Some(Link::Inventory),
                    action: None,
                },
                Found {
                    title: "Measure the kitchen".into(),
                    said: "I'll come measure Friday at 10".into(),
                    due: Some(at("2026-10-30 10:00")),
                    visit: true,
                    channel: None,
                    link: None,
                    action: Some(CrmAction::Schedule),
                },
            ]
        );
        // Only open items it was shown count (a quote task too).
        assert_eq!(reading.kept, vec![7, 9]);
        // One task; each ask once, on one line.
        assert_eq!(
            reading.quote,
            Some(QuoteTask {
                title: "Update the kitchen quote".into(),
                asks: vec![
                    "Replace the sink with a 33-inch undermount".into(),
                    "Add a backsplash".into(),
                ],
                said: "Can we do a bigger sink?".into(),
                due: Some(at("2026-10-29 19:00")),
            })
        );
        assert_eq!(
            reading.approvals,
            vec![Approval {
                title: "Approve the revised quote".into(),
                said: "Here is the revised quote".into(),
            }]
        );
        // No quote work; asks without a title are the quote to update.
        assert_eq!(parse_reading(r#"{"quote": null}"#, &shown).quote, None);
        assert_eq!(
            parse_reading(r#"{"quote": {"title": " ", "asks": []}}"#, &shown).quote,
            None
        );
        assert_eq!(
            parse_reading(r#"{"quote": {"asks": ["Use Alto quartz"]}}"#, &shown)
                .quote
                .map(|quote| quote.title),
            Some("Update the quote".into())
        );
        // An open approval the customer answered is kept too.
        assert_eq!(
            parse_reading(
                r#"{"kept": [4]}"#,
                &[open(4, "approval", "Sign the contract")]
            )
            .kept,
            vec![4]
        );
        assert_eq!(parse_reading("not json", &shown), Reading::default());
        assert!(
            parse_reading(r#"{"promises": "none"}"#, &shown)
                .promises
                .is_empty()
        );
    }

    #[test]
    fn the_prompt_lists_open_items_with_their_numbers() {
        let prompt = extraction_prompt(
            Source::Call,
            "Dema",
            "now",
            "now",
            &[
                open(7, "promise", "Send the price"),
                quote_task(9, "Swap the sink\nAdd a backsplash"),
            ],
            "Speaker A: hi",
        );
        assert!(
            prompt.contains("OPEN PROMISES (already on the rep's list):\n- #7 Send the price\n")
        );
        assert!(prompt.contains(
            "OPEN QUOTE TASK (already on the list, with the asks it has):\n- #9 Update the quote (has: Swap the sink; Add a backsplash)\n"
        ));
        assert!(
            prompt.contains("OPEN APPROVALS (sent, waiting for the customer's answer):\n(none)\n")
        );
    }

    #[test]
    fn the_morning_list_sorts_what_is_due() {
        let tz = chrono_tz::America::Indiana::Indianapolis;
        let promise = |kind: &str, title: &str, due: Option<&str>| ListedPromise {
            kind: kind.into(),
            title: title.into(),
            customer_name: Some("Ann Lee".into()),
            customer_digits: None,
            due_at: due.map(at),
        };
        // 8:30 AM in Indianapolis.
        let now = at("2026-10-28 12:30");
        let (summary, text) = morning_list(
            &[
                promise("promise", "Send the price", Some("2026-10-27 21:00")),
                promise("promise", "Call back", Some("2026-10-28 18:00")),
                promise("promise", "Send photos", None),
                promise("promise", "Visit next week", Some("2026-11-03 15:00")),
                promise("quote", "Update the quote", None),
                promise("approval", "Sign the contract", Some("2026-10-28 20:00")),
            ],
            &[Waiting {
                phone_digits: "3175550100".into(),
                customer_id: None,
                customer_name: None,
                text: "Can you send the price?".into(),
                at: at("2026-10-28 09:30"),
            }],
            now,
            tz,
        )
        .unwrap();
        assert_eq!(
            summary,
            "Morning list: 1 overdue, 1 due today, 1 without a time, 1 quotes to update, 1 waiting for approval, 1 waiting for your reply"
        );
        assert!(
            text.contains(
                "Waiting for the customer's approval (1):\n• Sign the contract — Ann Lee"
            )
        );
        assert!(text.starts_with("☀️ Good morning! Your list for Wed, Oct 28:"));
        assert!(text.contains("Overdue (1):\n• Send the price — Ann Lee (due Oct 27, 5:00 PM)"));
        assert!(text.contains("Due today (1):\n• 2:00 PM Call back — Ann Lee"));
        assert!(text.contains("No time set (1):\n• Send photos — Ann Lee"));
        assert!(text.contains("Quotes to make or update (1):\n• Update the quote — Ann Lee"));
        assert!(text.contains(
            "Waiting for your reply (1):\n• (317) 555-0100 — \"Can you send the price?\" (3 h)"
        ));
        assert!(!text.contains("Visit next week"));
        assert_eq!(morning_list(&[], &[], now, tz), None);
    }

    #[test]
    fn due_times_are_company_local() {
        let tz = chrono_tz::America::Indiana::Indianapolis;
        assert_eq!(
            due_utc(at("2026-10-28 17:00"), tz),
            Some(at("2026-10-28 21:00"))
        );
        assert_eq!(
            local_time(at("2026-10-28 19:06"), tz),
            "2026-10-28 15:06 (Wed)"
        );
    }

    #[test]
    fn what_is_open_or_listed_twice_is_dropped() {
        let promise = |title: &str| Found {
            title: title.into(),
            said: String::new(),
            due: None,
            visit: false,
            channel: None,
            link: None,
            action: None,
        };
        let mut reading = Reading {
            promises: vec![
                promise("Send the price"),
                promise("send the PRICE"),
                promise("Call back"),
            ],
            kept: Vec::new(),
            quote: Some(QuoteTask {
                title: "Update the quote".into(),
                asks: vec!["Swap the sink".into(), "Add a backsplash".into()],
                said: String::new(),
                due: None,
            }),
            approvals: Vec::new(),
            customer_name: None,
        };
        without_repeats(
            &mut reading,
            &[
                open(1, "promise", "Call back"),
                quote_task(2, "swap the SINK"),
            ],
        );
        assert_eq!(reading.promises, vec![promise("Send the price")]);
        // The open quote task has the sink already.
        assert_eq!(
            reading.quote.map(|quote| quote.asks),
            Some(vec!["Add a backsplash".to_string()])
        );
    }

    #[test]
    fn a_quote_task_gathers_each_ask_once() {
        assert_eq!(merged_details(None, &[]), None);
        assert_eq!(
            merged_details(None, &["Swap the sink".into()]),
            Some("Swap the sink".into())
        );
        assert_eq!(
            merged_details(
                Some("Swap the sink\n\nAdd a backsplash\n"),
                &["add a BACKSPLASH".into(), "Use Alto quartz".into()]
            ),
            Some("Swap the sink\nAdd a backsplash\nUse Alto quartz".into())
        );
        // The newest asks are kept.
        let many: Vec<String> = (1..=35).map(|n| format!("Ask {n}")).collect();
        let details = merged_details(None, &many).unwrap();
        assert_eq!(details.lines().count(), MAX_DETAIL_LINES);
        assert!(details.starts_with("Ask 6\n") && details.ends_with("Ask 35"));
    }

    #[test]
    fn closing_texts_wait_for_no_reply() {
        for text in [
            "Thank you!",
            "ok 👍",
            "Sounds good.",
            "👍",
            "Thanks so much!!",
        ] {
            assert!(is_closing(text), "{text}");
        }
        for text in [
            "Yes please that would be great",
            "Can you send the price?",
            "Ok, can we do Friday instead?",
        ] {
            assert!(!is_closing(text), "{text}");
        }
    }

    #[test]
    fn quoted_messages_are_cut_from_emails() {
        assert_eq!(
            without_quoted(
                "I'll send it tonight.\n\nOn Tue, Oct 27, Ann wrote:\n> Can you send it?"
            ),
            "I'll send it tonight."
        );
        assert_eq!(without_quoted("Thanks!\n"), "Thanks!");
        assert_eq!(
            split_quoted("  Sure.\nOn Mon, Bob wrote:\n> A bigger sink?"),
            ("Sure.", "On Mon, Bob wrote:\n> A bigger sink?")
        );
    }

    #[test]
    fn the_draft_request_carries_only_the_promised_link() {
        let links = Links {
            inventory: "https://x/stones".into(),
            quote: "https://x/quote".into(),
        };
        let mut found = Found {
            title: "Send the inventory link".into(),
            said: "I'll text you our inventory".into(),
            due: None,
            visit: false,
            channel: Some(Channel::Sms),
            link: Some(Link::Inventory),
            action: None,
        };
        let request = draft_request(&found, Source::Call, "2026-10-28 15:06 (Wed)", &links);
        assert!(request.contains("on our call"));
        assert!(request.contains("https://x/stones"));
        assert!(!request.contains("https://x/quote"));
        found.link = None;
        let request = draft_request(&found, Source::Sms, "now", &links);
        assert!(!request.contains("https://"));
    }

    #[test]
    fn a_long_transcript_keeps_its_start_and_end() {
        assert_eq!(start_and_end(" short ", 10), "short");
        let long = format!("{}{}", "a".repeat(50), "z".repeat(50));
        let kept = start_and_end(&long, 40);
        assert_eq!(kept, format!("{}\n…\n{}", "a".repeat(10), "z".repeat(30)));
    }
}
