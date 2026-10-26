//! The phone app's Texts tab: the user's Telnyx texts as threads of one of
//! their lines and one other number (the CRM's SMS page threads by the other
//! number alone). Same rows as the CRM (`telnyx_sms`, see
//! [`super::texting`]), same visibility: the numbers the user may call from.
//!
//! A thread's key is `{our number id}-{other number's last ten digits}`.
//! Unread: inbound texts on the line newer than both the user's read marker
//! for the other number (`telnyx_sms_thread_reads`, shared with the CRM, so
//! marking read here clears the CRM's badge too) and the line's last text to
//! them; a CRM "mark unread" counts as one.

use super::contacts::customers_for_numbers;
use super::devices::push_to_users;
use super::lines::{self, CallerId};
use super::numbers::last10;
use super::wire_enum;
use crate::openapi::ToSchema;
use crate::serde_helpers::js_date_required;
use crate::state::AppState;
use chrono::{DateTime, Utc};
use lambda_http::tracing;
use serde::Serialize;
use sqlx::{FromRow, MySqlPool, QueryBuilder};
use std::collections::{BTreeMap, HashMap};

/// The read marker a CRM "mark unread" leaves (see `routes::sms::rollup`).
const MANUAL_UNREAD_BEFORE: &str = "1971-01-01 00:00:00";

wire_enum! {
    /// Where a text is: `queued` (Telnyx has it), `sent` (handed to the
    /// carrier), `delivered`, `failed`, or `received` (an inbound text).
    pub enum TextStatus {
        /// Telnyx took it and has not sent it yet.
        Queued = "queued",
        /// Handed to the carrier (no delivery receipt yet, or none coming).
        Sent = "sent",
        /// The carrier confirmed delivery.
        Delivered = "delivered",
        /// It did not go out, or the carrier did not deliver it.
        Failed = "failed",
        /// An inbound text.
        Received = "received",
    }
}

wire_enum! {
    /// Why a text failed: `opted_out` (the person texted STOP),
    /// `invalid_number`, `carrier_rejected` or `failed` (anything else).
    pub enum TextError {
        /// The person texted STOP: no texts until they text START.
        OptedOut = "opted_out",
        /// Not a number that can get texts.
        InvalidNumber = "invalid_number",
        /// The carrier refused it (spam filter, unregistered sender ...).
        CarrierRejected = "carrier_rejected",
        /// It failed for another reason.
        Failed = "failed",
    }
}

/// A thread's key: our line and the other number.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ThreadKey {
    /// Our number (`telnyx_numbers.id`).
    pub number_id: i32,
    /// The other number's last ten digits.
    pub other10: String,
}

impl ThreadKey {
    /// The key as the app sends it back: `{number id}-{last ten digits}`.
    pub fn encode(&self) -> String {
        format!("{}-{}", self.number_id, self.other10)
    }

    /// A key the app sent; `None` when it is not one.
    pub fn parse(key: &str) -> Option<Self> {
        let (number, other) = key.trim().split_once('-')?;
        let number_id: i32 = number.parse().ok().filter(|id| *id > 0)?;
        (other.len() == 10 && other.bytes().all(|b| b.is_ascii_digit())).then(|| Self {
            number_id,
            other10: other.to_string(),
        })
    }
}

/// One of our numbers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TextNumber {
    /// The number's id (`ourNumberId` when texting from it).
    pub id: i32,
    /// E.164.
    pub e164: String,
    /// Its label, e.g. "Main line"; may be empty.
    pub label: String,
}

/// The customer the other number belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TextCustomer {
    /// The customer's id.
    pub id: i32,
    /// Their name.
    pub name: String,
    /// Their company; null when none.
    pub company: Option<String>,
}

/// The newest text of a thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TextLast {
    /// `inbound` or `outbound`.
    pub direction: String,
    /// Its text; empty for a picture or file alone.
    pub text: String,
    /// It carried pictures or files.
    pub has_media: bool,
    /// When it was sent or received (RFC 3339).
    #[serde(serialize_with = "js_date_required")]
    pub at: DateTime<Utc>,
    /// Where it is.
    pub status: TextStatus,
}

/// A conversation of one of my lines with one other number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TextThread {
    /// Opaque and stable: our number and the other number.
    pub key: String,
    /// Our line.
    pub our_number: TextNumber,
    /// The other number, E.164.
    pub other_number: String,
    /// The customer with that number; null when none.
    pub customer: Option<TextCustomer>,
    /// The newest text.
    pub last: TextLast,
    /// My unread texts in it.
    pub unread: i64,
}

/// A picture or file of a text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TextMedia {
    /// Its id (`GET /v1/phone/texts/media/{id}` gives a link to it).
    pub id: String,
    /// MIME type, e.g. `image/jpeg`.
    pub content_type: String,
    /// The file's name; null when unknown.
    pub name: Option<String>,
    /// Its size in bytes; null when unknown.
    pub size: Option<i64>,
}

/// One text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TextMessage {
    /// Our id of the text (`before` when paging).
    pub id: String,
    /// `inbound` or `outbound`.
    pub direction: String,
    /// The text; empty for pictures or files alone.
    pub text: String,
    /// Its pictures and files, in order.
    pub media: Vec<TextMedia>,
    /// Where it is.
    pub status: TextStatus,
    /// Why it failed; null unless `status` is `failed`.
    pub error: Option<TextError>,
    /// When it was sent or received (RFC 3339).
    #[serde(serialize_with = "js_date_required")]
    pub at: DateTime<Utc>,
    /// Who sent an outbound text; null for inbound and automated ones.
    pub sent_by: Option<String>,
}

/// The status the app shows for a stored row.
pub fn status_of(direction: &str, status: &str, delivery: Option<&str>) -> TextStatus {
    if direction == "inbound" {
        return TextStatus::Received;
    }
    match (status, delivery) {
        ("failed", _) => TextStatus::Failed,
        ("pending", _) | (_, Some("queued" | "sending")) => TextStatus::Queued,
        (_, Some("delivered")) => TextStatus::Delivered,
        _ => TextStatus::Sent,
    }
}

/// Why a failed row failed.
pub fn error_of(status: &str, message: Option<&str>, code: Option<&str>) -> Option<TextError> {
    if status != "failed" {
        return None;
    }
    Some(match (message, code) {
        (Some("opted_out"), _) | (_, Some(super::texting::OPTED_OUT_CODE)) => TextError::OptedOut,
        (Some("invalid_phone"), _) | (_, Some("40001" | "40012" | "40310")) => {
            TextError::InvalidNumber
        }
        (_, Some(code)) if code.starts_with("40") => TextError::CarrierRejected,
        _ => TextError::Failed,
    })
}

/// The active numbers `user_id` may call from, by id: their Texts lines.
pub async fn my_lines(
    pool: &MySqlPool,
    company_id: i32,
    user_id: i32,
) -> sqlx::Result<BTreeMap<i32, CallerId>> {
    Ok(lines::caller_ids(pool, company_id, user_id)
        .await?
        .into_iter()
        .map(|line| (line.number_id, line))
        .collect())
}

/// Which threads to list.
#[derive(Debug, Default, Clone)]
pub struct ThreadFilter {
    /// Only threads with this other number (last ten digits).
    pub other10: Option<String>,
    /// Only this thread.
    pub key: Option<ThreadKey>,
    /// Only threads whose newest text is older than this one (paging).
    pub before: Option<i64>,
}

#[derive(FromRow)]
struct ThreadRow {
    number_id: i32,
    phone_digits: String,
    last_id: i64,
}

/// The newest-first `(line, other number, newest text id)` of the threads
/// on `number_ids`, at most `limit`.
async fn thread_rows(
    pool: &MySqlPool,
    company_id: i32,
    number_ids: &[i32],
    filter: &ThreadFilter,
    limit: i64,
) -> sqlx::Result<Vec<ThreadRow>> {
    if number_ids.is_empty() {
        return Ok(Vec::new());
    }
    let company_lines = super::texting::company_lines(pool, company_id).await?;
    let mut query = QueryBuilder::new(
        "SELECT s.number_id, s.phone_digits, CAST(MAX(s.id) AS SIGNED) AS last_id
           FROM telnyx_sms s
          WHERE s.is_echo = 0 AND s.phone_digits IS NOT NULL AND s.company_id = ",
    );
    query.push_bind(company_id).push(" AND s.number_id IN (");
    let mut ids = query.separated(", ");
    for id in number_ids {
        ids.push_bind(*id);
    }
    query.push(")");
    if !company_lines.is_empty() {
        query.push(" AND s.phone_digits NOT IN (");
        let mut ours = query.separated(", ");
        for line in &company_lines {
            ours.push_bind(line.clone());
        }
        query.push(")");
    }
    if let Some(other) = &filter.other10 {
        query
            .push(" AND s.phone_digits = ")
            .push_bind(other.clone());
    }
    if let Some(key) = &filter.key {
        query
            .push(" AND s.number_id = ")
            .push_bind(key.number_id)
            .push(" AND s.phone_digits = ")
            .push_bind(key.other10.clone());
    }
    query.push(" GROUP BY s.number_id, s.phone_digits");
    if let Some(before) = filter.before {
        query.push(" HAVING MAX(s.id) < ").push_bind(before);
    }
    query
        .push(" ORDER BY last_id DESC LIMIT ")
        .push_bind(limit.max(0));
    query.build_query_as().fetch_all(pool).await
}

#[derive(FromRow)]
struct UnreadRow {
    number_id: i32,
    phone_digits: String,
    unread: i64,
    manual: bool,
}

/// My unread texts per thread of `number_ids` (only `only` when given).
async fn unread_by_thread(
    pool: &MySqlPool,
    company_id: i32,
    user_id: i32,
    number_ids: &[i32],
    only: Option<&[(i32, String)]>,
) -> sqlx::Result<HashMap<(i32, String), i64>> {
    if number_ids.is_empty() || only.is_some_and(<[_]>::is_empty) {
        return Ok(HashMap::new());
    }
    let mut query = QueryBuilder::new(
        "SELECT g.number_id, g.phone_digits,
                CAST(COALESCE(SUM(u.direction = 'inbound' AND u.created_date > GREATEST(
                    COALESCE(r.last_read_at, '1970-01-01 00:00:00'),
                    COALESCE(g.last_out, '1970-01-01 00:00:00'))), 0) AS SIGNED) AS unread,
                COALESCE(MAX(r.last_read_at < '",
    );
    query.push(MANUAL_UNREAD_BEFORE).push(
        "'), 0) <> 0 AS `manual`
           FROM (SELECT o.number_id, o.phone_digits,
                        MAX(CASE WHEN o.direction = 'outbound' AND o.status IN ('sent', 'pending')
                                 THEN o.created_date END) AS last_out
                   FROM telnyx_sms o
                  WHERE o.is_echo = 0 AND o.phone_digits IS NOT NULL AND o.company_id = ",
    );
    query.push_bind(company_id).push(" AND o.number_id IN (");
    let mut ids = query.separated(", ");
    for id in number_ids {
        ids.push_bind(*id);
    }
    query.push(")");
    if let Some(only) = only {
        query.push(" AND (FALSE");
        for (number_id, phone) in only {
            query
                .push(" OR (o.number_id = ")
                .push_bind(*number_id)
                .push(" AND o.phone_digits = ")
                .push_bind(phone.clone())
                .push(")");
        }
        query.push(")");
    }
    query.push(
        " GROUP BY o.number_id, o.phone_digits) g
           JOIN telnyx_sms u ON u.number_id = g.number_id AND u.phone_digits = g.phone_digits
                            AND u.is_echo = 0 AND u.company_id = ",
    );
    query
        .push_bind(company_id)
        .push(" LEFT JOIN telnyx_sms_thread_reads r ON r.user_id = ")
        .push_bind(user_id)
        .push(" AND r.company_id = ")
        .push_bind(company_id)
        .push(" AND r.customer_phone_digits = g.phone_digits GROUP BY g.number_id, g.phone_digits");
    let rows: Vec<UnreadRow> = query.build_query_as().fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let unread = if row.manual {
                row.unread.max(1)
            } else {
                row.unread
            };
            ((row.number_id, row.phone_digits), unread)
        })
        .collect())
}

/// My unread texts over every thread of my lines (the Texts badge).
pub async fn unread_total(pool: &MySqlPool, company_id: i32, user_id: i32) -> sqlx::Result<i64> {
    let lines = my_lines(pool, company_id, user_id).await?;
    let number_ids: Vec<i32> = lines.keys().copied().collect();
    let company_lines = super::texting::company_lines(pool, company_id).await?;
    Ok(
        unread_by_thread(pool, company_id, user_id, &number_ids, None)
            .await?
            .into_iter()
            .filter(|((_, phone), _)| !company_lines.contains(phone))
            .map(|(_, unread)| unread)
            .sum(),
    )
}

#[derive(FromRow)]
struct LastRow {
    id: i64,
    direction: String,
    text: String,
    status: String,
    delivery_status: Option<String>,
    created_date: DateTime<Utc>,
    has_media: bool,
}

/// The threads of `lines` (newest activity first), at most `limit`, and the
/// cursor of the next page.
pub async fn threads(
    pool: &MySqlPool,
    company_id: i32,
    user_id: i32,
    lines: &BTreeMap<i32, CallerId>,
    filter: &ThreadFilter,
    limit: i64,
) -> sqlx::Result<(Vec<TextThread>, Option<String>)> {
    let number_ids: Vec<i32> = lines.keys().copied().collect();
    let mut rows = thread_rows(pool, company_id, &number_ids, filter, limit + 1).await?;
    let more = i64::try_from(rows.len()).unwrap_or(i64::MAX) > limit;
    rows.truncate(usize::try_from(limit).unwrap_or(0));
    if rows.is_empty() {
        return Ok((Vec::new(), None));
    }
    let keys: Vec<(i32, String)> = rows
        .iter()
        .map(|row| (row.number_id, row.phone_digits.clone()))
        .collect();
    let unread = unread_by_thread(pool, company_id, user_id, &number_ids, Some(&keys)).await?;
    let mut last_query = QueryBuilder::new(
        "SELECT CAST(s.id AS SIGNED) AS id, CAST(s.direction AS CHAR) AS direction, s.text,
                CAST(s.status AS CHAR) AS status, s.delivery_status, s.created_date,
                EXISTS (SELECT 1 FROM telnyx_sms_attachments a WHERE a.telnyx_sms_id = s.id) AS has_media
           FROM telnyx_sms s WHERE s.company_id = ",
    );
    last_query.push_bind(company_id).push(" AND s.id IN (");
    let mut ids = last_query.separated(", ");
    for row in &rows {
        ids.push_bind(row.last_id);
    }
    last_query.push(")");
    let last: HashMap<i64, LastRow> = last_query
        .build_query_as::<LastRow>()
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|row| (row.id, row))
        .collect();
    let others: Vec<String> = keys.iter().map(|(_, phone)| phone.clone()).collect();
    let customers = customers(pool, company_id, &others).await?;
    let threads = rows
        .iter()
        .filter_map(|row| {
            let line = lines.get(&row.number_id)?;
            let last = last.get(&row.last_id)?;
            let key = ThreadKey {
                number_id: row.number_id,
                other10: row.phone_digits.clone(),
            };
            Some(TextThread {
                key: key.encode(),
                our_number: TextNumber {
                    id: line.number_id,
                    e164: line.e164.clone(),
                    label: line.label.clone(),
                },
                other_number: format!("+1{}", row.phone_digits),
                customer: customers.get(&row.phone_digits).cloned(),
                last: TextLast {
                    direction: last.direction.clone(),
                    text: last.text.clone(),
                    has_media: last.has_media,
                    at: last.created_date,
                    status: status_of(
                        &last.direction,
                        &last.status,
                        last.delivery_status.as_deref(),
                    ),
                },
                unread: unread
                    .get(&(row.number_id, row.phone_digits.clone()))
                    .copied()
                    .unwrap_or(0),
            })
        })
        .collect();
    let next = more
        .then(|| rows.last().map(|row| row.last_id.to_string()))
        .flatten();
    Ok((threads, next))
}

/// The customers with these numbers (last ten digits), with their name and
/// company.
async fn customers(
    pool: &MySqlPool,
    company_id: i32,
    numbers: &[String],
) -> sqlx::Result<HashMap<String, TextCustomer>> {
    let ids = customers_for_numbers(pool, company_id, numbers).await?;
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let mut query = QueryBuilder::new(
        "SELECT id, COALESCE(name, '') AS name, NULLIF(TRIM(company_name), '') AS company
           FROM customers WHERE company_id = ",
    );
    query.push_bind(company_id).push(" AND id IN (");
    let mut list = query.separated(", ");
    for id in ids.values() {
        list.push_bind(*id);
    }
    query.push(")");
    let rows: Vec<(i32, String, Option<String>)> = query.build_query_as().fetch_all(pool).await?;
    let by_id: HashMap<i32, TextCustomer> = rows
        .into_iter()
        .map(|(id, name, company)| (id, TextCustomer { id, name, company }))
        .collect();
    Ok(ids
        .into_iter()
        .filter_map(|(phone, id)| Some((phone, by_id.get(&id)?.clone())))
        .collect())
}

#[derive(FromRow)]
struct MessageRow {
    id: i64,
    direction: String,
    text: String,
    status: String,
    delivery_status: Option<String>,
    error_message: Option<String>,
    error_code: Option<String>,
    created_date: DateTime<Utc>,
    is_automated: bool,
    sender_name: Option<String>,
}

const MESSAGE_COLUMNS: &str =
    "SELECT CAST(s.id AS SIGNED) AS id, CAST(s.direction AS CHAR) AS direction,
        s.text, CAST(s.status AS CHAR) AS status, s.delivery_status, s.error_message, s.error_code,
        s.created_date, s.is_automated <> 0 AS is_automated, u.name AS sender_name
   FROM telnyx_sms s LEFT JOIN users u ON u.id = s.sender_user_id
  WHERE s.is_echo = 0 AND s.company_id = ";

/// The rows as texts, with their media.
async fn to_messages(pool: &MySqlPool, rows: Vec<MessageRow>) -> sqlx::Result<Vec<TextMessage>> {
    let mut media: HashMap<i64, Vec<TextMedia>> = HashMap::new();
    if !rows.is_empty() {
        let mut query = QueryBuilder::new(
            "SELECT CAST(id AS SIGNED), CAST(telnyx_sms_id AS SIGNED), content_type,
                    NULLIF(filename, '') FROM telnyx_sms_attachments WHERE telnyx_sms_id IN (",
        );
        let mut ids = query.separated(", ");
        for row in &rows {
            ids.push_bind(row.id);
        }
        query.push(") ORDER BY telnyx_sms_id, position, id");
        let files: Vec<(i64, i64, String, Option<String>)> =
            query.build_query_as().fetch_all(pool).await?;
        for (id, sms_id, content_type, name) in files {
            media.entry(sms_id).or_default().push(TextMedia {
                id: id.to_string(),
                content_type,
                name,
                size: None,
            });
        }
    }
    Ok(rows
        .into_iter()
        .map(|row| {
            let outbound = row.direction == "outbound";
            TextMessage {
                id: row.id.to_string(),
                media: media.remove(&row.id).unwrap_or_default(),
                status: status_of(&row.direction, &row.status, row.delivery_status.as_deref()),
                error: error_of(
                    &row.status,
                    row.error_message.as_deref(),
                    row.error_code.as_deref(),
                ),
                at: row.created_date,
                sent_by: (outbound && !row.is_automated)
                    .then_some(row.sender_name)
                    .flatten(),
                direction: row.direction,
                text: row.text,
            }
        })
        .collect())
}

/// The newest texts of a thread (before `before`), newest first, at most
/// `limit`, and the cursor of the next page.
pub async fn messages(
    pool: &MySqlPool,
    company_id: i32,
    key: &ThreadKey,
    before: Option<i64>,
    limit: i64,
) -> sqlx::Result<(Vec<TextMessage>, Option<String>)> {
    let mut query = QueryBuilder::new(MESSAGE_COLUMNS);
    query
        .push_bind(company_id)
        .push(" AND s.number_id = ")
        .push_bind(key.number_id)
        .push(" AND s.phone_digits = ")
        .push_bind(key.other10.clone());
    if let Some(before) = before {
        query.push(" AND s.id < ").push_bind(before);
    }
    query
        .push(" ORDER BY s.id DESC LIMIT ")
        .push_bind(limit + 1);
    let mut rows: Vec<MessageRow> = query.build_query_as().fetch_all(pool).await?;
    let more = i64::try_from(rows.len()).unwrap_or(i64::MAX) > limit;
    rows.truncate(usize::try_from(limit).unwrap_or(0));
    let next = more
        .then(|| rows.last().map(|row| row.id.to_string()))
        .flatten();
    Ok((to_messages(pool, rows).await?, next))
}

/// One text of the company, by id.
pub async fn message(
    pool: &MySqlPool,
    company_id: i32,
    id: i64,
) -> sqlx::Result<Option<TextMessage>> {
    let mut query = QueryBuilder::new(MESSAGE_COLUMNS);
    query
        .push_bind(company_id)
        .push(" AND s.id = ")
        .push_bind(id);
    let rows: Vec<MessageRow> = query.build_query_as().fetch_all(pool).await?;
    Ok(to_messages(pool, rows).await?.into_iter().next())
}

/// Marks the other number's texts read for `user_id` (the CRM's marker).
pub async fn mark_read(
    pool: &MySqlPool,
    company_id: i32,
    user_id: i32,
    other10: &str,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO telnyx_sms_thread_reads (user_id, company_id, customer_phone_digits, last_read_at)
         VALUES (?, ?, ?, UTC_TIMESTAMP())
         ON DUPLICATE KEY UPDATE last_read_at = GREATEST(last_read_at, VALUES(last_read_at))",
    )
    .bind(user_id)
    .bind(company_id)
    .bind(other10)
    .execute(pool)
    .await?;
    Ok(())
}

/// A file of a text on one of `number_ids`: its key, type and name.
pub async fn media_file(
    pool: &MySqlPool,
    company_id: i32,
    number_ids: &[i32],
    media_id: i64,
) -> sqlx::Result<Option<(String, String, Option<String>)>> {
    if number_ids.is_empty() {
        return Ok(None);
    }
    let mut query = QueryBuilder::new(
        "SELECT a.s3_key, a.content_type, NULLIF(a.filename, '')
           FROM telnyx_sms_attachments a
           JOIN telnyx_sms s ON s.id = a.telnyx_sms_id
          WHERE a.id = ",
    );
    query
        .push_bind(media_id)
        .push(" AND s.company_id = ")
        .push_bind(company_id)
        .push(" AND s.number_id IN (");
    let mut ids = query.separated(", ");
    for id in number_ids {
        ids.push_bind(*id);
    }
    query.push(")");
    query.build_query_as().fetch_optional(pool).await
}

/// The thread of the company's text `id` (not an echo); `None` when there
/// is no such text.
pub async fn thread_of(
    pool: &MySqlPool,
    company_id: i32,
    id: i64,
) -> sqlx::Result<Option<ThreadKey>> {
    let row: Option<(i32, String)> = sqlx::query_as(
        "SELECT number_id, phone_digits FROM telnyx_sms
          WHERE id = ? AND company_id = ? AND is_echo = 0
            AND number_id IS NOT NULL AND phone_digits IS NOT NULL",
    )
    .bind(id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(number_id, other10)| ThreadKey { number_id, other10 }))
}

/// Tells the phones of everyone who may text from the line about the new
/// inbound text `text_id` (`{granite: "1", type: "text", textId}`: no text,
/// number or thread key, which ends in the other number; the app asks
/// `GET /v1/phone/texts/messages/{id}/thread`), while the company texts
/// through Telnyx. Best effort.
pub async fn notify_inbound(state: &AppState, company_id: i32, key: &ThreadKey, text_id: i64) {
    let users: sqlx::Result<Vec<i32>> = sqlx::query_scalar(
        "SELECT nu.user_id FROM telnyx_number_users nu
           JOIN telnyx_numbers n ON n.id = nu.number_id AND n.company_id = ? AND n.active <> 0
           JOIN users u ON u.id = nu.user_id AND u.company_id = n.company_id AND u.is_deleted = 0
          WHERE nu.number_id = ? AND nu.can_call_from <> 0",
    )
    .bind(company_id)
    .bind(key.number_id)
    .fetch_all(&state.pool)
    .await;
    let users = match users {
        Ok(users) if !users.is_empty() => users,
        Ok(_) => return,
        Err(err) => {
            tracing::warn!(company_id, error = %err, "text push recipients not read");
            return;
        }
    };
    let data = BTreeMap::from([
        ("granite".to_string(), "1".to_string()),
        ("type".to_string(), "text".to_string()),
        ("textId".to_string(), text_id.to_string()),
    ]);
    if let Err(err) = push_to_users(state, company_id, &users, &data).await {
        tracing::warn!(company_id, error = %err, "text push not sent");
    }
}

/// The thread a stored text belongs to.
pub fn key_of(number_id: i32, phone: &str) -> Option<ThreadKey> {
    Some(ThreadKey {
        number_id,
        other10: last10(phone)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_round_trip_and_reject_junk() {
        let key = ThreadKey::parse("9701-3175550100").unwrap();
        assert_eq!((key.number_id, key.other10.as_str()), (9701, "3175550100"));
        assert_eq!(key.encode(), "9701-3175550100");
        for bad in [
            "",
            "9701",
            "0-3175550100",
            "x-3175550100",
            "9701-317555010",
            "9701-31755501000",
            "9701-31755501aa",
        ] {
            assert_eq!(ThreadKey::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn statuses_and_errors_as_the_app_shows_them() {
        assert_eq!(status_of("inbound", "received", None), TextStatus::Received);
        assert_eq!(status_of("outbound", "pending", None), TextStatus::Queued);
        assert_eq!(
            status_of("outbound", "sent", Some("queued")),
            TextStatus::Queued
        );
        assert_eq!(
            status_of("outbound", "sent", Some("sent")),
            TextStatus::Sent
        );
        assert_eq!(status_of("outbound", "sent", None), TextStatus::Sent);
        assert_eq!(
            status_of("outbound", "sent", Some("delivered")),
            TextStatus::Delivered
        );
        assert_eq!(
            status_of("outbound", "failed", Some("delivered")),
            TextStatus::Failed
        );
        assert_eq!(error_of("sent", None, None), None);
        assert_eq!(
            error_of("failed", Some("opted_out"), Some("40300")),
            Some(TextError::OptedOut)
        );
        assert_eq!(
            error_of("failed", Some("invalid_phone"), None),
            Some(TextError::InvalidNumber)
        );
        assert_eq!(
            error_of("failed", Some("telnyx_40002"), Some("40002")),
            Some(TextError::CarrierRejected)
        );
        assert_eq!(
            error_of("failed", Some("pending_timeout"), None),
            Some(TextError::Failed)
        );
    }
}
