//! The contact index the app searches offline (D13/D15): every live customer
//! of the company (but the onboarding sample), and coworkers with a phone
//! number (a direct line, a RingCentral or CloudTalk number, their own).
//!
//! Syncing: a full download pages by id; a since-sync returns customers
//! changed (`customers.updated_at`), added or called since the cursor, and
//! the ids deleted since. Deal changes (open deal, stage) reach the phone
//! with the daily full download. The ETag is a hash of what the index
//! depends on, so an unchanged index answers 304.

use super::customer_index;
use super::numbers::{last10, to_e164};
use super::wire_enum;
use crate::routes::auth::sha256_hex;
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use sqlx::{FromRow, MySql, MySqlPool, QueryBuilder};
use std::collections::HashMap;
use std::fmt::Write as _;
use utoipa::ToSchema;

/// The onboarding sample customer, never shown in the app.
pub const SAMPLE_CUSTOMER_ID: i32 = 990_000_021;
/// Contacts per page of a full download (and most changes a since-sync
/// returns; more and the app gets a full download instead).
pub const INDEX_PAGE: usize = 5_000;
/// A cursor starts this long before it is made, so changes saved while a
/// download runs (or with a slightly different clock) come again.
const CURSOR_MARGIN_MINUTES: i64 = 15;
/// Older cursors get a full download.
const CURSOR_MAX_AGE_DAYS: i64 = 30;

wire_enum! {
    /// How warm a lead is: `hot`, `medium` or `cold`.
    pub enum Temperature {
        /// Hot lead.
        Hot = "hot",
        /// Medium.
        Medium = "medium",
        /// Cold.
        Cold = "cold",
    }
}

/// A customer's oldest open deal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct OpenDeal {
    /// The deal.
    pub id: i64,
    /// Its stage, e.g. "Got a Quote".
    pub stage: String,
}

/// A customer in the phone's contact index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct IndexContact {
    /// Customer id.
    pub id: i32,
    /// Name; empty when the customer has none.
    pub name: String,
    /// Company name; null when empty.
    pub company: Option<String>,
    /// The last 10 digits of the phone, then of phone 2 (missing, short or
    /// repeated ones left out).
    pub phones: Vec<String>,
    /// The sales rep's user id; null when unassigned.
    pub rep_id: Option<i32>,
    /// The caller is the customer's sales rep.
    pub mine: bool,
    /// `hot`, `medium` or `cold`; null when not set.
    pub temperature: Option<Temperature>,
    /// The oldest open deal (not won or lost); null when none.
    pub open_deal: Option<OpenDeal>,
    /// When the last call with this customer started (RFC 3339); null when
    /// never.
    #[serde(serialize_with = "crate::serde_helpers::js_date")]
    pub last_call_at: Option<DateTime<Utc>>,
}

/// A coworker the user can call: a user of the company with at least one
/// phone number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Coworker {
    /// User id.
    pub user_id: i32,
    /// Name (the email when the user has none).
    pub name: String,
    /// Their first number (`phones[0]`), E.164; the one to call.
    pub e164: Option<String>,
    /// That number's label; null when empty.
    pub label: Option<String>,
    /// Every number they have, first the Granite Phone direct lines, then
    /// RingCentral, CloudTalk and their own phone; no number twice.
    pub phones: Vec<CoworkerPhone>,
}

/// One of a coworker's numbers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct CoworkerPhone {
    /// The number, E.164.
    pub e164: String,
    /// What it is: a direct line's own label (null when it has none),
    /// `RingCentral`, `CloudTalk` or `Mobile` (their own phone).
    pub label: Option<String>,
}

#[derive(FromRow)]
struct ContactRow {
    id: i32,
    name: Option<String>,
    company_name: Option<String>,
    phone: Option<String>,
    phone_2: Option<String>,
    sales_rep: Option<i32>,
    temperature: Option<String>,
    deal_id: Option<i64>,
    deal_stage: Option<String>,
    last_call_at: Option<DateTime<Utc>>,
}

/// The columns of an index contact, `FROM customers c` (more joins may
/// follow it).
const CONTACT_COLUMNS: &str =
    "SELECT c.id, c.name, c.company_name, c.phone, c.phone_2, c.sales_rep,
        CAST(c.customer_temperature AS CHAR) AS temperature,
        CAST(d.id AS SIGNED) AS deal_id, COALESCE(dl.name, d.status) AS deal_stage,
        (SELECT MAX(pc.started_at) FROM phone_calls pc
          WHERE pc.company_id = c.company_id AND pc.customer_id = c.id) AS last_call_at
   FROM customers c";

/// The deal joins of an index contact, then `WHERE c.company_id = ` (bind
/// it next).
const CONTACT_DEALS: &str = "
   LEFT JOIN deals d ON d.id = (SELECT MIN(od.id) FROM deals od
                                 WHERE od.customer_id = c.id AND od.deleted_at IS NULL
                                   AND od.is_won IS NULL)
   LEFT JOIN deals_list dl ON dl.id = d.list_id
  WHERE c.deleted_at IS NULL AND c.id <> 990000021 AND c.company_id = ";

/// A trimmed text, `None` when empty.
fn non_empty(text: Option<String>) -> Option<String> {
    text.map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

/// The last-10 phones of a customer: phone, then phone 2.
pub fn index_phones(phone: Option<&str>, phone_2: Option<&str>) -> Vec<String> {
    let mut phones: Vec<String> = Vec::with_capacity(2);
    for digits in [phone, phone_2].into_iter().flatten().filter_map(last10) {
        if !phones.contains(&digits) {
            phones.push(digits);
        }
    }
    phones
}

impl ContactRow {
    fn contact(self, me: i32) -> IndexContact {
        IndexContact {
            id: self.id,
            name: non_empty(self.name).unwrap_or_default(),
            company: non_empty(self.company_name),
            phones: index_phones(self.phone.as_deref(), self.phone_2.as_deref()),
            rep_id: self.sales_rep,
            mine: self.sales_rep == Some(me),
            temperature: self.temperature.as_deref().and_then(Temperature::parse),
            open_deal: self.deal_id.map(|id| OpenDeal {
                id,
                stage: self.deal_stage.unwrap_or_default(),
            }),
            last_call_at: self.last_call_at,
        }
    }
}

async fn fetch(
    pool: &MySqlPool,
    mut query: QueryBuilder<MySql>,
    me: i32,
) -> sqlx::Result<Vec<IndexContact>> {
    let rows: Vec<ContactRow> = query.build_query_as().fetch_all(pool).await?;
    Ok(rows.into_iter().map(|row| row.contact(me)).collect())
}

fn contacts_of(company_id: i32) -> QueryBuilder<MySql> {
    let mut query = QueryBuilder::new(CONTACT_COLUMNS);
    query.push(CONTACT_DEALS).push_bind(company_id);
    query
}

/// One page of a full download: up to `limit` contacts with ids above
/// `after`, by id. `me` decides `mine`.
pub async fn page(
    pool: &MySqlPool,
    company_id: i32,
    me: i32,
    after: i32,
    limit: usize,
) -> sqlx::Result<Vec<IndexContact>> {
    let mut query = contacts_of(company_id);
    query
        .push(" AND c.id > ")
        .push_bind(after)
        .push(" ORDER BY c.id LIMIT ")
        .push_bind(i64::try_from(limit).unwrap_or(i64::MAX));
    fetch(pool, query, me).await
}

/// Up to `limit` contacts changed, added or called since `cursor`, by id.
///
/// The three ways in are found apart, each by an index (customers by
/// `(company_id, deleted_at, updated_at)` and by id, calls by
/// `(company_id, started_at)`), and joined by id.
pub async fn changed_since(
    pool: &MySqlPool,
    company_id: i32,
    me: i32,
    cursor: Cursor,
    limit: usize,
) -> sqlx::Result<Vec<IndexContact>> {
    let mut query = QueryBuilder::new(CONTACT_COLUMNS);
    query
        .push(" JOIN (SELECT ch.id FROM customers ch WHERE ch.company_id = ")
        .push_bind(company_id)
        .push(" AND ch.deleted_at IS NULL AND ch.updated_at >= ")
        .push_bind(cursor.since)
        .push(" UNION SELECT cn.id FROM customers cn WHERE cn.company_id = ")
        .push_bind(company_id)
        .push(" AND cn.id > ")
        .push_bind(cursor.max_id)
        .push(" UNION SELECT pc2.customer_id FROM phone_calls pc2 WHERE pc2.company_id = ")
        .push_bind(company_id)
        .push(" AND pc2.started_at >= ")
        .push_bind(cursor.since)
        .push(" AND pc2.customer_id IS NOT NULL) changed ON changed.id = c.id")
        .push(CONTACT_DEALS)
        .push_bind(company_id)
        .push(" ORDER BY c.id LIMIT ")
        .push_bind(i64::try_from(limit).unwrap_or(i64::MAX));
    fetch(pool, query, me).await
}

/// Ids of the company's customers deleted since `cursor` (that the phone
/// can have: not newer than the cursor).
pub async fn deleted_since(
    pool: &MySqlPool,
    company_id: i32,
    cursor: Cursor,
) -> sqlx::Result<Vec<i32>> {
    sqlx::query_scalar(
        "SELECT id FROM customers
          WHERE company_id = ? AND deleted_at IS NOT NULL AND updated_at >= ? AND id <= ?
            AND id <> 990000021
          ORDER BY id",
    )
    .bind(company_id)
    .bind(cursor.since)
    .bind(cursor.max_id)
    .fetch_all(pool)
    .await
}

/// The index contacts with these ids (missing, deleted or other companies'
/// ids are left out), by id.
pub async fn by_ids(
    pool: &MySqlPool,
    company_id: i32,
    me: i32,
    ids: &[i32],
) -> sqlx::Result<Vec<IndexContact>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut query = contacts_of(company_id);
    crate::sql::push_in(&mut query, "c.id", ids);
    query.push(" ORDER BY c.id");
    fetch(pool, query, me).await
}

/// One index contact.
pub async fn contact(
    pool: &MySqlPool,
    company_id: i32,
    me: i32,
    id: i32,
) -> sqlx::Result<Option<IndexContact>> {
    Ok(by_ids(pool, company_id, me, &[id]).await?.pop())
}

/// A `LIKE` pattern every phone whose last 10 digits are `last10` matches:
/// its last 4 digits in order, with anything between and around them (the
/// free-text phone may hold dashes, spaces or brackets anywhere). Cheap to
/// test, so the costly digit extraction runs only on the few that match.
fn last_digits_pattern(last10: &str) -> String {
    let tail: Vec<char> = last10.chars().filter(char::is_ascii_digit).collect();
    let mut pattern = String::from("%");
    for digit in &tail[tail.len().saturating_sub(4)..] {
        pattern.push(*digit);
        pattern.push('%');
    }
    pattern
}

/// Each of `numbers` (last-10 strings) mapped to the newest live customer of
/// the company with that phone or phone 2.
///
/// By the customer phone index once the company's is filled
/// ([`customer_index`]); until then by reading the company's phones (a quick
/// `LIKE` on the last digits, then the digits compared).
pub async fn customers_for_numbers(
    pool: &MySqlPool,
    company_id: i32,
    numbers: &[String],
) -> sqlx::Result<HashMap<String, i32>> {
    let mut found = HashMap::new();
    if numbers.is_empty() {
        return Ok(found);
    }
    if customer_index::built(pool, company_id).await? {
        return customer_index::lookup(pool, company_id, numbers).await;
    }
    let mut query = QueryBuilder::new(
        "SELECT c.id, c.phone, c.phone_2 FROM customers c
          WHERE c.deleted_at IS NULL AND c.id <> 990000021 AND c.company_id = ",
    );
    query.push_bind(company_id).push(" AND (FALSE");
    for column in ["c.phone", "c.phone_2"] {
        // The cheap prefilter first (MySQL tests ANDed conditions in order).
        query.push(format!(" OR (({column} LIKE "));
        let mut patterns = query.separated(format!(" OR {column} LIKE "));
        for number in numbers {
            patterns.push_bind(last_digits_pattern(number));
        }
        query.push(format!(
            ") AND RIGHT(REGEXP_REPLACE(COALESCE({column}, ''), '[^0-9]', ''), 10) IN ("
        ));
        let mut list = query.separated(", ");
        for number in numbers {
            list.push_bind(number.clone());
        }
        query.push("))");
    }
    query.push(") ORDER BY c.id DESC");
    let rows: Vec<(i32, Option<String>, Option<String>)> =
        query.build_query_as().fetch_all(pool).await?;
    for (id, phone, phone_2) in rows {
        for digits in index_phones(phone.as_deref(), phone_2.as_deref()) {
            if numbers.contains(&digits) {
                found.entry(digits).or_insert(id);
            }
        }
    }
    Ok(found)
}

/// The newest live customer of the company with this phone (last 10 digits).
pub async fn customer_for_number(
    pool: &MySqlPool,
    company_id: i32,
    last10: &str,
) -> sqlx::Result<Option<i32>> {
    let numbers = [last10.to_string()];
    Ok(customers_for_numbers(pool, company_id, &numbers)
        .await?
        .remove(last10))
}

/// A user and the numbers kept on their profile.
#[derive(FromRow)]
struct UserNumbers {
    id: i32,
    name: Option<String>,
    email: String,
    ringcentral_phone_number: Option<String>,
    cloudtalk_phone_number: Option<String>,
    phone_number: Option<String>,
}

/// Users of the company with at least one phone number, but `except`, by
/// name: every direct line, then their RingCentral, CloudTalk and own
/// numbers.
pub async fn coworkers(
    pool: &MySqlPool,
    company_id: i32,
    except: i32,
) -> sqlx::Result<Vec<Coworker>> {
    let users: Vec<UserNumbers> = sqlx::query_as(
        "SELECT u.id, u.name, u.email, u.ringcentral_phone_number,
                    u.cloudtalk_phone_number, u.phone_number
               FROM users u
              WHERE u.company_id = ? AND u.is_deleted = 0 AND u.id <> ?
              ORDER BY u.id",
    )
    .bind(company_id)
    .bind(except)
    .fetch_all(pool)
    .await?;
    let lines: Vec<(i32, String, String)> = sqlx::query_as(
        "SELECT nu.user_id, n.e164, n.label
           FROM telnyx_number_users nu
           JOIN telnyx_numbers n ON n.id = nu.number_id AND n.kind = 'direct' AND n.active <> 0
          WHERE n.company_id = ?
          ORDER BY nu.user_id, n.id",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    let mut direct: HashMap<i32, Vec<CoworkerPhone>> = HashMap::new();
    for (user_id, e164, label) in lines {
        direct.entry(user_id).or_default().push(CoworkerPhone {
            e164,
            label: non_empty(Some(label)),
        });
    }
    let mut coworkers: Vec<Coworker> = Vec::new();
    for user in users {
        let mut found = direct.remove(&user.id).unwrap_or_default();
        let others = [
            (user.ringcentral_phone_number, "RingCentral"),
            (user.cloudtalk_phone_number, "CloudTalk"),
            (user.phone_number, "Mobile"),
        ];
        for (number, label) in others {
            if let Some(e164) = number.as_deref().and_then(to_e164) {
                found.push(CoworkerPhone {
                    e164,
                    label: Some(label.to_string()),
                });
            }
        }
        let phones = distinct_phones(found);
        let Some(first) = phones.first() else {
            continue;
        };
        coworkers.push(Coworker {
            user_id: user.id,
            name: non_empty(user.name).unwrap_or(user.email),
            e164: Some(first.e164.clone()),
            label: first.label.clone(),
            phones,
        });
    }
    coworkers.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then(a.user_id.cmp(&b.user_id))
    });
    Ok(coworkers)
}

/// The numbers in order, each once (by its last 10 digits).
fn distinct_phones(phones: Vec<CoworkerPhone>) -> Vec<CoworkerPhone> {
    let mut seen: Vec<String> = Vec::new();
    phones
        .into_iter()
        .filter(|phone| {
            let key = last10(&phone.e164).unwrap_or_else(|| phone.e164.clone());
            if seen.contains(&key) {
                return false;
            }
            seen.push(key);
            true
        })
        .collect()
}

/// Where a since-sync starts: changes from `since`, and customers with ids
/// above `max_id` (added since).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub since: DateTime<Utc>,
    pub max_id: i32,
}

impl Cursor {
    /// A cursor for an index read now whose highest customer id is `max_id`.
    pub fn now(now: DateTime<Utc>, max_id: i32) -> Self {
        Self {
            since: now - Duration::minutes(CURSOR_MARGIN_MINUTES),
            max_id,
        }
    }

    /// As the app holds it (opaque): `{unix seconds}.{max id}`.
    pub fn encode(self) -> String {
        format!("{}.{}", self.since.timestamp(), self.max_id)
    }

    /// A cursor the app sent; `None` when malformed, from the future or
    /// older than 30 days (the app then gets a full download).
    pub fn parse(text: &str, now: DateTime<Utc>) -> Option<Self> {
        let (seconds, max_id) = text.trim().split_once('.')?;
        let since = DateTime::from_timestamp(seconds.parse().ok()?, 0)?;
        let max_id = max_id.parse().ok()?;
        let fresh = since <= now + Duration::minutes(5)
            && since >= now - Duration::days(CURSOR_MAX_AGE_DAYS);
        fresh.then_some(Self { since, max_id })
    }
}

/// What the index depends on now, for the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexVersion {
    /// Contacts in the index.
    pub count: i64,
    /// Its highest customer id.
    pub max_id: i32,
    /// `"<hash>"`: changes whenever a customer is added, edited or deleted,
    /// a deal opens, moves or closes, a customer is called or a coworker's
    /// line changes.
    pub etag: String,
}

/// The company's highest live customer id (0 for none): the cursor of a
/// later page of a full download, without the cost of [`version`].
pub async fn max_id(pool: &MySqlPool, company_id: i32) -> sqlx::Result<i32> {
    let max: i64 = sqlx::query_scalar(
        "SELECT CAST(COALESCE(MAX(id), 0) AS SIGNED) FROM customers
          WHERE company_id = ? AND deleted_at IS NULL AND id <> 990000021",
    )
    .bind(company_id)
    .fetch_one(pool)
    .await?;
    Ok(i32::try_from(max).unwrap_or(i32::MAX))
}

/// The index's current version for user `me` (with these coworkers).
pub async fn version(
    pool: &MySqlPool,
    company_id: i32,
    me: i32,
    coworkers: &[Coworker],
) -> sqlx::Result<IndexVersion> {
    let (count, max_id, changed_at, deals, deals_hash, last_call): (i64, i64, i64, i64, i64, i64) =
        sqlx::query_as(
            "SELECT CAST(COALESCE(cs.n, 0) AS SIGNED), CAST(COALESCE(cs.m, 0) AS SIGNED),
                    CAST(COALESCE(cs.t, 0) AS SIGNED), CAST(ds.n AS SIGNED),
                    CAST(COALESCE(ds.h, 0) AS SIGNED), CAST(COALESCE(cl.m, 0) AS SIGNED)
               FROM (SELECT SUM(deleted_at IS NULL AND id <> 990000021) AS n,
                            MAX(CASE WHEN deleted_at IS NULL AND id <> 990000021 THEN id END) AS m,
                            UNIX_TIMESTAMP(MAX(updated_at)) AS t
                       FROM customers WHERE company_id = ?) cs,
                    (SELECT COUNT(*) AS n,
                            BIT_XOR(CRC32(CONCAT_WS(':', d.id, d.list_id, d.customer_id))) AS h
                       FROM deals d JOIN customers c ON c.id = d.customer_id
                      WHERE c.company_id = ? AND d.deleted_at IS NULL AND d.is_won IS NULL) ds,
                    (SELECT MAX(id) AS m FROM phone_calls
                      WHERE company_id = ? AND customer_id IS NOT NULL) cl",
        )
        .bind(company_id)
        .bind(company_id)
        .bind(company_id)
        .fetch_one(pool)
        .await?;
    let mut fingerprint =
        format!("v1|{me}|{count}|{max_id}|{changed_at}|{deals}|{deals_hash}|{last_call}");
    for coworker in coworkers {
        let _ = write!(fingerprint, "|{}:{}", coworker.user_id, coworker.name);
        for phone in &coworker.phones {
            let _ = write!(
                fingerprint,
                ":{}={}",
                phone.e164,
                phone.label.as_deref().unwrap_or_default()
            );
        }
    }
    Ok(IndexVersion {
        count,
        max_id: i32::try_from(max_id).unwrap_or(i32::MAX),
        etag: format!("\"{}\"", &sha256_hex(&fingerprint)[..32]),
    })
}

/// A customer's (email, rep's name, address, city, state, postal code).
type CardDetailsRow = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// The contact card's extra details: (primary email, sales rep's name,
/// address on one line).
pub type CardDetails = (Option<String>, Option<String>, Option<String>);

/// The contact card's extra details.
///
/// `None` when the customer is not a live customer of the company.
pub async fn card_details(
    pool: &MySqlPool,
    company_id: i32,
    id: i32,
) -> sqlx::Result<Option<CardDetails>> {
    let row: Option<CardDetailsRow> = sqlx::query_as(
        "SELECT COALESCE(pe.email, (SELECT e.email FROM customers_emails e
                                     WHERE e.customer_id = c.id ORDER BY e.created_at, e.id LIMIT 1)),
                u.name, c.address, c.city, c.state, c.postal_code
           FROM customers c
           LEFT JOIN customers_emails pe ON pe.id = c.email_id AND pe.customer_id = c.id
           LEFT JOIN users u ON u.id = c.sales_rep AND u.is_deleted = 0
          WHERE c.id = ? AND c.company_id = ? AND c.deleted_at IS NULL AND c.id <> 990000021",
    )
    .bind(id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(email, rep, address, city, state, postal_code)| {
        let region = [non_empty(state), non_empty(postal_code)]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
        let line = [non_empty(address), non_empty(city), non_empty(Some(region))]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(", ");
        (
            non_empty(email),
            non_empty(rep),
            (!line.is_empty()).then_some(line),
        )
    }))
}

/// A deal on the contact card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CardDeal {
    /// The deal.
    pub id: i64,
    /// Its stage.
    pub stage: String,
    /// `open`, `won` or `lost`.
    pub status: DealStatus,
    /// Its amount, as a decimal string (`"1500.25"`); null when not set.
    pub amount: Option<String>,
    /// Its newest note; null when it has none.
    pub last_note: Option<CardNote>,
}

wire_enum! {
    /// A deal's outcome: `open`, `won` or `lost`.
    pub enum DealStatus {
        /// Not decided yet.
        Open = "open",
        /// Sold.
        Won = "won",
        /// Lost.
        Lost = "lost",
    }
}

/// A deal's newest note.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CardNote {
    /// The note.
    pub content: String,
    /// When it was written.
    #[serde(serialize_with = "crate::serde_helpers::js_date_required")]
    pub created_at: DateTime<Utc>,
    /// Who wrote it (a name); null when unknown.
    pub created_by: Option<String>,
}

#[derive(FromRow)]
struct DealRow {
    id: i64,
    stage: Option<String>,
    status: String,
    amount: Option<String>,
    note_content: Option<String>,
    note_created_at: Option<DateTime<Utc>>,
    note_created_by: Option<String>,
}

/// The customer's live deals (open ones first, then newest), at most 20.
pub async fn card_deals(pool: &MySqlPool, customer_id: i32) -> sqlx::Result<Vec<CardDeal>> {
    let rows: Vec<DealRow> = sqlx::query_as(
        "SELECT CAST(d.id AS SIGNED) AS id, COALESCE(dl.name, d.status) AS stage,
                CASE WHEN d.is_won IS NULL THEN 'open' WHEN d.is_won = 1 THEN 'won' ELSE 'lost' END
                  AS status,
                CAST(d.amount AS CHAR) AS amount, n.content AS note_content,
                n.created_at AS note_created_at, n.created_by AS note_created_by
           FROM deals d
           LEFT JOIN deals_list dl ON dl.id = d.list_id
           LEFT JOIN deal_notes n ON n.id = (SELECT n2.id FROM deal_notes n2
                                              WHERE n2.deal_id = d.id AND n2.deleted_at IS NULL
                                              ORDER BY n2.created_at DESC, n2.id DESC LIMIT 1)
          WHERE d.customer_id = ? AND d.deleted_at IS NULL
          ORDER BY d.is_won IS NOT NULL, d.id DESC
          LIMIT 20",
    )
    .bind(customer_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| CardDeal {
            id: row.id,
            stage: row.stage.unwrap_or_default(),
            status: DealStatus::parse(&row.status).unwrap_or(DealStatus::Open),
            amount: row.amount,
            last_note: match (row.note_content, row.note_created_at) {
                (Some(content), Some(created_at)) => Some(CardNote {
                    content,
                    created_at,
                    created_by: non_empty(row.note_created_by),
                }),
                _ => None,
            },
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phones_are_last_ten_digits_once_each() {
        assert_eq!(
            index_phones(Some("317-555-0100"), Some("+1 (317) 555-0101")),
            ["3175550100", "3175550101"]
        );
        assert_eq!(
            index_phones(Some("317-555-0100"), Some("3175550100")),
            ["3175550100"]
        );
        assert!(index_phones(Some(""), Some("555-0100")).is_empty());
        assert_eq!(index_phones(None, Some("3175550102")), ["3175550102"]);
    }

    #[test]
    fn phones_are_prefiltered_by_their_last_four_digits() {
        assert_eq!(last_digits_pattern("3175550142"), "%0%1%4%2%");
        // Every way the CRM's free-text phones write the number matches.
        let matches = |phone: &str, pattern: &str| {
            let parts: Vec<&str> = pattern.trim_matches('%').split('%').collect();
            let mut rest = phone;
            parts.iter().all(|part| match rest.find(part) {
                Some(at) => {
                    rest = &rest[at + part.len()..];
                    true
                }
                None => false,
            })
        };
        for phone in [
            "317-555-0142",
            "(317) 555 01 42",
            "+1 317.555.0142 ",
            "3175550142 ext",
        ] {
            assert!(
                matches(phone, &last_digits_pattern("3175550142")),
                "{phone}"
            );
        }
        assert!(!matches("317-555-0124", &last_digits_pattern("3175550142")));
    }

    #[test]
    fn cursors_round_trip_and_expire() {
        let now = DateTime::from_timestamp(1_760_000_000, 0).unwrap();
        let cursor = Cursor::now(now, 1234);
        assert_eq!(cursor.since, now - Duration::minutes(15));
        assert_eq!(cursor.encode(), "1759999100.1234");
        assert_eq!(Cursor::parse(&cursor.encode(), now), Some(cursor));
        assert_eq!(Cursor::parse("nonsense", now), None);
        assert_eq!(Cursor::parse("1759999100", now), None);
        assert_eq!(Cursor::parse("1759999100.x", now), None);
        // 31 days old, or from the future: a full download.
        assert_eq!(Cursor::parse("1757321600.1", now), None);
        assert_eq!(Cursor::parse("1760001000.1", now), None);
    }
}
