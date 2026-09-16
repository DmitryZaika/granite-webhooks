//! Insert-time derivation of the SMS thread columns.
//!
//! The CRM used to recompute phone normalization, thread keys and "echo"
//! detection over a company's whole SMS history on every poll, which is what
//! overloaded the production database. Every writer now stores those values on
//! the row itself, so the read path only touches indexed stored columns.
//!
//! The rules live in `SMS_DERIVED_CONTRACT.md` and are implemented identically
//! by the TypeScript side (`app/utils/smsDerivedFields.server.ts`). Summary:
//!
//! * `agent` is stored normalized (trimmed, empty becomes `NULL`), so
//!   "untagged" is simply `agent IS NULL` — there is no `agent_blank` column.
//! * `sender10` / `recipient10` are the last ten digits of the two phones.
//! * `phone_digits` is the thread key: the customer's phone. For an inbound row
//!   that is the sender, unless the sender is one of the company's own lines
//!   (tracked in `sms_company_phones`), in which case it is the recipient.
//! * An untagged inbound row with non-empty text that repeats a live outbound
//!   row's text within ±300 s is an "echo" of the provider reflecting our own
//!   message back at us; it stores `is_echo = 1` and points at that row.

use std::collections::HashSet;
use std::hash::BuildHasher;

use chrono::{DateTime, Utc};
use sqlx::MySqlPool;
use sqlx::mysql::MySqlQueryResult;

/// Half-width of the echo window, in seconds. Must match
/// `SMS_ECHO_WINDOW_SECONDS` in the CRM.
pub const SMS_ECHO_WINDOW_SECONDS: i64 = 300;

/// The SMS provider whose table a row belongs to. Every table name in this
/// module is interpolated from this enum only; all values are bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SmsProvider {
    /// `CloudTalk` (`cloudtalk_sms`).
    Cloudtalk,
    /// `RingCentral` (`ringcentral_sms`).
    Ringcentral,
}

impl SmsProvider {
    /// Name of the provider's SMS table.
    #[must_use]
    pub const fn table(self) -> &'static str {
        match self {
            Self::Cloudtalk => "cloudtalk_sms",
            Self::Ringcentral => "ringcentral_sms",
        }
    }

    /// Name of the column holding the provider's own message id.
    #[must_use]
    pub const fn id_column(self) -> &'static str {
        match self {
            Self::Cloudtalk => "cloudtalk_id",
            Self::Ringcentral => "ringcentral_id",
        }
    }

    /// Value stored in the `sms_company_phones.provider` ENUM.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cloudtalk => "cloudtalk",
            Self::Ringcentral => "ringcentral",
        }
    }
}

/// Which way the message travelled, as stored in the `direction` ENUM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SmsDirection {
    /// A message from the customer to the company.
    Inbound,
    /// A message from the company to the customer.
    Outbound,
}

impl SmsDirection {
    /// Value stored in the `direction` ENUM.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Inbound => "inbound",
            Self::Outbound => "outbound",
        }
    }
}

/// Delivery state, as stored in the `status` ENUM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SmsStatus {
    /// An inbound message we received.
    Received,
    /// An outbound message the provider accepted.
    Sent,
    /// An outbound message still awaiting the provider's confirmation.
    Pending,
    /// An outbound message the provider rejected.
    Failed,
}

impl SmsStatus {
    /// Value stored in the `status` ENUM.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Received => "received",
            Self::Sent => "sent",
            Self::Pending => "pending",
            Self::Failed => "failed",
        }
    }

    /// Whether an outbound row in this state can still be echoed back to us.
    /// A failed send never produces an echo.
    #[must_use]
    pub const fn is_live_outbound(self) -> bool {
        matches!(self, Self::Sent | Self::Pending)
    }
}

/// Everything a single SMS row is inserted with, before derivation.
#[derive(Clone, Copy, Debug)]
pub struct SmsRowInput<'a> {
    /// Owning company (multi-tenant scope for every query here).
    pub company_id: i32,
    /// The provider's own message id, `NULL` for CRM-originated rows.
    pub provider_id: Option<i64>,
    /// Sending phone; `NULL` on CRM-originated outbound rows.
    pub sender: Option<u64>,
    /// Receiving phone.
    pub recipient: u64,
    /// Message body; may be empty (attachment-only messages).
    pub text: &'a str,
    /// Direction of the message.
    pub direction: SmsDirection,
    /// Delivery state of the message.
    pub status: SmsStatus,
    /// Raw agent id from the provider, normalized before it is stored.
    pub agent: Option<&'a str>,
    /// The `created_date` that will be stored; all echo-window math is done
    /// against this exact value in SQL.
    pub created_date: DateTime<Utc>,
}

/// The values derived for a row about to be inserted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SmsDerived {
    /// Normalized agent id (trimmed; `None` when blank).
    pub agent: Option<String>,
    /// Last ten digits of the sender.
    pub sender10: Option<String>,
    /// Last ten digits of the recipient (empty only if the recipient has no
    /// digits at all, which the schema makes impossible).
    pub recipient10: String,
    /// Thread key: the customer's phone for this row.
    pub phone_digits: String,
    /// Whether this row is a provider echo of one of our own outbound rows.
    pub is_echo: bool,
    /// The outbound row this one echoes, when [`SmsDerived::is_echo`].
    pub echo_of_sms_id: Option<i64>,
}

/// Digits of `value`, keeping the last ten when longer. `None` when `value`
/// holds no digits at all.
#[must_use]
pub fn last10_digits(value: &str) -> Option<String> {
    let digits: String = value.chars().filter(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    match digits.char_indices().nth_back(9) {
        Some((start, _)) => Some(digits[start..].to_string()),
        None => Some(digits),
    }
}

/// `NULLIF(TRIM(agent), '')`: what actually gets stored in the `agent` column.
#[must_use]
pub fn normalize_agent(agent: Option<&str>) -> Option<String> {
    agent
        .map(str::trim)
        .filter(|trimmed| !trimmed.is_empty())
        .map(str::to_string)
}

/// The thread key for a row: the customer's phone.
///
/// Outbound rows thread under the recipient. Inbound rows thread under the
/// sender, unless that sender is one of the company's own lines — an agent
/// texting from the shared number — in which case the customer is the
/// recipient.
#[must_use]
pub fn thread_phone_for_row<S: BuildHasher>(
    direction: SmsDirection,
    sender10: Option<&str>,
    recipient10: &str,
    company_senders: &HashSet<String, S>,
) -> String {
    if direction == SmsDirection::Outbound {
        return recipient10.to_string();
    }
    match sender10 {
        Some(sender) if !company_senders.contains(sender) => sender.to_string(),
        _ => recipient10.to_string(),
    }
}

/// Columns written after the provider id column. The insert statement and its
/// bind list are both built from this one place so they cannot drift apart
/// (see `insert_statement_binds_every_column`).
const INSERT_COLUMNS_AFTER_ID: [&str; 13] = [
    "sender",
    "recipient",
    "text",
    "agent",
    "company_id",
    "direction",
    "status",
    "created_date",
    "sender10",
    "recipient10",
    "phone_digits",
    "is_echo",
    "echo_of_sms_id",
];

/// Number of values [`insert_sms`] binds: the provider id plus every column in
/// [`INSERT_COLUMNS_AFTER_ID`].
const INSERT_BIND_COUNT: usize = 1 + INSERT_COLUMNS_AFTER_ID.len();

/// `?, ?, …` of the requested length.
fn placeholders(count: usize) -> String {
    vec!["?"; count].join(", ")
}

/// The `INSERT IGNORE` used by [`insert_sms`], with one placeholder per column.
fn insert_statement(provider: SmsProvider) -> String {
    let mut columns = String::from(provider.id_column());
    for column in INSERT_COLUMNS_AFTER_ID {
        columns.push_str(", ");
        columns.push_str(column);
    }
    format!(
        "INSERT IGNORE INTO {} ({columns}) VALUES ({})",
        provider.table(),
        placeholders(INSERT_BIND_COUNT),
    )
}

/// The distinct non-empty phones a row can be linked by, newest evidence first.
fn linkable_phones<'a>(sender10: Option<&'a str>, recipient10: &'a str) -> Vec<&'a str> {
    let mut phones: Vec<&str> = Vec::with_capacity(2);
    let recipient = if recipient10.is_empty() {
        None
    } else {
        Some(recipient10)
    };
    for phone in [sender10, recipient].into_iter().flatten() {
        if !phones.contains(&phone) {
            phones.push(phone);
        }
    }
    phones
}

/// Contract step 3: record what this row proves about the company's own lines,
/// then repair the threads of a line that just became a company line.
///
/// A phone is a company line once it has BOTH sent an agent-tagged message and
/// received an untagged inbound one. The upsert is monotone (`GREATEST`), and
/// MySQL reports how it went: `rows_affected` is 1 for a new row (one flag),
/// 2 when an existing row changed (the other flag was already set, so the line
/// just flipped) and 0 when nothing changed. Only the flip runs the repair, so
/// two concurrent webhooks are safe — the upsert is atomic on the server, and
/// whichever applies second is the one that sees the change — and an
/// established line costs nothing extra per message.
async fn apply_role_evidence(
    pool: &MySqlPool,
    provider: SmsProvider,
    company_id: i32,
    agent: Option<&str>,
    sender10: Option<&str>,
    recipient10: &str,
    direction: SmsDirection,
) -> Result<(), sqlx::Error> {
    // The two kinds of evidence are mutually exclusive: one needs a tagged
    // agent, the other an untagged one.
    let evidence = if let Some(phone) = sender10.filter(|_| agent.is_some()) {
        Some((phone, 1_i8, 0_i8))
    } else if direction == SmsDirection::Inbound && agent.is_none() && !recipient10.is_empty() {
        Some((recipient10, 0_i8, 1_i8))
    } else {
        None
    };
    let Some((phone, agent_sender, inbound_recipient)) = evidence else {
        return Ok(());
    };

    let upsert = sqlx::query(
        "INSERT INTO sms_company_phones \
           (provider, company_id, phone10, agent_sender, inbound_recipient) \
         VALUES (?, ?, ?, ?, ?) AS new \
         ON DUPLICATE KEY UPDATE \
           agent_sender = \
             GREATEST(sms_company_phones.agent_sender, new.agent_sender), \
           inbound_recipient = \
             GREATEST(sms_company_phones.inbound_recipient, new.inbound_recipient)",
    )
    .bind(provider.as_str())
    .bind(company_id)
    .bind(phone)
    .bind(agent_sender)
    .bind(inbound_recipient)
    .execute(pool)
    .await?;

    if upsert.rows_affected() == UPSERT_CHANGED_EXISTING_ROW {
        repair_company_line_threads(pool, provider, company_id, phone).await?;
    }
    Ok(())
}

/// `rows_affected` of `INSERT ... ON DUPLICATE KEY UPDATE` when an existing
/// row was changed (1 = inserted, 0 = existing row left as it was).
const UPSERT_CHANGED_EXISTING_ROW: u64 = 2;

/// Re-key the inbound history of a company line: rows it sent belong to the
/// thread of whoever received them, not to the line itself. Idempotent. The
/// index is forced because the optimizer has been seen preferring the unique
/// `(company_id, <provider>_id)` index for this UPDATE and reading the whole
/// company.
async fn repair_company_line_threads(
    pool: &MySqlPool,
    provider: SmsProvider,
    company_id: i32,
    phone10: &str,
) -> Result<u64, sqlx::Error> {
    let sql = format!(
        "UPDATE {} FORCE INDEX (idx_sms_sender10) SET phone_digits = recipient10 \
         WHERE company_id = ? AND direction = 'inbound' \
           AND sender10 = ? AND phone_digits <> recipient10",
        provider.table(),
    );
    let result = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(company_id)
        .bind(phone10)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

/// Contract step 4: the company's own lines (both role flags set).
async fn company_sender_phones(
    pool: &MySqlPool,
    provider: SmsProvider,
    company_id: i32,
) -> Result<HashSet<String>, sqlx::Error> {
    let phones = sqlx::query_scalar::<_, String>(
        "SELECT phone10 FROM sms_company_phones \
         WHERE provider = ? AND company_id = ? \
           AND agent_sender = 1 AND inbound_recipient = 1",
    )
    .bind(provider.as_str())
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    Ok(phones.into_iter().collect())
}

/// Contract step 6: the live outbound row this inbound row echoes, if any.
///
/// The window is expressed as `created_date BETWEEN ? AND ?` so the index on
/// `(company_id, direction, status, created_date)` is usable; the closest match
/// in time wins, ties broken by id.
async fn find_echo_outbound_id(
    pool: &MySqlPool,
    provider: SmsProvider,
    input: &SmsRowInput<'_>,
    sender10: Option<&str>,
    recipient10: &str,
) -> Result<Option<i64>, sqlx::Error> {
    if input.text.is_empty() {
        return Ok(None);
    }
    let phones = linkable_phones(sender10, recipient10);
    if phones.is_empty() {
        return Ok(None);
    }
    let list = placeholders(phones.len());
    let sql = format!(
        "SELECT o.id FROM {} o \
          WHERE o.company_id = ? \
            AND o.direction = 'outbound' \
            AND o.status IN ('sent', 'pending') \
            AND o.created_date BETWEEN (? - INTERVAL {SMS_ECHO_WINDOW_SECONDS} SECOND) \
                                   AND (? + INTERVAL {SMS_ECHO_WINDOW_SECONDS} SECOND) \
            AND o.text = ? \
            AND (o.recipient10 IN ({list}) OR o.sender10 IN ({list})) \
          ORDER BY ABS(TIMESTAMPDIFF(SECOND, o.created_date, ?)) ASC, o.id ASC \
          LIMIT 1",
        provider.table(),
    );
    let mut query = sqlx::query_scalar::<_, i32>(sqlx::AssertSqlSafe(sql))
        .bind(input.company_id)
        .bind(input.created_date)
        .bind(input.created_date)
        .bind(input.text);
    for phone in phones.iter().chain(phones.iter()) {
        query = query.bind(*phone);
    }
    let found = query
        .bind(input.created_date)
        .fetch_optional(pool)
        .await?
        .map(i64::from);
    Ok(found)
}

/// Contract step 8: an outbound row was just stored, so any untagged inbound
/// row that already repeated its text inside the window is its echo. Covers
/// out-of-order webhook delivery, where the echo lands before the send.
async fn mark_inbound_echoes_of_outbound(
    pool: &MySqlPool,
    provider: SmsProvider,
    input: &SmsRowInput<'_>,
    derived: &SmsDerived,
    outbound_id: i64,
) -> Result<u64, sqlx::Error> {
    if input.text.is_empty() {
        return Ok(0);
    }
    let phones = linkable_phones(derived.sender10.as_deref(), &derived.recipient10);
    if phones.is_empty() {
        return Ok(0);
    }
    let list = placeholders(phones.len());
    let sql = format!(
        "UPDATE {} inb \
            SET inb.is_echo = 1, inb.echo_of_sms_id = ? \
          WHERE inb.company_id = ? \
            AND inb.direction = 'inbound' \
            AND inb.agent IS NULL \
            AND inb.is_echo = 0 \
            AND inb.text = ? \
            AND inb.created_date BETWEEN (? - INTERVAL {SMS_ECHO_WINDOW_SECONDS} SECOND) \
                                     AND (? + INTERVAL {SMS_ECHO_WINDOW_SECONDS} SECOND) \
            AND (inb.sender10 IN ({list}) OR inb.recipient10 IN ({list}))",
        provider.table(),
    );
    let mut query = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(outbound_id)
        .bind(input.company_id)
        .bind(input.text)
        .bind(input.created_date)
        .bind(input.created_date);
    for phone in phones.iter().chain(phones.iter()) {
        query = query.bind(*phone);
    }
    Ok(query.execute(pool).await?.rows_affected())
}

/// Compute every derived value for a row about to be inserted (contract steps
/// 1–6), maintaining `sms_company_phones` on the way.
///
/// Call [`insert_sms`] instead unless you need the values without inserting.
pub async fn derive(
    pool: &MySqlPool,
    provider: SmsProvider,
    input: &SmsRowInput<'_>,
) -> Result<SmsDerived, sqlx::Error> {
    let agent = normalize_agent(input.agent);
    let sender10 = input
        .sender
        .map(|sender| sender.to_string())
        .and_then(|sender| last10_digits(&sender));
    let recipient10 = last10_digits(&input.recipient.to_string()).unwrap_or_default();

    apply_role_evidence(
        pool,
        provider,
        input.company_id,
        agent.as_deref(),
        sender10.as_deref(),
        &recipient10,
        input.direction,
    )
    .await?;

    let company_senders = company_sender_phones(pool, provider, input.company_id).await?;
    let phone_digits = thread_phone_for_row(
        input.direction,
        sender10.as_deref(),
        &recipient10,
        &company_senders,
    );

    // Only an untagged inbound row can be a provider echo of our own send.
    let echo_of_sms_id = if input.direction == SmsDirection::Inbound && agent.is_none() {
        find_echo_outbound_id(pool, provider, input, sender10.as_deref(), &recipient10).await?
    } else {
        None
    };

    Ok(SmsDerived {
        agent,
        sender10,
        recipient10,
        phone_digits,
        is_echo: echo_of_sms_id.is_some(),
        echo_of_sms_id,
    })
}

/// Derive and insert one SMS row (contract steps 1–8).
///
/// The insert is `INSERT IGNORE`, so a provider id we already stored is a
/// no-op. After a live outbound row is stored, earlier inbound echoes of it are
/// marked; `rows_affected() == 0` means nothing was inserted and nothing is
/// marked.
pub async fn insert_sms(
    pool: &MySqlPool,
    provider: SmsProvider,
    input: &SmsRowInput<'_>,
) -> Result<MySqlQueryResult, sqlx::Error> {
    let derived = derive(pool, provider, input).await?;
    let result = sqlx::query(sqlx::AssertSqlSafe(insert_statement(provider)))
        .bind(input.provider_id)
        .bind(input.sender)
        .bind(input.recipient)
        .bind(input.text)
        .bind(derived.agent.as_deref())
        .bind(input.company_id)
        .bind(input.direction.as_str())
        .bind(input.status.as_str())
        .bind(input.created_date)
        .bind(derived.sender10.as_deref())
        .bind(derived.recipient10.as_str())
        .bind(derived.phone_digits.as_str())
        .bind(i8::from(derived.is_echo))
        .bind(derived.echo_of_sms_id)
        .execute(pool)
        .await?;

    if input.direction == SmsDirection::Outbound
        && input.status.is_live_outbound()
        && result.rows_affected() > 0
        && let Ok(outbound_id) = i64::try_from(result.last_insert_id())
    {
        mark_inbound_echoes_of_outbound(pool, provider, input, &derived, outbound_id).await?;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::{
        INSERT_BIND_COUNT, SmsDirection, SmsProvider, SmsRowInput, SmsStatus, insert_sms,
        insert_statement, last10_digits, normalize_agent, thread_phone_for_row,
    };
    use chrono::{DateTime, Duration, TimeZone, Utc};
    use sqlx::MySqlPool;
    use std::collections::HashSet;

    const COMPANY: i32 = 42;
    const CUSTOMER: u64 = 3_173_161_456;
    const COMPANY_LINE: u64 = 6_468_956_758;

    #[test]
    fn last10_digits_keeps_the_last_ten() {
        assert_eq!(
            last10_digits("+1 (646) 895-6758").as_deref(),
            Some("6468956758")
        );
        assert_eq!(last10_digits("6468956758").as_deref(), Some("6468956758"));
        assert_eq!(last10_digits("12345").as_deref(), Some("12345"));
        assert_eq!(last10_digits("no digits here"), None);
        assert_eq!(last10_digits(""), None);
    }

    #[test]
    fn normalize_agent_trims_and_nulls_blanks() {
        assert_eq!(normalize_agent(Some("  540273 ")).as_deref(), Some("540273"));
        assert_eq!(normalize_agent(Some("540273")).as_deref(), Some("540273"));
        assert_eq!(normalize_agent(Some("   ")), None);
        assert_eq!(normalize_agent(Some("")), None);
        assert_eq!(normalize_agent(None), None);
    }

    #[test]
    fn thread_phone_prefers_the_customer_side() {
        let mut company_senders = HashSet::new();
        company_senders.insert("6468956758".to_string());

        assert_eq!(
            thread_phone_for_row(
                SmsDirection::Outbound,
                Some("6468956758"),
                "3173161456",
                &company_senders,
            ),
            "3173161456",
        );
        assert_eq!(
            thread_phone_for_row(
                SmsDirection::Inbound,
                Some("3173161456"),
                "6468956758",
                &company_senders,
            ),
            "3173161456",
        );
        // An inbound row sent BY a company line threads under its recipient.
        assert_eq!(
            thread_phone_for_row(
                SmsDirection::Inbound,
                Some("6468956758"),
                "3173161456",
                &company_senders,
            ),
            "3173161456",
        );
        assert_eq!(
            thread_phone_for_row(SmsDirection::Inbound, None, "3173161456", &company_senders),
            "3173161456",
        );
    }

    #[test]
    fn insert_statement_binds_every_column() {
        for provider in [SmsProvider::Cloudtalk, SmsProvider::Ringcentral] {
            let sql = insert_statement(provider);
            assert!(sql.contains(provider.table()));
            assert!(sql.contains(provider.id_column()));
            assert_eq!(
                sql.matches('?').count(),
                INSERT_BIND_COUNT,
                "placeholder count must match the bind list in insert_sms",
            );
        }
    }

    fn at(minute: u32, second: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 15, 12, minute, second)
            .single()
            .expect("valid test timestamp")
    }

    fn row<'a>(
        direction: SmsDirection,
        status: SmsStatus,
        sender: u64,
        recipient: u64,
        text: &'a str,
        agent: Option<&'a str>,
        created_date: DateTime<Utc>,
    ) -> SmsRowInput<'a> {
        SmsRowInput {
            company_id: COMPANY,
            provider_id: None,
            sender: Some(sender),
            recipient,
            text,
            direction,
            status,
            agent,
            created_date,
        }
    }

    #[derive(sqlx::FromRow, Debug)]
    struct StoredRow {
        id: i32,
        agent: Option<String>,
        sender10: Option<String>,
        recipient10: Option<String>,
        phone_digits: Option<String>,
        is_echo: i8,
        echo_of_sms_id: Option<i32>,
    }

    async fn stored_rows(pool: &MySqlPool, provider: SmsProvider) -> Vec<StoredRow> {
        let sql = format!(
            "SELECT id, agent, sender10, recipient10, phone_digits, is_echo, echo_of_sms_id \
             FROM {} WHERE company_id = ? ORDER BY id ASC",
            provider.table(),
        );
        sqlx::query_as::<_, StoredRow>(sqlx::AssertSqlSafe(sql))
            .bind(COMPANY)
            .fetch_all(pool)
            .await
            .expect("stored rows")
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn inbound_row_stores_normalized_phones(pool: MySqlPool) {
        insert_sms(
            &pool,
            SmsProvider::Cloudtalk,
            &row(
                SmsDirection::Inbound,
                SmsStatus::Received,
                CUSTOMER,
                COMPANY_LINE,
                "hello there",
                None,
                at(0, 0),
            ),
        )
        .await
        .expect("inbound insert");

        let rows = stored_rows(&pool, SmsProvider::Cloudtalk).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].sender10.as_deref(), Some("3173161456"));
        assert_eq!(rows[0].recipient10.as_deref(), Some("6468956758"));
        // The company line has not been proven yet, so the sender is the thread.
        assert_eq!(rows[0].phone_digits.as_deref(), Some("3173161456"));
        assert_eq!(rows[0].is_echo, 0);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn agent_is_trimmed_and_blank_agents_become_null(pool: MySqlPool) {
        insert_sms(
            &pool,
            SmsProvider::Ringcentral,
            &row(
                SmsDirection::Outbound,
                SmsStatus::Sent,
                COMPANY_LINE,
                CUSTOMER,
                "padded agent",
                Some("  540273  "),
                at(0, 0),
            ),
        )
        .await
        .expect("outbound insert");
        insert_sms(
            &pool,
            SmsProvider::Ringcentral,
            &row(
                SmsDirection::Inbound,
                SmsStatus::Received,
                CUSTOMER,
                COMPANY_LINE,
                "blank agent",
                Some("   "),
                at(1, 0),
            ),
        )
        .await
        .expect("inbound insert");

        let rows = stored_rows(&pool, SmsProvider::Ringcentral).await;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].agent.as_deref(), Some("540273"));
        assert_eq!(rows[1].agent, None, "a blank agent must be stored as NULL");
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn forward_echo_inside_the_window_is_hidden(pool: MySqlPool) {
        insert_sms(
            &pool,
            SmsProvider::Cloudtalk,
            &row(
                SmsDirection::Outbound,
                SmsStatus::Sent,
                COMPANY_LINE,
                CUSTOMER,
                "your quote is ready",
                Some("540273"),
                at(0, 0),
            ),
        )
        .await
        .expect("outbound insert");
        insert_sms(
            &pool,
            SmsProvider::Cloudtalk,
            &row(
                SmsDirection::Inbound,
                SmsStatus::Received,
                COMPANY_LINE,
                CUSTOMER,
                "your quote is ready",
                None,
                at(1, 0),
            ),
        )
        .await
        .expect("echo insert");

        let rows = stored_rows(&pool, SmsProvider::Cloudtalk).await;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].is_echo, 1, "the reflected copy must be an echo");
        assert_eq!(rows[1].echo_of_sms_id, Some(rows[0].id));
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn forward_echo_outside_the_window_is_kept(pool: MySqlPool) {
        insert_sms(
            &pool,
            SmsProvider::Cloudtalk,
            &row(
                SmsDirection::Outbound,
                SmsStatus::Sent,
                COMPANY_LINE,
                CUSTOMER,
                "same words, much later",
                Some("540273"),
                at(0, 0),
            ),
        )
        .await
        .expect("outbound insert");
        insert_sms(
            &pool,
            SmsProvider::Cloudtalk,
            &row(
                SmsDirection::Inbound,
                SmsStatus::Received,
                COMPANY_LINE,
                CUSTOMER,
                "same words, much later",
                None,
                at(0, 0) + Duration::seconds(super::SMS_ECHO_WINDOW_SECONDS + 1),
            ),
        )
        .await
        .expect("late insert");

        let rows = stored_rows(&pool, SmsProvider::Cloudtalk).await;
        assert_eq!(rows[1].is_echo, 0, "outside ±300s it is a real message");
        assert_eq!(rows[1].echo_of_sms_id, None);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn empty_text_never_echoes(pool: MySqlPool) {
        insert_sms(
            &pool,
            SmsProvider::Cloudtalk,
            &row(
                SmsDirection::Outbound,
                SmsStatus::Sent,
                COMPANY_LINE,
                CUSTOMER,
                "",
                Some("540273"),
                at(0, 0),
            ),
        )
        .await
        .expect("outbound insert");
        insert_sms(
            &pool,
            SmsProvider::Cloudtalk,
            &row(
                SmsDirection::Inbound,
                SmsStatus::Received,
                COMPANY_LINE,
                CUSTOMER,
                "",
                None,
                at(0, 30),
            ),
        )
        .await
        .expect("inbound insert");

        let rows = stored_rows(&pool, SmsProvider::Cloudtalk).await;
        assert_eq!(
            rows[1].is_echo, 0,
            "an image-only inbound must never be hidden"
        );
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn reverse_echo_hides_an_inbound_that_arrived_first(pool: MySqlPool) {
        insert_sms(
            &pool,
            SmsProvider::Ringcentral,
            &row(
                SmsDirection::Inbound,
                SmsStatus::Received,
                COMPANY_LINE,
                CUSTOMER,
                "out of order echo",
                None,
                at(0, 0),
            ),
        )
        .await
        .expect("inbound insert");
        insert_sms(
            &pool,
            SmsProvider::Ringcentral,
            &row(
                SmsDirection::Outbound,
                SmsStatus::Sent,
                COMPANY_LINE,
                CUSTOMER,
                "out of order echo",
                Some("540273"),
                at(1, 0),
            ),
        )
        .await
        .expect("outbound insert");

        let rows = stored_rows(&pool, SmsProvider::Ringcentral).await;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].is_echo, 1, "the earlier inbound must become an echo");
        assert_eq!(rows[0].echo_of_sms_id, Some(rows[1].id));
        assert_eq!(rows[1].is_echo, 0, "outbound rows are never echoes");
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn company_line_flip_rekeys_earlier_inbound_rows(pool: MySqlPool) {
        // 1. The company line texts a customer without an agent tag yet: at this
        //    point nothing proves it is a company line, so it threads under itself.
        insert_sms(
            &pool,
            SmsProvider::Cloudtalk,
            &row(
                SmsDirection::Inbound,
                SmsStatus::Received,
                COMPANY_LINE,
                CUSTOMER,
                "from the shared line",
                None,
                at(0, 0),
            ),
        )
        .await
        .expect("early inbound insert");
        let early = stored_rows(&pool, SmsProvider::Cloudtalk).await;
        assert_eq!(early[0].phone_digits.as_deref(), Some("6468956758"));

        // 2. An agent-tagged send from the line proves `agent_sender`.
        insert_sms(
            &pool,
            SmsProvider::Cloudtalk,
            &row(
                SmsDirection::Outbound,
                SmsStatus::Sent,
                COMPANY_LINE,
                CUSTOMER,
                "tagged send",
                Some("540273"),
                at(1, 0),
            ),
        )
        .await
        .expect("tagged outbound insert");

        // 3. An untagged inbound TO the line proves `inbound_recipient`; both
        //    flags are now set, so the earlier rows must be re-keyed.
        insert_sms(
            &pool,
            SmsProvider::Cloudtalk,
            &row(
                SmsDirection::Inbound,
                SmsStatus::Received,
                CUSTOMER,
                COMPANY_LINE,
                "a customer reply",
                None,
                at(2, 0),
            ),
        )
        .await
        .expect("untagged inbound insert");

        let flags = sqlx::query_as::<_, (i8, i8)>(
            "SELECT agent_sender, inbound_recipient FROM sms_company_phones \
             WHERE provider = 'cloudtalk' AND company_id = ? AND phone10 = '6468956758'",
        )
        .bind(COMPANY)
        .fetch_one(&pool)
        .await
        .expect("company phone row");
        assert_eq!(flags, (1, 1), "both roles must be recorded for the line");

        let rows = stored_rows(&pool, SmsProvider::Cloudtalk).await;
        assert_eq!(
            rows[0].phone_digits.as_deref(),
            Some("3173161456"),
            "the earlier inbound must be re-keyed to its recipient",
        );
        assert_eq!(
            rows[2].phone_digits.as_deref(),
            Some("3173161456"),
            "the customer reply threads under the customer",
        );
    }
}
