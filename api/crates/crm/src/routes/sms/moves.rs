//! A rep's texts follow them to the providers they text through now:
//! `RingCentral` (an extension) and Telnyx (a Granite Phone line, while the
//! company texts through it). Whenever their line changes (My Account, the
//! Users page, a number given to them, Granite Phone texting turned on)
//! their texts on every other provider are copied there: those they sent,
//! those tagged with an agent id they ever had, and those on a number they
//! ever had (their own: a shared Granite Phone line is not theirs alone).
//! The ids are kept in `user_sms_identities`, so a cleared field still
//! counts. On Telnyx the copies sit on the rep's own line, so the Granite
//! Phone app shows them too. A copy's `idempotency_key` names its original
//! ([`COPY_KEY`](super::provider::COPY_KEY)), which stays where it is: views
//! over every provider skip copies, and an inbox showing both keeps one.

use super::provider::{COPY_KEY, SmsProvider, phone10};
use crate::phone::lines::NumberKind;
use crate::routes::company::queries::company_features;
use sqlx::{MySql, MySqlPool, Transaction};

/// What an id of a user's is on a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The agent id their texts are tagged with (a `RingCentral` extension;
    /// on Telnyx, their user id).
    Agent,
    /// A number of theirs, as its last ten digits.
    Line,
}

impl Kind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Line => "line",
        }
    }
}

/// Keeps `ids` as ids the user has had; empty values and numbers of fewer
/// than ten digits are left out.
pub async fn remember(
    pool: &MySqlPool,
    company_id: i32,
    user_id: i32,
    ids: &[(SmsProvider, Kind, Option<&str>)],
) -> sqlx::Result<()> {
    for (provider, kind, value) in ids {
        let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
            continue;
        };
        let value = match kind {
            Kind::Agent => value.to_string(),
            Kind::Line => phone10(value),
        };
        if *kind == Kind::Line && value.len() != 10 {
            continue;
        }
        sqlx::query(
            "INSERT IGNORE INTO user_sms_identities (company_id, user_id, provider, kind, value)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(company_id)
        .bind(user_id)
        .bind(provider.as_str())
        .bind(kind.as_str())
        .bind(&value)
        .execute(pool)
        .await?;
    }
    Ok(())
}

#[derive(sqlx::FromRow)]
struct Current {
    cloudtalk_agent_id: Option<String>,
    cloudtalk_phone_number: Option<String>,
    ringcentral_extension_id: Option<String>,
    ringcentral_phone_number: Option<String>,
}

/// Copies the user's texts on other providers to the ones they text through
/// now, with pictures and read state; returns the texts copied. Their
/// current ids are remembered first. Copying twice copies nothing new.
pub async fn carry(pool: &MySqlPool, company_id: i32, user_id: i32) -> sqlx::Result<u64> {
    let current: Option<Current> = sqlx::query_as(
        "SELECT cloudtalk_agent_id, cloudtalk_phone_number, ringcentral_extension_id,
                ringcentral_phone_number
           FROM users WHERE id = ? AND company_id = ? AND is_deleted = 0",
    )
    .bind(user_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    let Some(current) = current else {
        return Ok(0);
    };
    let caller_ids = crate::phone::lines::caller_ids(pool, company_id, user_id).await?;
    let user = user_id.to_string();
    let mut ids = vec![
        (
            SmsProvider::Cloudtalk,
            Kind::Agent,
            current.cloudtalk_agent_id.as_deref(),
        ),
        (
            SmsProvider::Cloudtalk,
            Kind::Line,
            current.cloudtalk_phone_number.as_deref(),
        ),
        (
            SmsProvider::Ringcentral,
            Kind::Agent,
            current.ringcentral_extension_id.as_deref(),
        ),
        (
            SmsProvider::Ringcentral,
            Kind::Line,
            current.ringcentral_phone_number.as_deref(),
        ),
    ];
    if !caller_ids.is_empty() {
        ids.push((SmsProvider::Telnyx, Kind::Agent, Some(user.as_str())));
    }
    ids.extend(
        caller_ids
            .iter()
            .filter(|line| line.kind == NumberKind::Direct)
            .map(|line| (SmsProvider::Telnyx, Kind::Line, Some(line.e164.as_str()))),
    );
    remember(pool, company_id, user_id, &ids).await?;

    // The app threads Telnyx texts by line: copies go on the rep's own.
    let line_id = caller_ids
        .iter()
        .find(|line| line.kind == NumberKind::Direct)
        .or_else(|| caller_ids.first())
        .map(|line| line.number_id);
    let mut targets = Vec::new();
    if let Some(extension) = current
        .ringcentral_extension_id
        .as_deref()
        .map(str::trim)
        .filter(|extension| !extension.is_empty())
    {
        targets.push((SmsProvider::Ringcentral, extension.to_string(), None));
    }
    if line_id.is_some() && company_features(pool, company_id).await?.telnyx_sms {
        targets.push((SmsProvider::Telnyx, user.clone(), line_id));
    }
    let mut copied = 0;
    for (target, agent, line_id) in targets {
        let mut tx = pool.begin().await?;
        for source in SmsProvider::ALL {
            if source != target {
                copied += copy_texts(
                    &mut tx, company_id, user_id, source, target, &agent, line_id,
                )
                .await?;
            }
        }
        if let Some(line_id) = line_id {
            move_to_line(&mut tx, company_id, user_id, line_id).await?;
        }
        copy_files_and_reads(&mut tx, company_id, target).await?;
        tx.commit().await?;
    }
    Ok(copied)
}

/// The rep's Telnyx copies on a line no longer theirs move to `line_id`.
async fn move_to_line(
    tx: &mut Transaction<'_, MySql>,
    company_id: i32,
    user_id: i32,
    line_id: i32,
) -> sqlx::Result<()> {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE telnyx_sms SET number_id = ?
          WHERE company_id = ? AND agent = ? AND idempotency_key REGEXP '{COPY_KEY}'
            AND (number_id IS NULL OR number_id NOT IN (
                  SELECT nu.number_id FROM telnyx_number_users nu
                   WHERE nu.user_id = ? AND nu.can_call_from <> 0))"
    )))
    .bind(line_id)
    .bind(company_id)
    .bind(user_id.to_string())
    .bind(user_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// The user's texts on `source` (sent by them, tagged with an agent id of
/// theirs, or on a number of theirs) not yet on `target`, copied there and
/// tagged with their `agent` id on `target` (on Telnyx, on their line).
async fn copy_texts(
    tx: &mut Transaction<'_, MySql>,
    company_id: i32,
    user_id: i32,
    source: SmsProvider,
    target: SmsProvider,
    agent: &str,
    line_id: Option<i32>,
) -> sqlx::Result<u64> {
    let (table, from) = (target.sms_table(), source.sms_table());
    let name = source.as_str();
    // A copy keeps naming the first original. Oldest first: the app orders
    // a line's texts by id.
    let original = format!(
        "IF(COALESCE(s.idempotency_key, '') REGEXP '{COPY_KEY}', s.idempotency_key,
            CONCAT('{name}:', s.id))"
    );
    let mine = "SELECT i.value FROM user_sms_identities i
                 WHERE i.company_id = ? AND i.user_id = ?";
    let sql = format!(
        "INSERT INTO {table}
           (sender, recipient, text, direction, status, error_message, idempotency_key, agent,
            {line_column}sender_user_id, company_id, created_date, sender10, recipient10,
            phone_digits, is_echo, echo_of_sms_id, is_automated)
         SELECT s.sender, s.recipient, s.text, s.direction, s.status, s.error_message,
                {original}, ?, {line_value}s.sender_user_id, s.company_id, s.created_date,
                s.sender10, s.recipient10, s.phone_digits, 0, NULL, s.is_automated
           FROM {from} s
          WHERE s.company_id = ? AND s.is_echo = 0
            AND (s.sender_user_id = ?
                 OR s.agent IN ({mine} AND i.provider = '{name}' AND i.kind = 'agent')
                 OR s.recipient10 IN ({mine} AND i.kind = 'line')
                 OR s.sender10 IN ({mine} AND i.kind = 'line'))
            AND {original} NOT LIKE '{target_name}:%'
            AND {original} NOT IN (
              SELECT r.idempotency_key FROM {table} r
               WHERE r.company_id = ? AND r.idempotency_key REGEXP '{COPY_KEY}')
          ORDER BY s.created_date, s.id",
        target_name = target.as_str(),
        line_column = if line_id.is_some() { "number_id, " } else { "" },
        line_value = if line_id.is_some() { "?, " } else { "" },
    );
    let mut query = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(agent);
    if let Some(line_id) = line_id {
        query = query.bind(line_id);
    }
    query = query.bind(company_id).bind(user_id);
    for _ in 0..3 {
        query = query.bind(company_id).bind(user_id);
    }
    Ok(query
        .bind(company_id)
        .execute(&mut **tx)
        .await?
        .rows_affected())
}

/// The pictures of `target`'s copies, from their originals, and every read
/// state of a copied thread (or each copied inbound text would show as
/// unread again).
async fn copy_files_and_reads(
    tx: &mut Transaction<'_, MySql>,
    company_id: i32,
    target: SmsProvider,
) -> sqlx::Result<()> {
    let table = target.sms_table();
    let (files, file_sms) = target.attachments();
    for source in SmsProvider::ALL {
        if source == target {
            continue;
        }
        let name = source.as_str();
        let (from_files, from_sms) = source.attachments();
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT INTO {files}
               ({file_sms}, content_type, filename, s3_key, s3_url, width, height, position,
                created_date)
             SELECT t.id, a.content_type, a.filename, a.s3_key, a.s3_url, a.width, a.height,
                    a.position, a.created_date
               FROM {table} t
               JOIN {from_files} a
                 ON a.{from_sms} = CAST(SUBSTRING(t.idempotency_key, {start}) AS UNSIGNED)
              WHERE t.company_id = ? AND t.idempotency_key LIKE '{name}:%'
                AND NOT EXISTS (SELECT 1 FROM {files} x WHERE x.{file_sms} = t.id)",
            start = name.len() + 2,
        )))
        .bind(company_id)
        .execute(&mut **tx)
        .await?;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT INTO {reads} (user_id, company_id, customer_phone_digits, last_read_at)
             SELECT cr.user_id, cr.company_id, cr.customer_phone_digits, cr.last_read_at
               FROM {from_reads} cr
              WHERE cr.company_id = ?
                AND cr.customer_phone_digits IN (
                  SELECT DISTINCT t.phone_digits FROM {table} t
                   WHERE t.company_id = ? AND t.idempotency_key LIKE '{name}:%')
             ON DUPLICATE KEY UPDATE
               last_read_at = GREATEST({reads}.last_read_at, VALUES(last_read_at))",
            reads = target.reads_table(),
            from_reads = source.reads_table(),
        )))
        .bind(company_id)
        .bind(company_id)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}
