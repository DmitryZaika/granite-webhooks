//! Customers in every rep's own `RingCentral` contacts.
//!
//! `RingCentral` contacts belong to one login. The company's connection adds
//! customers only to the address book of whoever connected it (webhooks'
//! `ringcentral::api`, `ringcentral_contacts`). So that every rep can find a
//! customer by name in their `RingCentral` app, each rep who connected their
//! own `RingCentral` (`ringcentral_user_logins`) gets every customer in their
//! own book too; `ringcentral_user_contacts` remembers each copy.
//!
//! - [`sync_customer`]: a customer was added or changed: update or add them
//!   in every rep's book.
//! - [`remove_customer`]: a customer was deleted: drop them from every book.
//! - [`catch_up`]: the every-minute job. A rep's first run reads their book
//!   and matches its contacts to customers by phone, so no one gets a second
//!   copy of a contact they already had; later runs add the customers still
//!   missing, newest first, a few a minute (`RingCentral` allows a user about
//!   ten contact writes a minute).
//!
//! A rep's sign-in shares that limit with everything else read as them (the
//! bell's missed calls). Once `RingCentral` answers busy (429), every request
//! in the next minute keeps it busy, so that rep is left alone for
//! [`BUSY_PAUSE_MINUTES`]; customers saved meanwhile are marked `pending`
//! and the job brings their contacts up to date.

use super::ringcentral::{ApiError, Body, request_as};
use regex::Regex;
use reqwest::{Client, Method};
use serde_json::{Map, Value, json};
use sqlx::MySqlPool;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

/// Contacts one job run adds or updates in each rep's book: room under the
/// rep's ten a minute for the bell and customer saves.
const ADDS_PER_RUN: i64 = 6;
/// Minutes a rep is left alone after `RingCentral` answered busy.
const BUSY_PAUSE_MINUTES: i64 = 2;
/// Contacts per page when reading a rep's book, and the most pages read.
const PAGE_SIZE: usize = 1000;
const MAX_PAGES: usize = 30;
/// Longest name part sent. `RingCentral` refuses a contact whose name is too
/// long (it took 42 characters and refused 66); names that long are notes
/// typed into the name field.
const MAX_NAME_CHARS: usize = 40;

/// An email `RingCentral` takes. It refuses the whole contact over a
/// malformed one (a phone number, a stray `>`, letters typed in Cyrillic).
static EMAIL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}$").unwrap()
});

pub fn phone_digits_only(phone: &str) -> String {
    phone.chars().filter(char::is_ascii_digit).collect()
}

pub fn normalize_to_e164(phone: Option<&str>) -> Option<String> {
    let trimmed = phone?.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with('+') {
        return Some(format!("+{}", phone_digits_only(trimmed)));
    }
    let digits = phone_digits_only(trimmed);
    if digits.len() == 10 {
        return Some(format!("+1{digits}"));
    }
    if digits.len() == 11 && digits.starts_with('1') {
        return Some(format!("+{digits}"));
    }
    None
}

pub fn split_name(name: Option<&str>) -> (String, String) {
    let trimmed = name.unwrap_or("").trim();
    if trimmed.is_empty() {
        return ("Customer".to_string(), String::new());
    }
    let mut parts = trimmed.split_whitespace();
    let first = clip_name(parts.next().unwrap_or("Customer"));
    let last = clip_name(&parts.collect::<Vec<_>>().join(" "));
    (first, last)
}

fn clip_name(part: &str) -> String {
    let clipped: String = part.chars().take(MAX_NAME_CHARS).collect();
    clipped.trim_end().to_string()
}

/// The email as `RingCentral` takes it, or `None` for one it would refuse.
pub fn contact_email(email: Option<&str>) -> Option<String> {
    email
        .map(str::trim)
        .filter(|email| EMAIL_RE.is_match(email))
        .map(str::to_string)
}

/// The contact's note: a link to the customer in Granite Manager.
pub fn contact_notes(customer_id: i32) -> Option<String> {
    let app_url = std::env::var("APP_URL")
        .ok()
        .filter(|url| !url.is_empty())?;
    Some(format!(
        "Granite Manager: {app_url}/employee/customers/info/{customer_id}/info"
    ))
}

#[derive(sqlx::FromRow)]
struct Customer {
    id: i32,
    company_id: i32,
    name: Option<String>,
    phone: Option<String>,
    phone_2: Option<String>,
    email: Option<String>,
}

const CUSTOMER_COLUMNS: &str = "c.id, c.company_id, c.name, c.phone, c.phone_2, ce.email
       FROM customers c LEFT JOIN customers_emails ce ON ce.id = c.email_id";

impl Customer {
    fn phones(&self) -> Vec<String> {
        [&self.phone, &self.phone_2]
            .iter()
            .filter_map(|phone| normalize_to_e164(phone.as_deref()))
            .collect()
    }

    /// The contact, as the company's book has it.
    fn payload(&self) -> Value {
        let (first, last) = split_name(self.name.as_deref());
        let phones = self.phones();
        let mut body = Map::new();
        body.insert("firstName".into(), json!(first));
        body.insert("lastName".into(), json!(last));
        if let Some(email) = contact_email(self.email.as_deref()) {
            body.insert("email".into(), json!(email));
        }
        if let Some(phone) = phones.first() {
            body.insert("mobilePhone".into(), json!(phone));
        }
        if let Some(phone) = phones.get(1) {
            body.insert("homePhone".into(), json!(phone));
        }
        if let Some(notes) = contact_notes(self.id) {
            body.insert("notes".into(), json!(notes));
        }
        Value::Object(body)
    }
}

/// A rep with their own sign-in.
#[derive(sqlx::FromRow, Clone)]
struct Rep {
    user_id: i32,
    company_id: i32,
    extension_id: String,
    contacts_indexed_at: Option<chrono::NaiveDateTime>,
}

/// The reps of `company_id` (every company when `None`) with their own
/// sign-in, less the one who connected the company: its connection already
/// fills their book. Reps paused after a busy answer only with
/// `include_paused`.
async fn reps(
    pool: &MySqlPool,
    company_id: Option<i32>,
    include_paused: bool,
) -> Result<Vec<Rep>, sqlx::Error> {
    sqlx::query_as(
        "SELECT l.user_id, l.company_id, l.extension_id, l.contacts_indexed_at
           FROM ringcentral_user_logins l
           JOIN company co ON co.id = l.company_id
          WHERE (? IS NULL OR l.company_id = ?)
            AND l.user_id <> COALESCE(co.ringcentral_connected_by, 0)
            AND (? OR l.contacts_paused_until IS NULL OR l.contacts_paused_until <= NOW())",
    )
    .bind(company_id)
    .bind(company_id)
    .bind(include_paused)
    .fetch_all(pool)
    .await
}

/// Busy (429), or the sign-in failed or `RingCentral` broke: worth another
/// try later, unlike a contact `RingCentral` refused.
const fn is_transient(error: &ApiError) -> bool {
    matches!(error.status, 401 | 403 | 429) || error.status >= 500
}

/// Leaves the rep alone for [`BUSY_PAUSE_MINUTES`].
async fn pause(pool: &MySqlPool, user_id: i32) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE ringcentral_user_logins
            SET contacts_paused_until = NOW() + INTERVAL ? MINUTE
          WHERE user_id = ?",
    )
    .bind(BUSY_PAUSE_MINUTES)
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// The customer's contact in the rep's book needs adding or updating.
async fn mark_pending(pool: &MySqlPool, user_id: i32, customer_id: i32) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO ringcentral_user_contacts (user_id, customer_id, pending)
         VALUES (?, ?, 1)
         ON DUPLICATE KEY UPDATE pending = 1",
    )
    .bind(user_id)
    .bind(customer_id)
    .execute(pool)
    .await?;
    Ok(())
}

fn book_path(extension: &str) -> String {
    format!("account/~/extension/{extension}/address-book/contact")
}

async fn call(
    pool: &MySqlPool,
    client: &Client,
    rep: &Rep,
    method: Method,
    path: &str,
    body: Option<&Value>,
) -> Result<Value, ApiError> {
    let body = body.map_or(Body::Empty, Body::Json);
    request_as(
        pool,
        client,
        rep.company_id,
        Some(&rep.extension_id),
        method,
        path,
        &body,
        None,
    )
    .await
}

fn contact_id(json: &Value) -> Option<i64> {
    json.get("id")
        .and_then(|id| id.as_i64().or_else(|| id.as_str()?.parse().ok()))
        .filter(|id| *id > 0)
}

fn error_text(error: &ApiError) -> String {
    let text = format!(
        "{} {}",
        error.status,
        error.message.as_deref().unwrap_or_default()
    );
    text.chars().take(500).collect()
}

async fn save_mapping(
    pool: &MySqlPool,
    user_id: i32,
    customer_id: i32,
    result: &Result<i64, ApiError>,
) -> Result<(), sqlx::Error> {
    let (ringcentral_id, error) = match result {
        Ok(id) => (Some(*id), None),
        Err(error) => (None, Some(error_text(error))),
    };
    sqlx::query(
        "INSERT INTO ringcentral_user_contacts
                (user_id, customer_id, ringcentral_id, last_error, pending)
         VALUES (?, ?, ?, ?, 0)
         ON DUPLICATE KEY UPDATE
           ringcentral_id = COALESCE(VALUES(ringcentral_id), ringcentral_id),
           last_error = VALUES(last_error),
           pending = 0",
    )
    .bind(user_id)
    .bind(customer_id)
    .bind(ringcentral_id)
    .bind(error)
    .execute(pool)
    .await?;
    Ok(())
}

async fn create(
    pool: &MySqlPool,
    client: &Client,
    rep: &Rep,
    payload: &Value,
) -> Result<i64, ApiError> {
    let reply = call(
        pool,
        client,
        rep,
        Method::POST,
        &book_path(&rep.extension_id),
        Some(payload),
    )
    .await?;
    contact_id(&reply).ok_or(ApiError {
        status: 502,
        message: Some("RingCentral returned no contact id".into()),
    })
}

/// A contact in the rep's book with one of `phones`.
async fn find_by_phone(
    pool: &MySqlPool,
    client: &Client,
    rep: &Rep,
    phones: &[String],
) -> Result<Option<i64>, ApiError> {
    for phone in phones {
        let path = format!(
            "{}?phoneNumber={}&perPage=10",
            book_path(&rep.extension_id),
            urlencoding::encode(phone)
        );
        let reply = call(pool, client, rep, Method::GET, &path, None).await?;
        let found = reply
            .get("records")
            .and_then(Value::as_array)
            .and_then(|records| records.iter().find_map(contact_id));
        if found.is_some() {
            return Ok(found);
        }
    }
    Ok(None)
}

/// Updates the customer's contact in the rep's book, or adds it. Without a
/// known contact, `look_up` first searches the book by phone (the job skips
/// that: it already matched the book when it read it).
async fn upsert(
    pool: &MySqlPool,
    client: &Client,
    rep: &Rep,
    customer: &Customer,
    look_up: bool,
) -> Result<i64, ApiError> {
    let payload = customer.payload();
    let known: Option<i64> = sqlx::query_scalar(
        "SELECT ringcentral_id FROM ringcentral_user_contacts
          WHERE user_id = ? AND customer_id = ? AND ringcentral_id IS NOT NULL",
    )
    .bind(rep.user_id)
    .bind(customer.id)
    .fetch_optional(pool)
    .await
    .map_err(|error| ApiError {
        status: 500,
        message: Some(error.to_string()),
    })?;
    let existing = match known {
        Some(id) => Some(id),
        None if look_up => find_by_phone(pool, client, rep, &customer.phones()).await?,
        None => None,
    };
    if let Some(id) = existing {
        let path = format!("{}/{id}", book_path(&rep.extension_id));
        match call(pool, client, rep, Method::PUT, &path, Some(&payload)).await {
            Ok(_) => return Ok(id),
            // The rep deleted it in RingCentral: add it again.
            Err(error) if error.status == 404 => {}
            Err(error) => return Err(error),
        }
    }
    create(pool, client, rep, &payload).await
}

async fn load_customer(
    pool: &MySqlPool,
    customer_id: i32,
) -> Result<Option<Customer>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {CUSTOMER_COLUMNS} WHERE c.id = ? AND c.deleted_at IS NULL AND c.company_id IS NOT NULL"
    )))
    .bind(customer_id)
    .fetch_optional(pool)
    .await
}

/// Puts the customer, as they are now, in every rep's own book.
pub async fn sync_customer(
    pool: &MySqlPool,
    client: &Client,
    customer_id: i32,
) -> Result<(), sqlx::Error> {
    let Some(customer) = load_customer(pool, customer_id).await? else {
        return Ok(());
    };
    let ready: HashSet<i32> = reps(pool, Some(customer.company_id), false)
        .await?
        .iter()
        .map(|rep| rep.user_id)
        .collect();
    for rep in reps(pool, Some(customer.company_id), true).await? {
        // Busy a moment ago: a request now would keep them busy.
        if !ready.contains(&rep.user_id) {
            mark_pending(pool, rep.user_id, customer.id).await?;
            continue;
        }
        let result = upsert(pool, client, &rep, &customer, true).await;
        if let Err(error) = &result {
            tracing::warn!(
                customer_id,
                user_id = rep.user_id,
                status = error.status,
                message = ?error.message,
                "adding a customer to a rep's RingCentral contacts failed"
            );
            // The job brings the contact up to date within minutes.
            if is_transient(error) {
                if error.status == 429 {
                    pause(pool, rep.user_id).await?;
                }
                mark_pending(pool, rep.user_id, customer.id).await?;
                continue;
            }
        }
        save_mapping(pool, rep.user_id, customer.id, &result).await?;
    }
    Ok(())
}

/// Drops the customer from every rep's own book.
pub async fn remove_customer(
    pool: &MySqlPool,
    client: &Client,
    customer_id: i32,
) -> Result<(), sqlx::Error> {
    let copies: Vec<(i32, Option<i64>, i32, String)> = sqlx::query_as(
        "SELECT uc.user_id, uc.ringcentral_id, l.company_id, l.extension_id
           FROM ringcentral_user_contacts uc
           JOIN ringcentral_user_logins l ON l.user_id = uc.user_id
          WHERE uc.customer_id = ?",
    )
    .bind(customer_id)
    .fetch_all(pool)
    .await?;
    for (user_id, ringcentral_id, company_id, extension_id) in copies {
        if let Some(id) = ringcentral_id {
            let rep = Rep {
                user_id,
                company_id,
                extension_id,
                contacts_indexed_at: None,
            };
            let path = format!("{}/{id}", book_path(&rep.extension_id));
            if let Err(error) = call(pool, client, &rep, Method::DELETE, &path, None).await
                && error.status != 404
            {
                tracing::warn!(
                    customer_id,
                    user_id,
                    status = error.status,
                    "removing a customer from a rep's RingCentral contacts failed"
                );
                continue;
            }
        }
        sqlx::query("DELETE FROM ringcentral_user_contacts WHERE user_id = ? AND customer_id = ?")
            .bind(user_id)
            .bind(customer_id)
            .execute(pool)
            .await?;
    }
    Ok(())
}

/// The phones on a contact of a `RingCentral` book.
fn record_phones(record: &Value) -> Vec<String> {
    const FIELDS: [&str; 8] = [
        "mobilePhone",
        "homePhone",
        "homePhone2",
        "businessPhone",
        "businessPhone2",
        "callbackPhone",
        "carPhone",
        "otherPhone",
    ];
    FIELDS
        .iter()
        .filter_map(|field| normalize_to_e164(record.get(*field)?.as_str()))
        .collect()
}

/// Reads the rep's book and records which customers it already has, by
/// phone. Returns how many it matched.
async fn index_book(pool: &MySqlPool, client: &Client, rep: &Rep) -> Result<usize, ApiError> {
    let mut contact_by_phone: HashMap<String, i64> = HashMap::new();
    for page in 1..=MAX_PAGES {
        let path = format!(
            "{}?perPage={PAGE_SIZE}&page={page}",
            book_path(&rep.extension_id)
        );
        let reply = call(pool, client, rep, Method::GET, &path, None).await?;
        let records = reply
            .get("records")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for record in &records {
            if let Some(id) = contact_id(record) {
                for phone in record_phones(record) {
                    contact_by_phone.entry(phone).or_insert(id);
                }
            }
        }
        let total_pages = reply
            .pointer("/paging/totalPages")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if records.len() < PAGE_SIZE || page as u64 >= total_pages {
            break;
        }
    }
    let db_error = |error: sqlx::Error| ApiError {
        status: 500,
        message: Some(error.to_string()),
    };
    let customers: Vec<Customer> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {CUSTOMER_COLUMNS}
          WHERE c.company_id = ? AND c.deleted_at IS NULL
            AND NOT EXISTS (SELECT 1 FROM ringcentral_user_contacts uc
                             WHERE uc.user_id = ? AND uc.customer_id = c.id)"
    )))
    .bind(rep.company_id)
    .bind(rep.user_id)
    .fetch_all(pool)
    .await
    .map_err(db_error)?;
    let mut matched = 0;
    for customer in &customers {
        let found = customer
            .phones()
            .iter()
            .find_map(|phone| contact_by_phone.get(phone).copied());
        if let Some(id) = found {
            save_mapping(pool, rep.user_id, customer.id, &Ok(id))
                .await
                .map_err(db_error)?;
            matched += 1;
        }
    }
    sqlx::query("UPDATE ringcentral_user_logins SET contacts_indexed_at = NOW() WHERE user_id = ?")
        .bind(rep.user_id)
        .execute(pool)
        .await
        .map_err(db_error)?;
    Ok(matched)
}

/// One run for one rep: read their book the first time, else bring the
/// `pending` contacts up to date, then add the next customers they lack.
/// Returns how many contacts it added or updated.
async fn catch_up_rep(pool: &MySqlPool, client: &Client, rep: &Rep) -> Result<usize, ApiError> {
    let db_error = |error: sqlx::Error| ApiError {
        status: 500,
        message: Some(error.to_string()),
    };
    if rep.contacts_indexed_at.is_none() {
        let matched = index_book(pool, client, rep).await?;
        tracing::info!(
            user_id = rep.user_id,
            matched,
            "read a rep's RingCentral contacts"
        );
        return Ok(0);
    }
    let pending: Vec<Customer> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {CUSTOMER_COLUMNS}
           JOIN ringcentral_user_contacts uc
             ON uc.customer_id = c.id AND uc.user_id = ? AND uc.pending = 1
          WHERE c.company_id = ? AND c.deleted_at IS NULL
          ORDER BY c.id DESC
          LIMIT ?"
    )))
    .bind(rep.user_id)
    .bind(rep.company_id)
    .bind(ADDS_PER_RUN)
    .fetch_all(pool)
    .await
    .map_err(db_error)?;
    let room = ADDS_PER_RUN - i64::try_from(pending.len()).unwrap_or(ADDS_PER_RUN);
    let missing: Vec<Customer> = if room > 0 {
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {CUSTOMER_COLUMNS}
              WHERE c.company_id = ? AND c.deleted_at IS NULL
                AND NOT EXISTS (SELECT 1 FROM ringcentral_user_contacts uc
                                 WHERE uc.user_id = ? AND uc.customer_id = c.id)
              ORDER BY c.id DESC
              LIMIT ?"
        )))
        .bind(rep.company_id)
        .bind(rep.user_id)
        .bind(room)
        .fetch_all(pool)
        .await
        .map_err(db_error)?
    } else {
        Vec::new()
    };
    let mut done = 0;
    for (customer, is_pending) in pending
        .iter()
        .map(|customer| (customer, true))
        .chain(missing.iter().map(|customer| (customer, false)))
    {
        let result = if is_pending {
            upsert(pool, client, rep, customer, false).await
        } else {
            create(pool, client, rep, &customer.payload()).await
        };
        match &result {
            // Busy, or the sign-in no longer works: try again next run.
            Err(error) if is_transient(error) => {
                return Err(ApiError {
                    status: error.status,
                    message: error.message.clone(),
                });
            }
            Ok(_) => done += 1,
            Err(_) => {}
        }
        save_mapping(pool, rep.user_id, customer.id, &result)
            .await
            .map_err(db_error)?;
    }
    Ok(done)
}

/// The job's step: every rep's book a little fuller. Reps run side by side
/// (each has their own `RingCentral` limits). Returns contacts added.
pub async fn catch_up(pool: &MySqlPool, client: &Client) -> Result<usize, sqlx::Error> {
    let mut runs = tokio::task::JoinSet::new();
    for rep in reps(pool, None, false).await? {
        let (pool, client) = (pool.clone(), client.clone());
        runs.spawn(async move {
            let result = catch_up_rep(&pool, &client, &rep).await;
            (rep.user_id, result)
        });
    }
    let mut added = 0;
    while let Some(joined) = runs.join_next().await {
        match joined {
            Ok((_, Ok(count))) => added += count,
            Ok((user_id, Err(error))) => {
                tracing::warn!(
                    user_id,
                    status = error.status,
                    message = ?error.message,
                    "filling a rep's RingCentral contacts stopped for this run"
                );
                if error.status == 429 {
                    pause(pool, user_id).await?;
                }
            }
            Err(error) => tracing::warn!(%error, "a RingCentral contacts run panicked"),
        }
    }
    Ok(added)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phone_digits_only_strips_formatting() {
        assert_eq!(phone_digits_only("+1 (555) 123-4567"), "15551234567");
    }

    #[test]
    fn normalize_to_e164_accepts_us_numbers() {
        assert_eq!(
            normalize_to_e164(Some("5551234567")),
            Some("+15551234567".to_string())
        );
        assert_eq!(
            normalize_to_e164(Some("15551234567")),
            Some("+15551234567".to_string())
        );
        assert_eq!(
            normalize_to_e164(Some("+15551234567")),
            Some("+15551234567".to_string())
        );
        assert_eq!(normalize_to_e164(Some("123")), None);
        assert_eq!(normalize_to_e164(Some("")), None);
        assert_eq!(normalize_to_e164(None), None);
    }

    #[test]
    fn split_name_keeps_a_usable_first_name() {
        assert_eq!(
            split_name(Some("Pat Stone")),
            ("Pat".to_string(), "Stone".to_string())
        );
        assert_eq!(
            split_name(Some("  ")),
            ("Customer".to_string(), String::new())
        );
        assert_eq!(split_name(None), ("Customer".to_string(), String::new()));
    }

    #[test]
    fn split_name_clips_a_note_typed_into_the_name() {
        let (first, last) = split_name(Some(&format!("Pat {}", "word ".repeat(30))));
        assert_eq!(first, "Pat");
        assert_eq!(last.chars().count(), 39);
        assert!(!last.ends_with(' '));
    }

    #[test]
    fn only_well_formed_emails_are_sent() {
        for good in ["pat@example.com", "pat.stone+quotes@mail.example.co"] {
            assert!(EMAIL_RE.is_match(good), "{good}");
        }
        for bad in [
            "317-555-0100",
            "pat@example.com>",
            "pat@#example.com",
            "pat\u{44b}@example.com",
            "pat@example",
            "pat stone@example.com",
        ] {
            assert!(!EMAIL_RE.is_match(bad), "{bad}");
        }
    }

    #[test]
    fn a_contact_carries_name_phones_and_email() {
        let customer = Customer {
            id: 7,
            company_id: 1,
            name: Some("Garner Austin".into()),
            phone: Some("765-721-0238".into()),
            phone_2: Some("bad".into()),
            email: Some("g@example.com".into()),
        };
        let body = customer.payload();
        assert_eq!(body["firstName"], "Garner");
        assert_eq!(body["lastName"], "Austin");
        assert_eq!(body["mobilePhone"], "+17657210238");
        assert!(body.get("homePhone").is_none());
        assert_eq!(body["email"], "g@example.com");
    }

    #[test]
    fn a_book_contact_matches_on_any_phone() {
        let record = json!({"id": 5, "businessPhone": "(765) 721-0238", "homePhone": ""});
        assert_eq!(record_phones(&record), vec!["+17657210238".to_string()]);
        assert_eq!(contact_id(&record), Some(5));
        assert_eq!(contact_id(&json!({"id": "12"})), Some(12));
    }

    /// Company 77, connected by user 4; reps 4 and 5 signed in; two
    /// customers. Returns rep 5 and the customers' ids, oldest first.
    async fn rep_with_customers(pool: &MySqlPool) -> (Rep, i32, i32) {
        sqlx::query(
            "INSERT INTO company (id, name, ringcentral_connected_by) VALUES (77, 'Rc Co', 4)",
        )
        .execute(pool)
        .await
        .unwrap();
        for (user_id, extension) in [(4, "444"), (5, "555")] {
            sqlx::query(
                "INSERT INTO ringcentral_user_logins
                        (user_id, company_id, extension_id, access_token, refresh_token,
                         access_expires, refresh_expires)
                 VALUES (?, 77, ?, 'a', 'r', 0, 0)",
            )
            .bind(user_id)
            .bind(extension)
            .execute(pool)
            .await
            .unwrap();
        }
        let mut ids = Vec::new();
        for name in ["Old One", "New One"] {
            let id = sqlx::query(
                "INSERT INTO customers (name, phone, company_id, source)
                 VALUES (?, '3175550100', 77, 'leads')",
            )
            .bind(name)
            .execute(pool)
            .await
            .unwrap()
            .last_insert_id();
            ids.push(i32::try_from(id).unwrap());
        }
        let rep = Rep {
            user_id: 5,
            company_id: 77,
            extension_id: "555".into(),
            contacts_indexed_at: None,
        };
        (rep, ids[0], ids[1])
    }

    #[test_db::test]
    async fn the_one_who_connected_the_company_is_skipped(pool: MySqlPool) {
        let (rep, _, _) = rep_with_customers(&pool).await;
        let found = reps(&pool, Some(rep.company_id), false).await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].user_id, 5);
        assert!(found[0].contacts_indexed_at.is_none());
    }

    #[test_db::test]
    async fn a_busy_rep_is_left_alone_and_their_saves_wait(pool: MySqlPool) {
        let (rep, old, _) = rep_with_customers(&pool).await;
        pause(&pool, rep.user_id).await.unwrap();
        assert!(
            reps(&pool, Some(rep.company_id), false)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            reps(&pool, Some(rep.company_id), true).await.unwrap().len(),
            1
        );
        // A customer saved while busy reaches nobody now (no request is
        // made, so this needs no RingCentral) and waits for the job.
        sync_customer(&pool, &crate::http::client(), old).await.unwrap();
        let pending: Vec<i32> = sqlx::query_scalar(
            "SELECT customer_id FROM ringcentral_user_contacts WHERE user_id = ? AND pending = 1",
        )
        .bind(rep.user_id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(pending, vec![old]);
        // Bringing it up to date clears the mark and keeps the contact id.
        save_mapping(&pool, rep.user_id, old, &Ok(9)).await.unwrap();
        mark_pending(&pool, rep.user_id, old).await.unwrap();
        save_mapping(
            &pool,
            rep.user_id,
            old,
            &Err(ApiError {
                status: 400,
                message: None,
            }),
        )
        .await
        .unwrap();
        let row: (Option<i64>, bool) = sqlx::query_as(
            "SELECT ringcentral_id, pending FROM ringcentral_user_contacts WHERE user_id = ? AND customer_id = ?",
        )
        .bind(rep.user_id)
        .bind(old)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row, (Some(9), false));
    }

    #[test]
    fn only_busy_and_broken_answers_are_tried_again() {
        let error = |status| ApiError {
            status,
            message: None,
        };
        for status in [401, 403, 429, 500, 503] {
            assert!(is_transient(&error(status)), "{status}");
        }
        for status in [400, 404, 422] {
            assert!(!is_transient(&error(status)), "{status}");
        }
    }

    #[test_db::test]
    async fn a_refused_contact_keeps_its_id_and_records_why(pool: MySqlPool) {
        let (rep, old, _) = rep_with_customers(&pool).await;
        save_mapping(&pool, rep.user_id, old, &Ok(42))
            .await
            .unwrap();
        let refused = Err(ApiError {
            status: 400,
            message: Some("bad".into()),
        });
        save_mapping(&pool, rep.user_id, old, &refused)
            .await
            .unwrap();
        let row: (Option<i64>, Option<String>) = sqlx::query_as(
            "SELECT ringcentral_id, last_error FROM ringcentral_user_contacts WHERE user_id = ? AND customer_id = ?",
        )
        .bind(rep.user_id)
        .bind(old)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row, (Some(42), Some("400 bad".into())));
    }
}
