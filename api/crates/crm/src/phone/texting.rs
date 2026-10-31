//! Texting through Telnyx (the `telnyx` SMS provider): which lines a user
//! texts from and reads, sending texts and MMS, inbound texts with their
//! files and side effects, delivery status, and opt-outs (STOP).
//!
//! Rows live in `telnyx_sms` (+ `_attachments`, `_thread_reads`) with the
//! derived thread columns the other providers have (`sender10`,
//! `recipient10`, `phone_digits` = the customer), so the SMS routes
//! (`routes/sms/`) read them like CloudTalk's and RingCentral's. A user's
//! Telnyx inbox is every thread of the numbers they may call from
//! (`telnyx_number_users.can_call_from`): a shared line is a shared inbox.
//! They text from their line ([`user_line`]); `agent` on what they send is
//! their user id. Texting is on when the company's Telnyx setup is enabled
//! and names its messaging profile ([`CompanyConfig::texting_on`]).
//!
//! Sends go straight to Telnyx (`POST /messages`): pictures and PDFs up to
//! Telnyx's 1 MB per message as MMS (presigned links valid 3 hours, split
//! into several texts when needed), anything bigger or of another type as
//! links. US and Canada numbers only. Nothing here logs phone numbers,
//! message text or links.

use super::company::{self, CompanyConfig};
use super::lines::{self, CallerId};
use super::numbers::{is_nanp, last10, to_e164};
use crate::error::ApiError;
use crate::routes::sms::provider::SmsProvider;
use crate::state::AppState;
use crate::telnyx::TelnyxError;
use crate::telnyx::messaging::{MessagePayload, OutgoingMessage};
use crate::webhooks_client::{OutgoingSms, OutgoingSmsFile, SmsFailure};
use chrono::{DateTime, SubsecRound, Utc};
use lambda_http::tracing;
use sqlx::MySqlPool;
use std::collections::HashMap;
use std::time::Duration;

/// Telnyx refuses a message over 1 MB in all.
pub const MMS_MAX_BYTES: u64 = 1_000_000;
/// Files in one MMS.
pub const MMS_MAX_FILES: usize = 10;
/// Types sent as MMS media; any other file goes as a link.
const MMS_TYPES: [&str; 4] = ["image/jpeg", "image/png", "image/gif", "application/pdf"];
/// Pictures go to Telnyx through the Cloudflare picture worker (general_datebase
/// `infra/cloudflare`): JPEG of at most 1280 px and about 110 KB (`?for=mms`),
/// so about nine fit one MMS. `PICTURE_WORKER_URL` overrides it; set empty,
/// the stored files go as they are.
const PICTURE_WORKER_URL: &str = "https://granite-media.granite-manager.workers.dev";
/// Pictures the worker makes ready for a text (a GIF goes as stored, animated).
const WORKER_PICTURE_TYPES: [&str; 2] = ["image/jpeg", "image/png"];
/// How long a picture may take to come back from the worker.
const WORKER_TIMEOUT: Duration = Duration::from_secs(20);
/// How long the links Telnyx fetches an MMS's files from stay valid.
const MEDIA_LINK_TTL: Duration = Duration::from_secs(3 * 60 * 60);
/// The largest file of an inbound MMS that is kept.
const INBOUND_MEDIA_MAX_BYTES: usize = 20 * 1024 * 1024;
/// An outbound text still `pending` after this long is failed.
const PENDING_TIMEOUT_MINUTES: i64 = 5;
/// Telnyx's code for a text to someone who texted STOP.
pub const OPTED_OUT_CODE: &str = "40300";
/// The list a won deal sits on (`CLOSED_WON_LIST_ID` of the webhooks crate).
const CLOSED_WON_LIST_ID: i32 = 4;

/// What an inbound text asks for, by Telnyx's default keywords and the
/// FCC's opt-out words (47 CFR 64.1200(a)(10), since April 2025).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptKeyword {
    /// STOP, STOPALL, STOP ALL, UNSUBSCRIBE, CANCEL, END, QUIT, REVOKE,
    /// OPT OUT (OPTOUT, OPT-OUT).
    OptOut,
    /// START, UNSTOP.
    OptIn,
}

/// A text's time as `telnyx_sms.created_date` keeps it: whole seconds, cut
/// down. MySQL would round a fraction up into the next second, a time
/// after a read marker set in the same second (`UTC_TIMESTAMP()`, cut
/// down), and the text would stay unread.
fn stored_time(at: DateTime<Utc>) -> DateTime<Utc> {
    at.trunc_subsecs(0)
}

/// The opt-out keyword a whole text is, if it is one (case, spacing and
/// trailing punctuation ignored).
pub fn opt_keyword(text: &str) -> Option<OptKeyword> {
    let words: Vec<String> = text
        .split_whitespace()
        .map(|word| {
            word.trim_matches(|c: char| !c.is_alphanumeric())
                .to_uppercase()
        })
        .filter(|word| !word.is_empty())
        .collect();
    match words.join(" ").as_str() {
        "STOP" | "STOPALL" | "STOP ALL" | "UNSUBSCRIBE" | "CANCEL" | "END" | "QUIT" | "REVOKE"
        | "OPT OUT" | "OPTOUT" | "OPT-OUT" => Some(OptKeyword::OptOut),
        "START" | "UNSTOP" => Some(OptKeyword::OptIn),
        _ => None,
    }
}

/// The company's Telnyx setup when it texts through Telnyx.
pub async fn texting_setup(
    pool: &MySqlPool,
    company_id: i32,
) -> sqlx::Result<Option<CompanyConfig>> {
    Ok(company::load(pool, company_id)
        .await?
        .filter(CompanyConfig::texting_on))
}

/// The line `user_id` texts from (`lines::user_line`), when the company
/// texts through Telnyx.
pub async fn user_line(
    pool: &MySqlPool,
    company_id: i32,
    user_id: i32,
) -> sqlx::Result<Option<CallerId>> {
    if texting_setup(pool, company_id).await?.is_none() {
        return Ok(None);
    }
    lines::user_line(pool, company_id, user_id).await
}

/// The last ten digits of every active number `user_id` may call from: the
/// threads of these lines are the user's Telnyx inbox.
pub async fn inbox_lines(
    pool: &MySqlPool,
    company_id: i32,
    user_id: i32,
) -> sqlx::Result<Vec<String>> {
    Ok(lines::caller_ids(pool, company_id, user_id)
        .await?
        .iter()
        .filter_map(|line| last10(&line.e164))
        .collect())
}

/// SQL over `telnyx_sms s`: a text of `rep`'s, on any line: tagged with
/// their user id (theirs, or copied for them when they moved to Telnyx,
/// `crate::routes::sms::moves`) or sent by them. Binds the rep's user id as
/// text, then as a number.
const REP_ROWS: &str = "(s.agent = ? OR s.sender_user_id = ?)";

/// The customer phones (thread keys) that texted or were texted from
/// `lines`, and those of `rep`'s own texts ([`REP_ROWS`]).
pub async fn line_threads(
    pool: &MySqlPool,
    company_id: i32,
    lines: &[String],
    rep: Option<i32>,
) -> sqlx::Result<Vec<String>> {
    let mut parts = Vec::new();
    if !lines.is_empty() {
        let marks = vec!["?"; lines.len()].join(", ");
        // idx_sms_inbound_recipient and idx_sms_outbound_sender: index only.
        parts.push(format!(
            "SELECT s.phone_digits FROM telnyx_sms s
              WHERE s.company_id = ? AND s.direction = 'inbound' AND s.recipient10 IN ({marks})
                AND s.phone_digits IS NOT NULL
             UNION
             SELECT s.phone_digits FROM telnyx_sms s
              WHERE s.company_id = ? AND s.direction = 'outbound' AND s.sender10 IN ({marks})
                AND s.phone_digits IS NOT NULL"
        ));
    }
    if rep.is_some() {
        // idx_sms_agent_threads and idx_telnyx_sms_sender_user.
        parts.push(
            "SELECT s.phone_digits FROM telnyx_sms s
              WHERE s.company_id = ? AND s.agent = ? AND s.phone_digits IS NOT NULL
             UNION
             SELECT s.phone_digits FROM telnyx_sms s
              WHERE s.sender_user_id = ? AND s.company_id = ? AND s.phone_digits IS NOT NULL"
                .to_string(),
        );
    }
    if parts.is_empty() {
        return Ok(Vec::new());
    }
    let mut query = sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(parts.join(" UNION ")));
    if !lines.is_empty() {
        for _ in 0..2 {
            query = query.bind(company_id);
            for line in lines {
                query = query.bind(line);
            }
        }
    }
    if let Some(rep) = rep {
        query = query
            .bind(company_id)
            .bind(rep.to_string())
            .bind(rep)
            .bind(company_id);
    }
    query.fetch_all(pool).await
}

/// The thread `phone` has a row on one of `lines`, or one of `rep`'s own
/// texts ([`REP_ROWS`]).
pub async fn thread_on_lines(
    pool: &MySqlPool,
    company_id: i32,
    lines: &[String],
    rep: Option<i32>,
    phone: &str,
) -> sqlx::Result<bool> {
    let mut parts = Vec::new();
    if !lines.is_empty() {
        let marks = vec!["?"; lines.len()].join(", ");
        parts.push(format!(
            "(s.direction = 'inbound' AND s.recipient10 IN ({marks}))
              OR (s.direction = 'outbound' AND s.sender10 IN ({marks}))"
        ));
    }
    if rep.is_some() {
        parts.push(REP_ROWS.to_string());
    }
    if parts.is_empty() {
        return Ok(false);
    }
    let sql = format!(
        "SELECT 1 FROM telnyx_sms s
          WHERE s.company_id = ? AND s.phone_digits = ? AND ({})
          LIMIT 1",
        parts.join(" OR ")
    );
    let mut query = sqlx::query_scalar::<_, i32>(sqlx::AssertSqlSafe(sql))
        .bind(company_id)
        .bind(phone);
    for _ in 0..2 {
        for line in lines {
            query = query.bind(line);
        }
    }
    if let Some(rep) = rep {
        query = query.bind(rep.to_string()).bind(rep);
    }
    Ok(query.fetch_optional(pool).await?.is_some())
}

/// The company's numbers (last ten digits): never thread keys.
pub async fn company_lines(pool: &MySqlPool, company_id: i32) -> sqlx::Result<Vec<String>> {
    sqlx::query_scalar("SELECT last10 FROM telnyx_numbers WHERE company_id = ?")
        .bind(company_id)
        .fetch_all(pool)
        .await
}

/// The company's users who may text through Telnyx: (user id, name), by
/// name. Their id is their `agent` on Telnyx texts.
pub async fn texting_users(pool: &MySqlPool, company_id: i32) -> sqlx::Result<Vec<(i32, String)>> {
    sqlx::query_as(
        "SELECT u.id, COALESCE(u.name, '') AS name FROM users u
          WHERE u.company_id = ? AND u.is_deleted = 0
            AND EXISTS (SELECT 1 FROM telnyx_number_users nu
                          JOIN telnyx_numbers n ON n.id = nu.number_id AND n.company_id = u.company_id
                         WHERE nu.user_id = u.id AND nu.can_call_from <> 0 AND n.active <> 0)
          ORDER BY name, u.id",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await
}

/// Names of the company's users by id (as Telnyx texts carry it in `agent`).
pub async fn user_names(
    pool: &MySqlPool,
    company_id: i32,
) -> sqlx::Result<std::collections::HashMap<String, String>> {
    let rows: Vec<(i32, Option<String>)> =
        sqlx::query_as("SELECT id, name FROM users WHERE company_id = ? AND is_deleted = 0")
            .bind(company_id)
            .fetch_all(pool)
            .await?;
    Ok(rows
        .into_iter()
        .map(|(id, name)| (id.to_string(), name.unwrap_or_default()))
        .collect())
}

/// Who an inbound text to a direct line is for: line (last ten digits) →
/// (user id, name) of the one user who may call from it.
pub async fn direct_line_users(
    pool: &MySqlPool,
    company_id: i32,
) -> sqlx::Result<std::collections::HashMap<String, (String, String)>> {
    let rows: Vec<(String, i32, Option<String>)> = sqlx::query_as(
        "SELECT n.last10, MIN(u.id), MIN(u.name) FROM telnyx_numbers n
           JOIN telnyx_number_users nu ON nu.number_id = n.id AND nu.can_call_from <> 0
           JOIN users u ON u.id = nu.user_id AND u.company_id = n.company_id AND u.is_deleted = 0
          WHERE n.company_id = ? AND n.kind = 'direct'
          GROUP BY n.last10
         HAVING COUNT(DISTINCT u.id) = 1",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(line, id, name)| (line, (id.to_string(), name.unwrap_or_default())))
        .collect())
}

/// The phone texted STOP to the company (or Telnyx refused a text to it as
/// opted out).
pub async fn is_opted_out(pool: &MySqlPool, company_id: i32, phone: &str) -> sqlx::Result<bool> {
    let Some(phone10) = last10(phone) else {
        return Ok(false);
    };
    let found: Option<i32> =
        sqlx::query_scalar("SELECT 1 FROM sms_opt_outs WHERE company_id = ? AND phone10 = ?")
            .bind(company_id)
            .bind(phone10)
            .fetch_optional(pool)
            .await?;
    Ok(found.is_some())
}

/// Marks the phone opted out (`source`: `stop` or `carrier`).
pub async fn opt_out(
    pool: &MySqlPool,
    company_id: i32,
    phone10: &str,
    source: &str,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO sms_opt_outs (company_id, phone10, source, opted_out_at)
         VALUES (?, ?, ?, UTC_TIMESTAMP())
         ON DUPLICATE KEY UPDATE source = VALUES(source), opted_out_at = VALUES(opted_out_at)",
    )
    .bind(company_id)
    .bind(phone10)
    .bind(source)
    .execute(pool)
    .await?;
    Ok(())
}

/// The phone texted START: texts may go to it again.
pub async fn opt_in(pool: &MySqlPool, company_id: i32, phone10: &str) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM sms_opt_outs WHERE company_id = ? AND phone10 = ?")
        .bind(company_id)
        .bind(phone10)
        .execute(pool)
        .await?;
    Ok(())
}

/// Fails the company's texts that never finished sending (the Lambda
/// stopped mid-send).
pub async fn fail_stale_pending(pool: &MySqlPool, company_id: i32) -> sqlx::Result<u64> {
    Ok(sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE telnyx_sms SET status = 'failed', error_message = 'pending_timeout'
          WHERE company_id = ? AND direction = 'outbound' AND status = 'pending'
            AND created_date < UTC_TIMESTAMP() - INTERVAL {PENDING_TIMEOUT_MINUTES} MINUTE"
    )))
    .bind(company_id)
    .execute(pool)
    .await?
    .rows_affected())
}

/// The token the CRM shows for a refused or failed Telnyx send.
pub fn error_token(err: &TelnyxError) -> String {
    match err {
        TelnyxError::NotConfigured => "telnyx_not_configured".into(),
        TelnyxError::Status {
            code: Some(code), ..
        } if code == OPTED_OUT_CODE => "opted_out".into(),
        TelnyxError::Status {
            code: Some(code), ..
        } if !code.is_empty() && code.chars().all(|c| c.is_ascii_digit()) => {
            format!("telnyx_{code}")
        }
        TelnyxError::Status { status: 429, .. } => "rate_limited".into(),
        _ => "send_failed".into(),
    }
}

fn failure(status: u16, error: &str) -> SmsFailure {
    SmsFailure {
        status,
        error: error.to_string(),
        sent_ids: Vec::new(),
        unsent_s3_keys: Vec::new(),
    }
}

/// "Photo 1: <url>", "File 1: name\n<url>" under the caption (the other
/// providers' link fallback).
pub fn link_body(caption: &str, files: &[&OutgoingSmsFile]) -> String {
    let (mut photos, mut others) = (0, 0);
    let mut lines = Vec::new();
    for file in files {
        if file.content_type.starts_with("image/") {
            photos += 1;
            lines.push(format!("Photo {photos}: {}", file.url));
        } else {
            others += 1;
            let name = file.filename.trim();
            lines.push(format!(
                "File {others}: {}",
                if name.is_empty() { "file" } else { name }
            ));
            lines.push(file.url.clone());
        }
    }
    let joined = lines.join("\n");
    if caption.is_empty() {
        joined
    } else {
        format!("{caption}\n{joined}")
    }
}

/// One message of a send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned<'a> {
    /// What the row shows (the caption; files show as attachments).
    pub text: String,
    /// What Telnyx sends as text: the caption, or the links of `links`.
    pub body: String,
    /// Files sent as MMS media.
    pub media: Vec<&'a OutgoingSmsFile>,
    /// Files sent as links.
    pub links: Vec<&'a OutgoingSmsFile>,
}

/// Splits a send into Telnyx messages: MMS-able files (`MMS_TYPES`, known
/// size, at most [`MMS_MAX_BYTES`]) in order into messages of at most
/// [`MMS_MAX_FILES`] files and 1 MB, the caption with the first; the rest in
/// one more text as links (with the caption when nothing went as MMS).
/// `sizes[i]` is the size of `files[i]`, `None` when unknown.
pub fn plan<'a>(
    text: &str,
    files: &'a [OutgoingSmsFile],
    sizes: &[Option<u64>],
) -> Vec<Planned<'a>> {
    if files.is_empty() {
        return vec![Planned {
            text: text.to_string(),
            body: text.to_string(),
            media: Vec::new(),
            links: Vec::new(),
        }];
    }
    let mut batches: Vec<(Vec<&OutgoingSmsFile>, u64)> = Vec::new();
    let mut links = Vec::new();
    for (index, file) in files.iter().enumerate() {
        let size = sizes.get(index).copied().flatten();
        match size.filter(|size| {
            *size <= MMS_MAX_BYTES && MMS_TYPES.contains(&file.content_type.as_str())
        }) {
            Some(size) => {
                let fits = batches.last().is_some_and(|(batch, total)| {
                    batch.len() < MMS_MAX_FILES && total + size <= MMS_MAX_BYTES
                });
                if fits {
                    if let Some((batch, total)) = batches.last_mut() {
                        batch.push(file);
                        *total += size;
                    }
                } else {
                    batches.push((vec![file], size));
                }
            }
            None => links.push(file),
        }
    }
    let mut planned: Vec<Planned<'a>> = batches
        .into_iter()
        .enumerate()
        .map(|(index, (media, _))| {
            let caption = if index == 0 { text } else { "" };
            Planned {
                text: caption.to_string(),
                body: caption.to_string(),
                media,
                links: Vec::new(),
            }
        })
        .collect();
    if !links.is_empty() {
        let caption = if planned.is_empty() { text } else { "" };
        planned.push(Planned {
            text: caption.to_string(),
            body: link_body(caption, &links),
            media: Vec::new(),
            links,
        });
    }
    planned
}

/// E.164 digits as stored in `sender` / `recipient`.
fn e164_digits(e164: &str) -> Option<i64> {
    e164.trim_start_matches('+').parse().ok()
}

/// A text already sent under this idempotency key (a retried flow step).
async fn already_sent(pool: &MySqlPool, company_id: i32, key: &str) -> sqlx::Result<Option<i64>> {
    sqlx::query_scalar(
        "SELECT CAST(id AS SIGNED) FROM telnyx_sms
          WHERE company_id = ? AND idempotency_key = ? AND direction = 'outbound'
            AND status IN ('sent', 'pending')
          ORDER BY id LIMIT 1",
    )
    .bind(company_id)
    .bind(key)
    .fetch_optional(pool)
    .await
}

/// Who sends, from which line to whom, through which profile.
struct Delivery<'a> {
    state: &'a AppState,
    sms: &'a OutgoingSms,
    config: &'a CompanyConfig,
    line: &'a CallerId,
    to: String,
    webhook_url: String,
    /// The worker's copy of each picture that has one, by S3 key.
    worker_links: HashMap<String, String>,
}

impl Delivery<'_> {
    /// Stores one message `pending` with its files; its row id.
    async fn store_pending(&self, message: &Planned<'_>, key: &str) -> sqlx::Result<i64> {
        let pool = &self.state.pool;
        let line10 = last10(&self.line.e164).unwrap_or_default();
        let customer10 = last10(&self.to).unwrap_or_default();
        let id = sqlx::query(
            "INSERT INTO telnyx_sms
               (telnyx_id, sender, recipient, text, direction, status, idempotency_key, agent,
                sender_user_id, company_id, created_date, sender10, recipient10, phone_digits,
                is_echo, is_automated, number_id)
             VALUES (NULL, ?, ?, ?, 'outbound', 'pending', ?, ?, ?, ?, ?, ?, ?, ?, 0, ?, ?)",
        )
        .bind(e164_digits(&self.line.e164))
        .bind(e164_digits(&self.to).unwrap_or_default())
        .bind(&message.text)
        .bind(key)
        .bind(self.sms.user_id.to_string())
        .bind(self.sms.user_id)
        .bind(self.sms.company_id)
        .bind(stored_time(Utc::now()))
        .bind(&line10)
        .bind(&customer10)
        .bind(&customer10)
        .bind(self.sms.is_automated)
        .bind(self.line.number_id)
        .execute(pool)
        .await?
        .last_insert_id();
        let id = i64::try_from(id).map_err(|err| sqlx::Error::Decode(Box::new(err)))?;
        for (position, file) in message.media.iter().chain(&message.links).enumerate() {
            sqlx::query(
                "INSERT INTO telnyx_sms_attachments
                   (telnyx_sms_id, content_type, filename, s3_key, s3_url, width, height, position)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(id)
            .bind(&file.content_type)
            .bind(&file.filename)
            .bind(&file.s3_key)
            .bind(&file.url)
            .bind(file.width)
            .bind(file.height)
            .bind(i32::try_from(position).unwrap_or(i32::MAX))
            .execute(pool)
            .await?;
        }
        Ok(id)
    }

    /// Stores, sends and records one message: its row id, and the error
    /// token when Telnyx refused it.
    async fn send(&self, message: &Planned<'_>, key: &str) -> sqlx::Result<(i64, Option<String>)> {
        let state = self.state;
        let id = self.store_pending(message, key).await?;
        let mut media_urls = Vec::with_capacity(message.media.len());
        for file in &message.media {
            if let Some(link) = self.worker_links.get(&file.s3_key) {
                media_urls.push(link.clone());
                continue;
            }
            match state
                .storage
                .presign_get_in_for(None, &file.s3_key, &file.content_type, None, MEDIA_LINK_TTL)
                .await
            {
                Ok(url) => media_urls.push(url),
                Err(err) => {
                    tracing::error!(error = %err, id, "Telnyx MMS: file link not made");
                    self.finish(id, Err(("send_failed".into(), None))).await?;
                    return Ok((id, Some("send_failed".into())));
                }
            }
        }
        let outgoing = OutgoingMessage {
            from: &self.line.e164,
            to: &self.to,
            text: &message.body,
            media_urls: &media_urls,
            messaging_profile_id: self
                .config
                .messaging_profile_id
                .as_deref()
                .unwrap_or_default(),
            webhook_url: &self.webhook_url,
        };
        match state.telnyx.send_message(&outgoing).await {
            Ok(sent) => {
                // Sent: a database error from here on must not fail the send.
                if let Err(err) = self.finish(id, Ok((sent.id, sent.status))).await {
                    tracing::error!(error = %err, id, "Telnyx text sent but not recorded");
                }
                Ok((id, None))
            }
            Err(err) => {
                let token = error_token(&err);
                tracing::warn!(id, status = ?err.status(), code = ?err.code(), "Telnyx refused a text");
                if err.code() == Some(OPTED_OUT_CODE) {
                    let phone10 = last10(&self.to).unwrap_or_default();
                    opt_out(&state.pool, self.sms.company_id, &phone10, "carrier").await?;
                }
                self.finish(id, Err((token.clone(), err.code().map(str::to_string))))
                    .await?;
                Ok((id, Some(token)))
            }
        }
    }

    /// Records how a send ended: Telnyx's id and status, or the error.
    async fn finish(
        &self,
        id: i64,
        outcome: Result<(String, Option<String>), (String, Option<String>)>,
    ) -> sqlx::Result<()> {
        let pool = &self.state.pool;
        match outcome {
            Ok((telnyx_id, status)) => {
                sqlx::query(
                    "UPDATE telnyx_sms SET status = 'sent', telnyx_id = ?,
                            delivery_status = COALESCE(delivery_status, ?)
                      WHERE id = ? AND status = 'pending'",
                )
                .bind(telnyx_id)
                .bind(status)
                .bind(id)
                .execute(pool)
                .await?;
            }
            Err((token, code)) => {
                sqlx::query(
                    "UPDATE telnyx_sms SET status = 'failed', error_message = ?, error_code = ?
                      WHERE id = ?",
                )
                .bind(token)
                .bind(code)
                .bind(id)
                .execute(pool)
                .await?;
            }
        }
        Ok(())
    }

    /// Removes a never-sent row and its files' rows.
    async fn discard(&self, id: i64) -> sqlx::Result<()> {
        sqlx::query("DELETE FROM telnyx_sms WHERE id = ? AND telnyx_id IS NULL")
            .bind(id)
            .execute(&self.state.pool)
            .await?;
        Ok(())
    }
}

/// The picture worker's address, or `None` when `PICTURE_WORKER_URL` is set empty.
fn picture_worker() -> Option<String> {
    let base = std::env::var("PICTURE_WORKER_URL").unwrap_or_else(|_| PICTURE_WORKER_URL.into());
    let base = base.trim().trim_end_matches('/');
    (!base.is_empty()).then(|| base.to_string())
}

/// The worker's URL of a stored picture made ready for a text, or `None` for
/// other files.
fn worker_picture_url(base: &str, file: &OutgoingSmsFile) -> Option<String> {
    WORKER_PICTURE_TYPES
        .contains(&file.content_type.as_str())
        .then(|| format!("{base}/{}?for=mms", file.s3_key))
}

/// The worker's copy of a picture: its URL and size. Fetched once here, so
/// its real size plans the messages and Telnyx then gets it from Cloudflare's
/// cache. `None` (the stored file goes instead) when the worker fails.
async fn worker_picture(
    http: &reqwest::Client,
    base: &str,
    file: &OutgoingSmsFile,
) -> Option<(String, u64)> {
    let url = worker_picture_url(base, file)?;
    let response = http.get(&url).timeout(WORKER_TIMEOUT).send().await.ok()?;
    if !response.status().is_success() {
        tracing::warn!(status = %response.status(), "picture worker refused an MMS picture");
        return None;
    }
    let bytes = response.bytes().await.ok()?;
    Some((url, u64::try_from(bytes.len()).unwrap_or(u64::MAX)))
}

/// Sends `sms` from `line` through the company's Telnyx messaging profile
/// (the "send as user" of every sender): the stored rows, in order, the
/// first carrying the text. Refused (`400`) when Telnyx texting is off
/// (`telnyx_not_configured`), the phone is not a US or Canadian number
/// (`invalid_phone`) or texted STOP (`opted_out`); `502` with a token when
/// Telnyx failed. A retried send with the same idempotency key that already
/// went out is not sent again.
pub async fn deliver(
    state: &AppState,
    line: &CallerId,
    sms: &OutgoingSms,
) -> Result<Vec<i64>, SmsFailure> {
    let pool = &state.pool;
    let Some(config) = texting_setup(pool, sms.company_id).await? else {
        return Err(failure(400, "telnyx_not_configured"));
    };
    let Some(to) = to_e164(&sms.phone_digits).filter(|to| is_nanp(to)) else {
        return Err(failure(400, "invalid_phone"));
    };
    if is_opted_out(pool, sms.company_id, &to).await? {
        return Err(failure(400, "opted_out"));
    }
    if let Some(key) = &sms.idempotency_key
        && let Some(id) = already_sent(pool, sms.company_id, key).await?
    {
        return Ok(vec![id]);
    }
    let worker = picture_worker();
    let mut sizes = Vec::with_capacity(sms.attachments.len());
    let mut worker_links = HashMap::new();
    for file in &sms.attachments {
        let picture = match worker.as_deref() {
            Some(base) => worker_picture(&state.telephony_http, base, file).await,
            None => None,
        };
        if let Some((link, size)) = picture {
            worker_links.insert(file.s3_key.clone(), link);
            sizes.push(Some(size));
            continue;
        }
        sizes.push(
            state
                .storage
                .size_in(None, &file.s3_key)
                .await
                .ok()
                .flatten(),
        );
    }
    let text = sms.text.trim();
    let planned = plan(text, &sms.attachments, &sizes);
    let delivery = Delivery {
        state,
        sms,
        config: &config,
        line,
        to,
        webhook_url: format!("{}/messaging", config.webhook_base(&state.phone)),
        worker_links,
    };
    let mut ids = Vec::with_capacity(planned.len());
    for (index, message) in planned.iter().enumerate() {
        // Only the first message of a split send reuses the caller's key.
        let key = match &sms.idempotency_key {
            Some(key) if index == 0 => key.clone(),
            _ => uuid::Uuid::new_v4().to_string(),
        };
        let (id, refused) = delivery.send(message, &key).await?;
        let Some(token) = refused else {
            ids.push(id);
            continue;
        };
        if ids.is_empty() {
            let status = if token == "opted_out" { 400 } else { 502 };
            return Err(failure(status, &token));
        }
        // Partly sent: the page keeps the files that did not go out.
        delivery.discard(id).await?;
        return Err(SmsFailure {
            status: 502,
            error: token,
            sent_ids: ids,
            unsent_s3_keys: planned[index..]
                .iter()
                .flat_map(|m| m.media.iter().chain(&m.links).map(|f| f.s3_key.clone()))
                .collect(),
        });
    }
    Ok(ids)
}

/// A file extension for a stored file of this type.
fn extension(content_type: &str) -> &str {
    match content_type {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "application/pdf" => "pdf",
        "text/vcard" => "vcf",
        "text/plain" => "txt",
        _ => content_type
            .rsplit('/')
            .next()
            .filter(|ext| {
                !ext.is_empty() && ext.len() <= 8 && ext.chars().all(|c| c.is_ascii_alphanumeric())
            })
            .unwrap_or("bin"),
    }
}

/// Pixel size of a picture.
fn dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

/// An inbound file as it is stored: a picture compressed as the CRM
/// compresses uploads (`compress_for_storage`), with its new type; anything
/// else, or a picture that already fits, as it came.
pub async fn stored_file(bytes: Vec<u8>, content_type: String) -> (Vec<u8>, String) {
    if crate::images::sniff(&bytes).is_none() {
        return (bytes, content_type);
    }
    let source = bytes.clone();
    match tokio::task::spawn_blocking(move || crate::images::compress_for_storage(&source)).await {
        Ok(Some(stored)) => (stored.bytes, stored.content_type.to_string()),
        _ => (bytes, content_type),
    }
}

/// Copies an inbound MMS's files from Telnyx (their links expire) into our
/// bucket (`sms-attachments/<company>/tx-<row>/`), as the row's attachments.
/// A file already copied (a redelivered webhook) is kept as it is; a file
/// that cannot be fetched is skipped (logged with ids only). How many were
/// stored.
pub async fn copy_inbound_media(
    state: &AppState,
    company_id: i32,
    sms_id: i64,
    payload: &MessagePayload,
) -> sqlx::Result<usize> {
    let mut stored = 0;
    for (position, media) in payload.media.iter().enumerate() {
        let Some(url) = media.url.as_deref().filter(|url| !url.is_empty()) else {
            continue;
        };
        let position = i32::try_from(position).unwrap_or(i32::MAX);
        let exists: Option<i32> = sqlx::query_scalar(
            "SELECT 1 FROM telnyx_sms_attachments WHERE telnyx_sms_id = ? AND position = ?",
        )
        .bind(sms_id)
        .bind(position)
        .fetch_optional(&state.pool)
        .await?;
        if exists.is_some() {
            continue;
        }
        let (bytes, served_type) = match state
            .telnyx
            .download_media(url, INBOUND_MEDIA_MAX_BYTES)
            .await
        {
            Ok(file) => file,
            Err(err) => {
                tracing::error!(company_id, sms_id, position, error = %err, "Telnyx MMS file not fetched");
                continue;
            }
        };
        let content_type = media
            .content_type
            .as_deref()
            .map(|kind| kind.trim().to_lowercase())
            .filter(|kind| !kind.is_empty())
            .or(served_type)
            .unwrap_or_else(|| "application/octet-stream".into());
        let (bytes, content_type) = stored_file(bytes, content_type).await;
        let is_image = content_type.starts_with("image/");
        let (width, height) = if is_image {
            dimensions(&bytes).map_or((None, None), |(w, h)| (Some(w), Some(h)))
        } else {
            (None, None)
        };
        let ext = extension(&content_type).to_string();
        let key = format!(
            "sms-attachments/{company_id}/tx-{sms_id}/{}.{ext}",
            uuid::Uuid::new_v4()
        );
        if let Err(err) = state.storage.put(&key, bytes, &content_type).await {
            tracing::error!(company_id, sms_id, error = %err, "Telnyx MMS file not stored");
            continue;
        }
        let Ok(public) = state.storage.public_url(&key) else {
            continue;
        };
        let filename = format!("{}.{ext}", if is_image { "image" } else { "file" });
        stored += sqlx::query(
            "INSERT IGNORE INTO telnyx_sms_attachments
               (telnyx_sms_id, content_type, filename, s3_key, s3_url, width, height, position)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(sms_id)
        .bind(&content_type)
        .bind(filename)
        .bind(&key)
        .bind(public)
        .bind(width)
        .bind(height)
        .bind(position)
        .execute(&state.pool)
        .await?
        .rows_affected();
    }
    Ok(usize::try_from(stored).unwrap_or(usize::MAX))
}

/// What an inbound text did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Received {
    /// Its row.
    pub sms_id: i64,
    /// First delivery (a redelivery of a stored text sets nothing off).
    pub new: bool,
}

/// Stores an inbound text (`message.received`) to one of the company's
/// numbers, then, the first time: STOP / START, the customer's SMS flows
/// stop (they replied), and their deal leaves the first stage of its
/// pipeline (the other providers' inbound rule); and its files are copied.
/// `None` when it is not to one of the company's numbers or has no sender.
pub async fn receive(
    state: &AppState,
    company_id: i32,
    payload: &MessagePayload,
    occurred_at: Option<DateTime<Utc>>,
) -> Result<Option<Received>, ApiError> {
    let pool = &state.pool;
    let from = payload
        .from
        .as_ref()
        .and_then(|party| party.phone_number.as_deref())
        .and_then(to_e164);
    let to = payload
        .to
        .first()
        .and_then(|party| party.phone_number.as_deref())
        .and_then(to_e164);
    let (Some(telnyx_id), Some(from), Some(to)) = (payload.id.as_deref(), from, to) else {
        return Ok(None);
    };
    let (Some(customer10), Some(line10)) = (last10(&from), last10(&to)) else {
        return Ok(None);
    };
    let number_id: Option<i32> = sqlx::query_scalar(
        "SELECT id FROM telnyx_numbers WHERE company_id = ? AND last10 = ? LIMIT 1",
    )
    .bind(company_id)
    .bind(&line10)
    .fetch_optional(pool)
    .await?;
    let Some(number_id) = number_id else {
        tracing::warn!(
            company_id,
            "Telnyx text to a number that is not the company's: ignored"
        );
        return Ok(None);
    };
    let text = payload.text.clone().unwrap_or_default();
    let inserted = sqlx::query(
        "INSERT IGNORE INTO telnyx_sms
           (telnyx_id, sender, recipient, text, direction, status, company_id, created_date,
            sender10, recipient10, phone_digits, is_echo, number_id)
         VALUES (?, ?, ?, ?, 'inbound', 'received', ?, ?, ?, ?, ?, 0, ?)",
    )
    .bind(telnyx_id)
    .bind(e164_digits(&from))
    .bind(e164_digits(&to).unwrap_or_default())
    .bind(&text)
    .bind(company_id)
    .bind(stored_time(occurred_at.unwrap_or_else(Utc::now)))
    .bind(&customer10)
    .bind(&line10)
    .bind(&customer10)
    .bind(number_id)
    .execute(pool)
    .await?;
    let new = inserted.rows_affected() > 0;
    let sms_id: i64 = if new {
        i64::try_from(inserted.last_insert_id()).unwrap_or_default()
    } else {
        sqlx::query_scalar(
            "SELECT CAST(id AS SIGNED) FROM telnyx_sms WHERE company_id = ? AND telnyx_id = ?",
        )
        .bind(company_id)
        .bind(telnyx_id)
        .fetch_one(pool)
        .await?
    };
    if new {
        after_inbound(state, company_id, &customer10, &text).await;
    }
    // Copied on the first delivery; a redelivery fills in what failed.
    copy_inbound_media(state, company_id, sms_id, payload).await?;
    // The phones of the line's users hear about it (with its files stored).
    if new && texting_setup(pool, company_id).await?.is_some() {
        let key = super::texts::ThreadKey {
            number_id,
            other10: customer10.clone(),
        };
        super::texts::notify_inbound(state, company_id, &key, sms_id).await;
    }
    Ok(Some(Received { sms_id, new }))
}

/// The side effects of a new inbound text; each is best effort (logged
/// with ids only).
async fn after_inbound(state: &AppState, company_id: i32, customer10: &str, text: &str) {
    let pool = &state.pool;
    let opt = match opt_keyword(text) {
        Some(OptKeyword::OptOut) => opt_out(pool, company_id, customer10, "stop").await,
        Some(OptKeyword::OptIn) => opt_in(pool, company_id, customer10).await,
        None => Ok(()),
    };
    if let Err(err) = opt {
        tracing::error!(company_id, error = %err, "Telnyx STOP/START not recorded");
    }
    match crate::routes::sms::flow_stops::stop_after_customer_conversation(
        pool,
        SmsProvider::Telnyx,
        company_id,
        customer10,
    )
    .await
    {
        Ok(0) => {}
        Ok(stopped) => tracing::info!(company_id, stopped, "SMS flows stopped by a Telnyx reply"),
        Err(err) => {
            tracing::error!(company_id, error = %err, "SMS flows not stopped on a Telnyx reply")
        }
    }
    if let Err(err) = move_deal_on_reply(pool, company_id, customer10).await {
        tracing::error!(company_id, error = %err, "deal not moved on a Telnyx reply");
    }
}

/// The customer's latest open deal leaves the first stage of its pipeline
/// (a won deal stays): whether it moved.
async fn move_deal_on_reply(
    pool: &MySqlPool,
    company_id: i32,
    customer10: &str,
) -> sqlx::Result<bool> {
    let Some(customer) = crate::routes::sms::thread::customer_by_phone(
        pool,
        SmsProvider::Telnyx,
        company_id,
        customer10,
    )
    .await?
    else {
        return Ok(false);
    };
    let deal: Option<i64> = sqlx::query_scalar(
        "SELECT CAST(id AS SIGNED) FROM deals
          WHERE customer_id = ? AND list_id <> ? AND deleted_at IS NULL
          ORDER BY id DESC LIMIT 1",
    )
    .bind(customer.id)
    .bind(CLOSED_WON_LIST_ID)
    .fetch_optional(pool)
    .await?;
    match deal {
        Some(deal_id) => super::outcomes::move_out_of_first_stage(pool, company_id, deal_id).await,
        None => Ok(false),
    }
}

/// Telnyx's report on a text we sent (`message.sent` / `message.finalized`):
/// `sent` → its delivery status; `delivered` / `delivery_unconfirmed` →
/// kept with the time; `sending_failed` / `delivery_failed` → the text
/// failed, with Telnyx's code (`40300` also marks the phone opted out). A
/// later `message.sent` never undoes a final status. Whether the text is
/// ours.
pub async fn record_status(
    pool: &MySqlPool,
    company_id: i32,
    event_type: &str,
    payload: &MessagePayload,
    occurred_at: Option<DateTime<Utc>>,
) -> sqlx::Result<bool> {
    let Some(telnyx_id) = payload.id.as_deref() else {
        return Ok(false);
    };
    let row: Option<(i64, Option<String>)> = sqlx::query_as(
        "SELECT CAST(id AS SIGNED), phone_digits FROM telnyx_sms
          WHERE company_id = ? AND telnyx_id = ? AND direction = 'outbound'",
    )
    .bind(company_id)
    .bind(telnyx_id)
    .fetch_optional(pool)
    .await?;
    let Some((id, phone_digits)) = row else {
        return Ok(false);
    };
    let status = payload.status().unwrap_or(if event_type == "message.sent" {
        "sent"
    } else {
        "delivery_unconfirmed"
    });
    let at = occurred_at.unwrap_or_else(Utc::now);
    match status {
        "sending_failed" | "delivery_failed" => {
            let code = payload.error_code();
            let token = match code.as_deref() {
                Some(OPTED_OUT_CODE) => "opted_out".to_string(),
                Some(code) if code.chars().all(|c| c.is_ascii_digit()) => format!("telnyx_{code}"),
                _ => status.to_string(),
            };
            sqlx::query(
                "UPDATE telnyx_sms SET status = 'failed', delivery_status = ?, error_code = ?,
                        error_message = ?
                  WHERE id = ?",
            )
            .bind(status)
            .bind(&code)
            .bind(token)
            .bind(id)
            .execute(pool)
            .await?;
            if code.as_deref() == Some(OPTED_OUT_CODE)
                && let Some(phone) = phone_digits.filter(|phone| phone.len() == 10)
            {
                opt_out(pool, company_id, &phone, "carrier").await?;
            }
        }
        "delivered" | "delivery_unconfirmed" => {
            sqlx::query(
                "UPDATE telnyx_sms SET delivery_status = ?,
                        delivered_at = IF(? = 'delivered', ?, delivered_at),
                        status = IF(status = 'pending', 'sent', status)
                  WHERE id = ? AND status <> 'failed'",
            )
            .bind(status)
            .bind(status)
            .bind(at.naive_utc())
            .bind(id)
            .execute(pool)
            .await?;
        }
        other => {
            // queued / sending / sent: only before a final status.
            sqlx::query(
                "UPDATE telnyx_sms SET delivery_status = ?, status = IF(status = 'pending', 'sent', status)
                  WHERE id = ? AND status <> 'failed'
                    AND (delivery_status IS NULL OR delivery_status IN ('queued', 'sending', 'sent'))",
            )
            .bind(other.chars().take(32).collect::<String>())
            .bind(id)
            .execute(pool)
            .await?;
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, content_type: &str) -> OutgoingSmsFile {
        OutgoingSmsFile {
            s3_key: format!("sms-attachments/1/m/{name}"),
            url: format!("https://bucket/{name}"),
            content_type: content_type.into(),
            filename: name.into(),
            width: None,
            height: None,
        }
    }

    #[test]
    fn photos_go_through_the_picture_worker() {
        let base = "https://worker.test";
        assert_eq!(
            worker_picture_url(base, &file("a.jpg", "image/jpeg")).as_deref(),
            Some("https://worker.test/sms-attachments/1/m/a.jpg?for=mms")
        );
        assert!(worker_picture_url(base, &file("b.png", "image/png")).is_some());
        // An animated GIF and a PDF go as stored.
        assert!(worker_picture_url(base, &file("c.gif", "image/gif")).is_none());
        assert!(worker_picture_url(base, &file("d.pdf", "application/pdf")).is_none());
    }

    #[test]
    fn stop_and_start_are_whole_texts() {
        assert_eq!(opt_keyword("STOP"), Some(OptKeyword::OptOut));
        assert_eq!(opt_keyword(" stop. "), Some(OptKeyword::OptOut));
        assert_eq!(opt_keyword("Stop all"), Some(OptKeyword::OptOut));
        assert_eq!(opt_keyword("unsubscribe"), Some(OptKeyword::OptOut));
        // The FCC's words too, any case and spacing.
        for text in [
            "REVOKE",
            "revoke!",
            "Opt out",
            "OPT   OUT.",
            " optout ",
            "Opt-Out",
            "quit",
            "End",
            "cancel",
        ] {
            assert_eq!(opt_keyword(text), Some(OptKeyword::OptOut), "{text}");
        }
        assert_eq!(opt_keyword("I want to opt out of the quote"), None);
        assert_eq!(opt_keyword("revoke my discount?"), None);
        assert_eq!(opt_keyword("Start"), Some(OptKeyword::OptIn));
        assert_eq!(opt_keyword("UNSTOP"), Some(OptKeyword::OptIn));
        assert_eq!(opt_keyword("please stop texting me"), None);
        assert_eq!(opt_keyword("Can't stop thinking about the slab"), None);
        assert_eq!(opt_keyword(""), None);
    }

    #[test]
    fn text_times_are_whole_seconds_cut_down() {
        let at = DateTime::parse_from_rfc3339("2026-10-23T12:00:00.900Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(stored_time(at).to_rfc3339(), "2026-10-23T12:00:00+00:00");
    }

    #[test]
    fn files_split_into_one_megabyte_messages_and_links() {
        let files = vec![
            file("a.jpg", "image/jpeg"),
            file("b.png", "image/png"),
            file("big.pdf", "application/pdf"),
            file("c.jpg", "image/jpeg"),
            file("notes.docx", "application/msword"),
        ];
        let sizes = [
            Some(600_000),
            Some(300_000),
            Some(2_000_000),
            Some(500_000),
            Some(10_000),
        ];
        let planned = plan("Your quote", &files, &sizes);
        let names =
            |list: &[&OutgoingSmsFile]| list.iter().map(|f| f.filename.clone()).collect::<Vec<_>>();
        assert_eq!(planned.len(), 3);
        assert_eq!(planned[0].text, "Your quote");
        assert_eq!(names(&planned[0].media), ["a.jpg", "b.png"]);
        assert_eq!(planned[1].text, "");
        assert_eq!(names(&planned[1].media), ["c.jpg"]);
        assert_eq!(names(&planned[2].links), ["big.pdf", "notes.docx"]);
        assert_eq!(
            planned[2].body,
            "File 1: big.pdf\nhttps://bucket/big.pdf\nFile 2: notes.docx\nhttps://bucket/notes.docx"
        );
        // Nothing as MMS: the caption goes with the links.
        let only_links = plan("See", &files[2..3], &[Some(2_000_000)]);
        assert_eq!(only_links.len(), 1);
        assert_eq!(only_links[0].text, "See");
        assert_eq!(
            only_links[0].body,
            "See\nFile 1: big.pdf\nhttps://bucket/big.pdf"
        );
        // An unknown size is never risked as MMS.
        assert!(plan("x", &files[..1], &[None])[0].media.is_empty());
        // A plain text is one message.
        assert_eq!(plan("Hi", &[], &[]).len(), 1);
    }

    #[test]
    fn eleven_pictures_are_two_messages() {
        let files: Vec<OutgoingSmsFile> = (0..11)
            .map(|n| file(&format!("{n}.jpg"), "image/jpeg"))
            .collect();
        let planned = plan("", &files, &[Some(10_000); 11]);
        assert_eq!(planned.len(), 2);
        assert_eq!(planned[0].media.len(), MMS_MAX_FILES);
        assert_eq!(planned[1].media.len(), 1);
    }

    #[test]
    fn errors_become_tokens() {
        let status = |code: &str| TelnyxError::Status {
            status: 400,
            code: Some(code.into()),
        };
        assert_eq!(error_token(&status("40300")), "opted_out");
        assert_eq!(error_token(&status("40310")), "telnyx_40310");
        assert_eq!(error_token(&status("bad code")), "send_failed");
        assert_eq!(
            error_token(&TelnyxError::NotConfigured),
            "telnyx_not_configured"
        );
        assert_eq!(extension("image/jpeg"), "jpg");
        assert_eq!(extension("video/mp4"), "mp4");
        assert_eq!(extension("application/vnd.ms-excel"), "bin");
    }
}
