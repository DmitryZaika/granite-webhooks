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
//!    scan) and has the AI list what the rep promised and by when, which of
//!    the customer's open promises it keeps (those are closed), and the
//!    changes the customer asked for in their quote (kept as kind `quote`,
//!    with their newest quote to open); the AI sees the open ones, so it
//!    never lists one twice;
//! 4. writes the message that keeps each promise a text or email can keep,
//!    as the AI Reply writes a custom draft (the rep's notes and voice, the
//!    whole conversation, the customer's quotes), and saves the promises
//!    (`ai_promises`) for the CRM's promise list and composers;
//! 5. reminds the rep of each promise once it is due ([`remind_due`]): a bell
//!    notification pushed to their phones, else sent to their Telegram;
//! 6. from 8 AM their time, once a day, sends the rep's morning list
//!    ([`send_digests`]): due today, overdue, without a time, quote changes,
//!    and the customers whose text waits for their reply.

use crate::ai::{AiError, Caller, Chat, MINI_FALLBACK_MODEL, MINI_MODEL};
use crate::auth::session::{SessionUser, find_live_user};
use crate::followups::gather::{customer_inventory_page_url, customer_quote_page_url};
use crate::phone::company;
use crate::routes::ai::conversation_reply::{self, ConversationReplyInput, ReplyMode};
use crate::routes::calls::recording::{CallTranscriptError, transcribe_call};
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
/// Calls and messages read per run, all at once.
pub const SCANS_PER_RUN: i64 = 4;
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
const TITLE_MAX_CHARS: usize = 160;
const SAID_MAX_CHARS: usize = 500;
/// The text tables a scan's `source_key` may name.
const SMS_TABLES: [&str; 3] = ["ringcentral_sms", "cloudtalk_sms", "telnyx_sms"];

/// SQL condition on `ai_promise_scans`: waiting, with attempts left, and no
/// run holds it (binds: the most attempts, the claim's minutes).
const DUE: &str = "status = 'pending' AND attempts < ?
     AND (claimed_at IS NULL OR claimed_at < UTC_TIMESTAMP() - INTERVAL ? MINUTE)";

pub const PROMISES_SYSTEM_PROMPT: &str = r#"You read what a countertop sales rep said to a customer: a phone call transcript, a text the rep sent (with the texts before it), or an email the rep sent (with the messages it quotes). Answer three things.
1. "promises": every promise the rep made to the customer: something the rep said THEY would do later ("I'll text you our inventory link", "I'll send you the price tonight", "I'll call you back Tuesday", "Let me check if that slab is in stock", "I'll come measure Friday at 10").
Only the rep's own promises to this customer. Not what the customer will do, not what is already done in the same call or message (a link the text itself contains is sent, not promised), not closings like "let me know if you have questions" or "talk soon", and nothing already in OPEN PROMISES. In texts, promises come only from the LAST text (the rep's); in an email, only from the rep's new text, not the quoted messages.
2. "kept": the numbers (#) of OPEN PROMISES this call or message keeps: the rep did it here (sent the link or the price, called back, answered, came by). Only ones clearly done.
3. "quote_changes": changes the CUSTOMER asked for in their countertop quote, anywhere in what you read: a different sink, color or stone, edge, backsplash, sizes, adding or removing a piece or a cutout. Not questions about the price, and nothing already in OPEN QUOTE CHANGES.
In a transcript the speakers are labeled A, B...; the rep is the one from the countertop company.
Return JSON {"promises": [...], "kept": [numbers], "quote_changes": [...]}, with empty lists when there is nothing. Each promise:
- "title": what the rep has to do, a short imperative of at most 8 words ("Send the inventory link", "Call back about the sink").
- "said": the rep's words that made the promise, as said (at most 200 characters).
- "due": when the rep promised it, "YYYY-MM-DD HH:MM" in the same local time as WHEN and NOW, or null when they named no time. "Tonight" is 19:00, "tomorrow" with no hour is 10:00, "this week" is Friday 17:00, "right after this call" or "in a minute" is 15 minutes after WHEN.
- "visit": true when it is an appointment at a set time: a visit, measuring or templating, a showroom meeting, an installation; else false.
- "message": "sms" or "email" when a text or an email that needs only words and a link keeps the promise (send the inventory link, send the online quote link, answer with facts the rep already has, confirm a time); "email" only when the rep said they would email. null when keeping it takes anything else: a call, a visit, measuring, ordering, checking stock, a price or a document the rep must prepare, photos.
- "link": "inventory" when the message is about our stones, colors or inventory; "quote" when it is about sending their measurements or the online quote form; else null.
Each quote change:
- "change": what to change in the quote, a short imperative of at most 12 words ("Replace the sink with a 33-inch undermount", "Switch the color to Calacatta Gold").
- "said": the customer's words that asked for it (at most 200 characters)."#;

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
}

/// A change the customer asked for in their quote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuoteChange {
    pub change: String,
    pub said: String,
}

/// What the AI found in a call or message.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reading {
    pub promises: Vec<Found>,
    /// Open promises (their ids) this call or message keeps.
    pub kept: Vec<u64>,
    pub quote_changes: Vec<QuoteChange>,
}

/// An open promise or quote change of the customer, as the AI is shown it.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct OpenItem {
    pub id: u64,
    /// `promise` or `quote`.
    pub kind: String,
    pub title: String,
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
        .filter(|id| {
            open.iter()
                .any(|item| item.id == *id && item.kind == "promise")
        })
        .collect();
    kept.sort_unstable();
    kept.dedup();
    let quote_changes = list("quote_changes")
        .iter()
        .filter_map(|item| {
            let change = clip(item.get("change")?.as_str()?, TITLE_MAX_CHARS);
            (!change.is_empty()).then(|| QuoteChange {
                change,
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
        quote_changes,
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
            .filter(|item| item.kind == kind)
            .map(|item| format!("- #{} {}", item.id, item.title))
            .collect();
        if lines.is_empty() {
            "(none)".to_string()
        } else {
            lines.join("\n")
        }
    };
    format!(
        "SALES REP: {rep_name}\nWHEN: {when}\nNOW: {now}\nOPEN PROMISES (already on the rep's list):\n{}\nOPEN QUOTE CHANGES (already on the list):\n{}\n\n{what}:\n{text}",
        listed("promise"),
        listed("quote"),
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
    /// Promises and quote changes saved.
    pub promises: usize,
    /// Due promises the rep was reminded of.
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
    let due: Vec<Scan> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT id, company_id, user_id, CAST(source AS CHAR) AS source, source_key, happened_at,
                customer_digits, customer_email, recording_link, transcript
           FROM ai_promise_scans
          WHERE {DUE}
            AND user_id IN (SELECT id FROM users WHERE ai_promises = 1 AND is_deleted = 0)
          ORDER BY happened_at, id LIMIT ?"
    )))
    .bind(MAX_ATTEMPTS)
    .bind(CLAIM_TTL_MINUTES)
    .bind(SCANS_PER_RUN)
    .fetch_all(pool)
    .await?;
    let tasks: Vec<_> = due
        .into_iter()
        .map(|scan| {
            let state = state.clone();
            tokio::spawn(async move { scan_one(&state, &scan).await })
        })
        .collect();
    for task in tasks {
        match task.await {
            Ok(Ok(Outcome::Read(saved))) => {
                run.read += 1;
                run.promises += saved;
            }
            Ok(Ok(Outcome::Later)) => {}
            Ok(Err(err)) => return Err(err),
            Err(err) => tracing::error!(error = %err, "promise scan task panicked"),
        }
    }
    Ok(run)
}

/// A due promise to remind the rep of.
#[derive(Debug, sqlx::FromRow)]
struct DuePromise {
    id: u64,
    user_id: i32,
    customer_id: Option<i32>,
    customer_name: Option<String>,
    title: String,
    said: String,
    /// The rep gets activity reminders on Telegram.
    telegram: bool,
}

/// Step 5: the open promises that came due, each once: a bell notification
/// (`ai_promise_due`) pushed to the rep's phones, else sent to their Telegram
/// when they get activity reminders there (as deal activity reminders are).
/// One more than a day past due when first seen (the job was off) is not.
pub async fn remind_due(state: &AppState) -> sqlx::Result<usize> {
    let pool = &state.pool;
    let due: Vec<DuePromise> = sqlx::query_as(
        "SELECT p.id, p.user_id, p.customer_id, c.name AS customer_name, p.title, p.said,
                COALESCE(u.telegram_activity_notifications, 0) <> 0 AS telegram
           FROM ai_promises p
           JOIN users u ON u.id = p.user_id AND u.ai_promises = 1 AND u.is_deleted = 0
           LEFT JOIN customers c ON c.id = p.customer_id
          WHERE p.status = 'open' AND p.kind = 'promise' AND p.reminded_at IS NULL
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
        let message = clip(&format!("Promise due: {}", promise.title), 255);
        let mut text = format!("⏰ {message}");
        if let Some(name) = promise
            .customer_name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
        {
            text.push_str(&format!("\n{name}"));
        }
        if !promise.said.is_empty() {
            text.push_str(&format!("\nYou said: \"{}\"", promise.said));
        }
        notify(
            state,
            promise.user_id,
            promise.customer_id,
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

/// A bell notification of `kind` pushed to the user's phones; when no phone
/// took it, `text` goes to their Telegram if `telegram`.
async fn notify(
    state: &AppState,
    user_id: i32,
    customer_id: Option<i32>,
    kind: &str,
    message: &str,
    text: &str,
    telegram: bool,
) -> sqlx::Result<()> {
    let id = sqlx::query(
        "INSERT INTO notifications (user_id, customer_id, message, notification_type, due_at, is_done, created_at)
         VALUES (?, ?, ?, ?, NOW(), 0, NOW())",
    )
    .bind(user_id)
    .bind(customer_id)
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

/// An open promise or quote change for the morning list.
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
            None,
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
        ("Quote changes customers asked for", &quotes),
        ("Waiting for your reply", &waiting_lines),
    ];
    if sections.iter().all(|(_, lines)| lines.is_empty()) {
        return None;
    }
    let counts: Vec<String> = [
        (overdue.len(), "overdue"),
        (due_today.len(), "due today"),
        (undated.len(), "without a time"),
        (quotes.len(), "quote changes"),
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
/// waits for the rep's reply: threads the rep has texted in the last 60 days,
/// oldest wait first, at most 10. A closing text ([`is_closing`]) waits for
/// nothing.
pub async fn waiting_customers(
    pool: &MySqlPool,
    company_id: i32,
    user_id: i32,
) -> sqlx::Result<Vec<Waiting>> {
    let mut waiting = Vec::new();
    for table in SMS_TABLES {
        let rows: Vec<(String, Option<String>, NaiveDateTime)> =
            sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "SELECT COALESCE(RIGHT(m.phone_digits, 10), ''), m.text, CAST(m.created_date AS DATETIME)
                   FROM {table} m
                   JOIN (SELECT phone_digits, MAX(id) AS last_id FROM {table}
                          WHERE company_id = ? AND created_date > UTC_TIMESTAMP() - INTERVAL 3 DAY
                          GROUP BY phone_digits) last ON last.last_id = m.id
                  WHERE m.direction = 'inbound'
                    AND m.created_date < UTC_TIMESTAMP() - INTERVAL 30 MINUTE
                    AND EXISTS (SELECT 1 FROM {table} r
                                 WHERE r.company_id = m.company_id AND r.phone_digits = m.phone_digits
                                   AND r.sender_user_id = ? AND r.direction = 'outbound'
                                   AND r.created_date > UTC_TIMESTAMP() - INTERVAL 60 DAY)
                  ORDER BY m.created_date LIMIT 10"
            )))
            .bind(company_id)
            .bind(user_id)
            .fetch_all(pool)
            .await?;
        for (digits, text, at) in rows {
            let text = text.unwrap_or_default();
            if digits.len() != 10
                || is_closing(&text)
                || waiting.iter().any(|w: &Waiting| w.phone_digits == digits)
            {
                continue;
            }
            let customer: Option<(i32, Option<String>)> = sqlx::query_as(
                "SELECT id, name FROM customers
                  WHERE company_id = ? AND deleted_at IS NULL
                    AND (RIGHT(REGEXP_REPLACE(COALESCE(phone, ''), '[^0-9]', ''), 10) = ?
                      OR RIGHT(REGEXP_REPLACE(COALESCE(phone_2, ''), '[^0-9]', ''), 10) = ?)
                  ORDER BY id DESC LIMIT 1",
            )
            .bind(company_id)
            .bind(&digits)
            .bind(&digits)
            .fetch_optional(pool)
            .await?;
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

/// Step 1: the reps' calls, texts and emails not queued yet.
async fn queue(pool: &MySqlPool) -> sqlx::Result<u64> {
    // INSERT IGNORE: a talk time that is not a number counts as 0 instead of
    // failing the statement.
    let mut queued = sqlx::query(
        "INSERT IGNORE INTO ai_promise_scans
                (company_id, user_id, source, source_key, happened_at, customer_digits, recording_link)
         SELECT pc.company_id, u.id, 'call', CONCAT(pc.provider, ':', pc.call_id), pc.started_at,
                pc.customer_digits, pc.item->>'$.Cdr.recording_link'
           FROM users u
           JOIN provider_calls pc ON pc.company_id = u.company_id
          WHERE u.ai_promises = 1 AND u.is_deleted = 0
            AND pc.started_at > UTC_TIMESTAMP() - INTERVAL ? DAY
            AND pc.customer_digits IS NOT NULL
            AND COALESCE(pc.item->>'$.Cdr.recording_link', '') NOT IN ('', 'null')
            AND CAST(COALESCE(pc.item->>'$.Cdr.talking_time', '0') AS UNSIGNED) >= ?
            AND ((pc.provider = 'ringcentral'
                  AND (pc.item->>'$.Cdr.user_id' = u.ringcentral_extension_id
                       OR RIGHT(pc.item->>'$.Cdr.public_internal', 10) = RIGHT(u.ringcentral_phone_number, 10)))
              OR (pc.provider = 'cloudtalk' AND pc.item->>'$.Agent.id' = u.cloudtalk_agent_id))
            AND NOT EXISTS (SELECT 1 FROM ai_promise_scans s
                             WHERE s.user_id = u.id AND s.source_key = CONCAT(pc.provider, ':', pc.call_id))",
    )
    .bind(LOOKBACK_DAYS)
    .bind(MIN_CALL_SECONDS)
    .execute(pool)
    .await?
    .rows_affected();
    queued += sqlx::query(
        "INSERT IGNORE INTO ai_promise_scans
                (company_id, user_id, source, source_key, happened_at, customer_digits, recording_link)
         SELECT c.company_id, u.id, 'call', CONCAT('telnyx:', c.id), c.started_at,
                RIGHT(REGEXP_REPLACE(c.other_number, '[^0-9]', ''), 10), CONCAT('tx_', c.id)
           FROM users u
           JOIN phone_calls c ON c.user_id = u.id AND c.company_id = u.company_id
          WHERE u.ai_promises = 1 AND u.is_deleted = 0
            AND c.started_at > UTC_TIMESTAMP() - INTERVAL ? DAY
            AND c.duration_secs >= ? AND c.recording_key IS NOT NULL
            AND NOT EXISTS (SELECT 1 FROM ai_promise_scans s
                             WHERE s.user_id = u.id AND s.source_key = CONCAT('telnyx:', c.id))",
    )
    .bind(LOOKBACK_DAYS)
    .bind(MIN_CALL_SECONDS)
    .execute(pool)
    .await?
    .rows_affected();
    for table in SMS_TABLES {
        queued += sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT IGNORE INTO ai_promise_scans
                    (company_id, user_id, source, source_key, happened_at, customer_digits)
             SELECT m.company_id, u.id, 'sms', CONCAT('{table}:', m.id), m.created_date,
                    RIGHT(m.phone_digits, 10)
               FROM users u
               JOIN {table} m ON m.sender_user_id = u.id AND m.company_id = u.company_id
              WHERE u.ai_promises = 1 AND u.is_deleted = 0
                AND m.direction = 'outbound' AND COALESCE(m.is_automated, 0) = 0
                AND COALESCE(m.is_echo, 0) = 0 AND m.status <> 'failed'
                AND m.created_date > UTC_TIMESTAMP() - INTERVAL ? DAY
                AND NOT EXISTS (SELECT 1 FROM ai_promise_scans s
                                 WHERE s.user_id = u.id AND s.source_key = CONCAT('{table}:', m.id))"
        )))
        .bind(LOOKBACK_DAYS)
        .execute(pool)
        .await?
        .rows_affected();
    }
    queued += sqlx::query(
        "INSERT IGNORE INTO ai_promise_scans
                (company_id, user_id, source, source_key, happened_at, customer_email)
         SELECT e.company_id, u.id, 'email', CONCAT('email:', e.id), e.sent_at,
                LOWER(TRIM(e.receiver_email))
           FROM users u
           JOIN emails e ON e.sender_user_id = u.id AND e.company_id = u.company_id
          WHERE u.ai_promises = 1 AND u.is_deleted = 0
            AND COALESCE(e.is_draft, 0) = 0 AND COALESCE(e.is_automated, 0) = 0
            AND e.deleted_at IS NULL AND e.sent_at > UTC_TIMESTAMP() - INTERVAL ? DAY
            AND NOT EXISTS (SELECT 1 FROM ai_promise_scans s
                             WHERE s.user_id = u.id AND s.source_key = CONCAT('email:', e.id))",
    )
    .bind(LOOKBACK_DAYS)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(queued)
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
        "SELECT id, CAST(kind AS CHAR) AS kind, title FROM ai_promises
          WHERE user_id = ? AND status = 'open'
            AND (customer_id = ? OR customer_digits = ? OR customer_email = ?)
          ORDER BY id DESC LIMIT 20",
    )
    .bind(user.id)
    .bind(customer_id)
    .bind(scan.customer_digits.as_deref())
    .bind(scan.customer_email.as_deref())
    .fetch_all(pool)
    .await?;
    let prompt = extraction_prompt(
        source,
        user.name.as_deref().unwrap_or("the rep"),
        &when,
        &local_time(Utc::now().naive_utc(), tz),
        &open,
        &start_and_end(&text, MAX_SOURCE_CHARS),
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
    let reading = parse_reading(&raw, &open);
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
    let quote_id = match customer_id {
        Some(customer_id) if !reading.quote_changes.is_empty() => {
            newest_quote(pool, scan.company_id, customer_id).await?
        }
        _ => None,
    };
    let mut tx = pool.begin().await?;
    for (promise, draft) in found.iter().zip(&drafts) {
        sqlx::query(
            "INSERT INTO ai_promises
                    (company_id, user_id, scan_id, source, promised_at, customer_id,
                     customer_digits, customer_email, title, said, due_at, is_appointment,
                     channel, draft)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(scan.company_id)
        .bind(user.id)
        .bind(scan.id)
        .bind(source.as_str())
        .bind(scan.happened_at)
        .bind(customer_id)
        .bind(scan.customer_digits.as_deref())
        .bind(scan.customer_email.as_deref())
        .bind(&promise.title)
        .bind(&promise.said)
        .bind(promise.due.and_then(|due| due_utc(due, tz)))
        .bind(promise.visit)
        .bind(promise.channel.map(Channel::as_str))
        .bind(draft.as_deref())
        .execute(&mut *tx)
        .await?;
    }
    for change in &reading.quote_changes {
        sqlx::query(
            "INSERT INTO ai_promises
                    (company_id, user_id, scan_id, source, kind, promised_at, customer_id,
                     customer_digits, customer_email, quote_id, title, said)
             VALUES (?, ?, ?, ?, 'quote', ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(scan.company_id)
        .bind(user.id)
        .bind(scan.id)
        .bind(source.as_str())
        .bind(scan.happened_at)
        .bind(customer_id)
        .bind(scan.customer_digits.as_deref())
        .bind(scan.customer_email.as_deref())
        .bind(quote_id)
        .bind(&change.change)
        .bind(&change.said)
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
    Ok(Outcome::Read(found.len() + reading.quote_changes.len()))
}

/// The customer's newest quote, the one a change they asked for goes in.
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

/// The customer with the scan's phone or email.
async fn customer_of(pool: &MySqlPool, scan: &Scan) -> sqlx::Result<Option<i32>> {
    if let Some(digits) = scan.customer_digits.as_deref().filter(|d| d.len() == 10) {
        return sqlx::query_scalar(
            "SELECT id FROM customers
              WHERE company_id = ? AND deleted_at IS NULL
                AND (RIGHT(REGEXP_REPLACE(COALESCE(phone, ''), '[^0-9]', ''), 10) = ?
                  OR RIGHT(REGEXP_REPLACE(COALESCE(phone_2, ''), '[^0-9]', ''), 10) = ?)
              ORDER BY id DESC LIMIT 1",
        )
        .bind(scan.company_id)
        .bind(digits)
        .bind(digits)
        .fetch_optional(pool)
        .await;
    }
    if let Some(email) = scan.customer_email.as_deref().filter(|e| e.contains('@')) {
        return sqlx::query_scalar(
            "SELECT c.id FROM customers c
               JOIN customers_emails ce ON ce.customer_id = c.id
              WHERE c.company_id = ? AND c.deleted_at IS NULL AND LOWER(TRIM(ce.email)) = ?
              ORDER BY c.id DESC LIMIT 1",
        )
        .bind(scan.company_id)
        .bind(email)
        .fetch_optional(pool)
        .await;
    }
    Ok(None)
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
        }
    }

    #[test]
    fn the_model_json_is_read() {
        let shown = [
            open(7, "promise", "Send the price"),
            open(9, "quote", "Swap the sink"),
        ];
        let reading = parse_reading(
            r##"{"promises": [
                {"title": " Send the inventory link ", "said": "I'll text you our inventory link", "due": "2026-10-28 17:00", "visit": false, "message": "sms", "link": "inventory"},
                {"title": "Measure the kitchen", "said": "I'll come measure Friday at 10", "due": "2026-10-30 10:00", "visit": true, "message": null, "link": null},
                {"title": "", "said": "nothing"}
            ],
            "kept": [7, "#7", 9, 12],
            "quote_changes": [{"change": "Replace the sink with a 33-inch undermount", "said": "Can we do a bigger sink?"}, {"change": " "}]}"##,
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
                },
                Found {
                    title: "Measure the kitchen".into(),
                    said: "I'll come measure Friday at 10".into(),
                    due: Some(at("2026-10-30 10:00")),
                    visit: true,
                    channel: None,
                    link: None,
                },
            ]
        );
        // Only open promises it was shown count; quote changes are not kept.
        assert_eq!(reading.kept, vec![7]);
        assert_eq!(
            reading.quote_changes,
            vec![QuoteChange {
                change: "Replace the sink with a 33-inch undermount".into(),
                said: "Can we do a bigger sink?".into(),
            }]
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
                open(9, "quote", "Swap the sink"),
            ],
            "Speaker A: hi",
        );
        assert!(
            prompt.contains("OPEN PROMISES (already on the rep's list):\n- #7 Send the price\n")
        );
        assert!(prompt.contains("OPEN QUOTE CHANGES (already on the list):\n- #9 Swap the sink\n"));
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
                promise("quote", "Swap the sink", None),
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
            "Morning list: 1 overdue, 1 due today, 1 without a time, 1 quote changes, 1 waiting for your reply"
        );
        assert!(text.starts_with("☀️ Good morning! Your list for Wed, Oct 28:"));
        assert!(text.contains("Overdue (1):\n• Send the price — Ann Lee (due Oct 27, 5:00 PM)"));
        assert!(text.contains("Due today (1):\n• 2:00 PM Call back — Ann Lee"));
        assert!(text.contains("No time set (1):\n• Send photos — Ann Lee"));
        assert!(text.contains("Quote changes customers asked for (1):\n• Swap the sink — Ann Lee"));
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
