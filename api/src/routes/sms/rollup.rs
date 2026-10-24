//! Per-thread aggregates, the one query behind the inbox, its badge and the
//! desktop alerts. Port of `buildSmsThreadRollupSql`.
//!
//! The aggregates read only `idx_sms_thread` (company_id, is_echo,
//! phone_digits, created_date, id, direction, status, sender10, agent) — never
//! `text` unless a search needs it. "Which row is last" compares row keys:
//! `created_date` seconds scaled past the id space plus the id, so one `MAX()`
//! picks the row `ORDER BY created_date DESC, id DESC` would.

use super::provider::{Param, SmsProvider, marks};
use super::scope::Narrowing;
use crate::auth::session::SessionUser;
use crate::routes::customers::fuzzy::{NamedCustomer, find_by_name};
use sqlx::MySqlPool;
use std::collections::BTreeSet;

const KEY_SCALE: &str = "100000000000";
/// Older than any row: "no marker" becomes "no cutoff".
const EPOCH: &str = "TIMESTAMP'1970-01-01 00:00:00'";
/// Mark-unread stores this; opening a thread stores `NOW()`.
pub const MANUAL_UNREAD_AT: &str = "1970-01-01 00:00:01";

fn row_key(alias: &str) -> String {
    format!(
        "(CAST(UNIX_TIMESTAMP({alias}.created_date) AS DECIMAL(20, 0)) * {KEY_SCALE} + {alias}.id)"
    )
}

/// The read marker is the mark-unread sentinel.
pub fn manual_unread(column: &str) -> String {
    format!("({column} IS NOT NULL AND {column} < '1971-01-01 00:00:00')")
}

/// A search over the SMS rows: digits in the thread phone, the text, or the
/// phone of a customer or supplier whose name matches.
pub struct Search {
    parts: Vec<String>,
    params: Vec<Param>,
}

impl Search {
    /// The predicate over `alias`, `None` without a search.
    fn sql(&self, alias: &str) -> Option<String> {
        if self.parts.is_empty() {
            return None;
        }
        let parts: Vec<String> = self
            .parts
            .iter()
            .map(|part| part.replace("{a}", alias))
            .collect();
        Some(format!("({})", parts.join(" OR ")))
    }

    pub const fn is_active(&self) -> bool {
        !self.parts.is_empty()
    }
}

pub async fn search(pool: &MySqlPool, company_id: i32, term: &str) -> Result<Search, sqlx::Error> {
    let term = term.trim();
    let mut search = Search {
        parts: Vec::new(),
        params: Vec::new(),
    };
    if term.is_empty() {
        return Ok(search);
    }
    let digits: String = term.chars().filter(char::is_ascii_digit).collect();
    if !digits.is_empty() {
        search.parts.push("{a}.phone_digits LIKE ?".to_string());
        search.params.push(format!("%{digits}%").into());
    }
    search.parts.push("LOWER({a}.text) LIKE ?".to_string());
    search
        .params
        .push(format!("%{}%", term.to_lowercase()).into());
    if digits.is_empty() && term.chars().count() >= 2 {
        let mut phones = customer_phones_by_name(pool, company_id, term).await?;
        phones.extend(supplier_phones_by_name(pool, company_id, term).await?);
        if !phones.is_empty() {
            search
                .parts
                .push(format!("{{a}}.phone_digits IN ({})", marks(phones.len())));
            search.params.extend(phones.into_iter().map(Param::from));
        }
    }
    Ok(search)
}

fn last10(phone: Option<&str>) -> Option<String> {
    let digits = super::provider::phone10(phone?);
    (digits.len() >= 10).then_some(digits)
}

/// Phones of customers whose name (or company) matches, typos allowed, and of
/// their `CloudTalk` contacts. Port of `findThreadPhoneDigitsByCustomerName`.
async fn customer_phones_by_name(
    pool: &MySqlPool,
    company_id: i32,
    term: &str,
) -> Result<BTreeSet<String>, sqlx::Error> {
    let rows = find_by_name(pool, company_id, term, 100).await?;
    let matching: Vec<&NamedCustomer> = rows.iter().collect();
    let mut phones = BTreeSet::new();
    for customer in &matching {
        phones.extend(last10(customer.phone.as_deref()));
        phones.extend(last10(customer.phone_2.as_deref()));
    }
    if matching.is_empty() {
        return Ok(phones);
    }
    let mut params: Vec<Param> = vec![company_id.into()];
    params.extend(matching.iter().map(|c| Param::from(c.id)));
    let contacts: Vec<(Option<String>, Option<String>)> = super::provider::fetch_all(
        pool,
        format!(
            "SELECT phone_e164_1, phone_e164_2 FROM cloudtalk_contacts
              WHERE company_id = ? AND customer_id IN ({})",
            marks(matching.len())
        ),
        &params,
    )
    .await?;
    for (first, second) in contacts {
        phones.extend(last10(first.as_deref()));
        phones.extend(last10(second.as_deref()));
    }
    Ok(phones)
}

/// A supplier's phone without punctuation, last ten digits.
pub const SUPPLIER_PHONE10: &str = "RIGHT(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(COALESCE(phone, ''), ' ', ''), '(', ''), ')', ''), '-', ''), '+', ''), 10)";

async fn supplier_phones_by_name(
    pool: &MySqlPool,
    company_id: i32,
    term: &str,
) -> Result<BTreeSet<String>, sqlx::Error> {
    let escaped: String = term
        .chars()
        .flat_map(|c| {
            if matches!(c, '\\' | '%' | '_') {
                vec!['\\', c]
            } else {
                vec![c]
            }
        })
        .collect();
    let phones: Vec<Option<String>> = sqlx::query_scalar(
        "SELECT phone FROM suppliers
          WHERE company_id = ? AND supplier_name IS NOT NULL AND TRIM(supplier_name) <> ''
            AND supplier_name LIKE ?
          ORDER BY supplier_name LIMIT 10",
    )
    .bind(company_id)
    .bind(format!("%{escaped}%"))
    .fetch_all(pool)
    .await?;
    Ok(phones
        .iter()
        .filter_map(|phone| {
            let digits = super::provider::phone10(phone.as_deref()?);
            (digits.len() == 10).then_some(digits)
        })
        .collect())
}

/// What the outermost level selects.
#[derive(Clone, Copy)]
pub enum Mode {
    /// A page of the inbox.
    List { limit: i64, offset: i64 },
    /// The number of unread threads.
    Count,
    /// Unread threads whose last row came from the customer.
    Alerts { limit: i64 },
}

/// The rollup statement and its binds.
pub fn rollup_sql(
    provider: SmsProvider,
    narrowing: &Narrowing,
    user: &SessionUser,
    search: &Search,
    mode: Mode,
) -> (String, Vec<Param>) {
    let sms = provider.sms_table();
    let reads = provider.reads_table();
    let key = row_key("s");
    let (row_filter, row_params) = narrowing.row_filter(user.company_id);

    // With a search every "latest / count" aggregate only counts matching rows.
    let hit = search.sql("s");
    let mut agg_params: Vec<Param> = Vec::new();
    let mut gate = |condition: Option<&str>| -> Option<String> {
        let Some(hit) = &hit else {
            return condition.map(str::to_string);
        };
        agg_params.extend(search.params.iter().cloned());
        Some(condition.map_or_else(|| hit.clone(), |c| format!("{hit} AND {c}")))
    };
    let message_count = gate(None).map_or_else(
        || "COUNT(*)".to_string(),
        |gate| format!("SUM(CASE WHEN {gate} THEN 1 ELSE 0 END)"),
    );
    let mut max_when = |expr: &str, condition: Option<&str>| {
        gate(condition).map_or_else(
            || format!("MAX({expr})"),
            |gate| format!("MAX(CASE WHEN {gate} THEN {expr} END)"),
        )
    };
    let last_key = max_when(&key, None);
    let last_at = max_when("s.created_date", None);
    let last_inbound_at = max_when(
        "s.created_date",
        Some("s.direction = 'inbound' AND s.agent IS NULL AND s.sender10 = s.phone_digits"),
    );
    let last_customer_key = max_when(
        &key,
        Some("s.direction = 'inbound' AND s.sender10 = s.phone_digits"),
    );
    let last_non_plain_key = max_when(
        &key,
        Some("(s.direction = 'outbound' OR (s.direction = 'inbound' AND s.agent IS NOT NULL))"),
    );
    let (index_hint, having) = if search.is_active() {
        ("", " HAVING message_count > 0")
    } else {
        (" USE INDEX (idx_sms_thread)", "")
    };
    let agg = format!(
        "SELECT s.phone_digits,
                {message_count} AS message_count,
                {last_key} AS last_key,
                {last_at} AS last_at,
                {last_inbound_at} AS last_inbound_at,
                {last_customer_key} AS last_customer_key,
                {last_non_plain_key} AS last_non_plain_key,
                MAX(CASE
                      WHEN s.direction = 'outbound' AND s.status IN ('sent', 'pending') THEN s.created_date
                      WHEN s.direction = 'inbound' AND s.agent IS NOT NULL THEN s.created_date
                    END) AS last_rep_at
           FROM {sms} s{index_hint}
          WHERE {row_filter}
          GROUP BY s.phone_digits{having}"
    );
    agg_params.extend(row_params);

    // Unread: plain inbound rows newer than both the read marker and our last
    // response — a lookup of one thread, never company-wide.
    let mut params: Vec<Param> = vec![user.company_id.into()];
    let unread_search = search.sql("u").map_or_else(String::new, |sql| {
        params.extend(search.params.iter().cloned());
        format!(" AND {sql}")
    });
    params.extend(agg_params);
    params.push(user.id.into());
    params.push(user.company_id.into());
    let manual = manual_unread("r.last_read_at");
    // A manual mark wins; a latest outbound or agent-tagged row clears the
    // badge; otherwise the unread total applies.
    let threads = format!(
        "SELECT a.phone_digits,
                CAST(a.last_key MOD {KEY_SCALE} AS SIGNED) AS last_id,
                a.last_key, a.last_at, a.last_customer_key,
                CAST(a.message_count AS SIGNED) AS message_count,
                a.last_inbound_at,
                CAST(CASE
                  WHEN {manual} THEN 1
                  WHEN a.last_non_plain_key = a.last_key THEN 0
                  ELSE (SELECT COUNT(*) FROM {sms} u
                         WHERE u.company_id = ? AND u.is_echo = 0
                           AND u.phone_digits = a.phone_digits
                           AND u.direction = 'inbound' AND u.agent IS NULL
                           AND u.sender10 = u.phone_digits
                           AND u.created_date > GREATEST(COALESCE(r.last_read_at, {EPOCH}),
                                                         COALESCE(a.last_rep_at, {EPOCH})){unread_search})
                END AS SIGNED) AS unread
           FROM ({agg}) a
           LEFT JOIN {reads} r
             ON r.user_id = ? AND r.company_id = ? AND r.customer_phone_digits = a.phone_digits"
    );
    outer(&threads, params, mode, narrowing.one_rep)
}

/// The outermost level of the rollup for `mode`.
fn outer(threads: &str, mut params: Vec<Param>, mode: Mode, one_rep: bool) -> (String, Vec<Param>) {
    match mode {
        Mode::Count => (
            format!("SELECT COUNT(*) FROM ({threads}) t WHERE t.unread > 0"),
            params,
        ),
        Mode::Alerts { limit } => {
            params.push(limit.into());
            (
                format!(
                    "SELECT t.phone_digits, t.last_id, t.last_at FROM ({threads}) t
                      WHERE t.unread > 0 AND t.last_customer_key = t.last_key
                      ORDER BY t.last_key DESC LIMIT ?"
                ),
                params,
            )
        }
        Mode::List { limit, offset } => {
            // Unread first only for one rep's inbox; the company-wide one is by
            // the latest inbound.
            let order = if one_rep {
                "(t.unread > 0) DESC, t.last_key DESC"
            } else {
                "COALESCE(t.last_inbound_at, t.last_at) DESC, t.last_id DESC"
            };
            params.push(limit.into());
            params.push(offset.into());
            (
                format!(
                    "SELECT t.phone_digits, t.last_id, t.last_at, t.last_inbound_at, t.message_count, t.unread,
                            COUNT(*) OVER () AS total_count,
                            CAST(SUM(CASE WHEN t.unread > 0 THEN 1 ELSE 0 END) OVER () AS SIGNED) AS total_unread
                       FROM ({threads}) t
                      ORDER BY {order}
                      LIMIT ? OFFSET ?"
                ),
                params,
            )
        }
    }
}
