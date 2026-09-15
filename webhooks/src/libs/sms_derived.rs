//! SMS derived-field computation shared by the CloudTalk and RingCentral
//! webhook inserts. Mirrors `app/utils/smsDerivedFields.server.ts` in the
//! CRM: every SMS row stores its normalized phones, thread key, and echo
//! status at insert time so the read path never needs whole-history scans.
//!
//! Echo semantics (must match the TS side exactly):
//! - An untagged inbound row is an echo when a sent/pending outbound row
//!   with the same non-empty text exists within ±300s with any of the five
//!   phone linkages.
//! - Empty text never echoes (protects image-only inbound messages).

use sqlx::{MySqlPool, Row};

/// Must match `SMS_ECHO_WINDOW_SECONDS` in the CRM.
pub const SMS_ECHO_WINDOW_SECONDS: i64 = 300;

/// Last-10-digits normalization. Mirrors `canonicalPhone10` (TS): strip
/// non-digits, keep the last 10 when longer.
pub fn last10_digits(value: &str) -> Option<String> {
    let digits: String = value.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    if digits.len() > 10 {
        Some(digits[digits.len() - 10..].to_string())
    } else {
        Some(digits)
    }
}

pub fn trim_agent(agent: Option<&str>) -> String {
    agent.unwrap_or("").trim().to_string()
}

fn sms_table(provider: &str) -> &'static str {
    match provider {
        "cloudtalk" => "cloudtalk_sms",
        "ringcentral" => "ringcentral_sms",
        _ => panic!("unknown SMS provider: {provider}"),
    }
}

/// Derived fields computed for a row about to be inserted.
pub struct SmsDerivedFields {
    pub sender10: Option<String>,
    pub recipient10: String,
    pub phone_digits: String,
    pub agent_blank: bool,
    pub is_echo: bool,
    pub echo_of_sms_id: Option<i64>,
}

/// Mirror of the TS `recordSmsPhoneRoles`: a phone is a company line when it
/// has BOTH sent an agent-tagged message AND received an untagged inbound.
/// Returns the phone10 that flipped to company-sender because of this row.
pub async fn record_sms_phone_roles(
    pool: &MySqlPool,
    provider: &str,
    company_id: i32,
    sender10: Option<&str>,
    recipient10: Option<&str>,
    direction: &str,
    agent_blank: bool,
) -> Result<Option<String>, sqlx::Error> {
    let agent_sender_evidence = match sender10 {
        Some(s) if !agent_blank => Some(s),
        _ => None,
    };
    let inbound_recipient_evidence =
        match (direction, agent_blank, recipient10) {
            ("inbound", true, Some(r)) => Some(r),
            _ => None,
        };
    if agent_sender_evidence.is_none() && inbound_recipient_evidence.is_none() {
        return Ok(None);
    }

    if let Some(phone) = agent_sender_evidence {
        let existing: Option<(i8, i8)> = sqlx::query(
            "SELECT agent_sender, inbound_recipient FROM sms_company_phones \
             WHERE provider = ? AND company_id = ? AND phone10 = ?",
        )
        .bind(provider)
        .bind(company_id)
        .bind(phone)
        .fetch_optional(pool)
        .await?
        .map(|row| (row.get(0), row.get(1)));
        let was_sender = existing.map(|(s, _)| s == 1).unwrap_or(false);
        let was_recipient = existing.map(|(_, r)| r == 1).unwrap_or(false);
        sqlx::query(
            "INSERT INTO sms_company_phones \
               (provider, company_id, phone10, agent_sender, inbound_recipient) \
             VALUES (?, ?, ?, 1, 0) \
             ON DUPLICATE KEY UPDATE agent_sender = 1",
        )
        .bind(provider)
        .bind(company_id)
        .bind(phone)
        .execute(pool)
        .await?;
        if !was_sender && was_recipient {
            return Ok(Some(phone.to_string()));
        }
    }

    if let Some(phone) = inbound_recipient_evidence {
        let existing: Option<(i8, i8)> = sqlx::query(
            "SELECT agent_sender, inbound_recipient FROM sms_company_phones \
             WHERE provider = ? AND company_id = ? AND phone10 = ?",
        )
        .bind(provider)
        .bind(company_id)
        .bind(phone)
        .fetch_optional(pool)
        .await?
        .map(|row| (row.get(0), row.get(1)));
        let was_sender = existing.map(|(s, _)| s == 1).unwrap_or(false);
        let was_recipient = existing.map(|(_, r)| r == 1).unwrap_or(false);
        sqlx::query(
            "INSERT INTO sms_company_phones \
               (provider, company_id, phone10, agent_sender, inbound_recipient) \
             VALUES (?, ?, ?, 0, 1) \
             ON DUPLICATE KEY UPDATE inbound_recipient = 1",
        )
        .bind(provider)
        .bind(company_id)
        .bind(phone)
        .execute(pool)
        .await?;
        if !was_recipient && was_sender {
            return Ok(Some(phone.to_string()));
        }
    }
    Ok(None)
}

/// Phones currently known to be company lines (both role flags set).
pub async fn load_company_sender_phones(
    pool: &MySqlPool,
    provider: &str,
    company_id: i32,
) -> Result<std::collections::HashSet<String>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT phone10 FROM sms_company_phones \
         WHERE provider = ? AND company_id = ? AND agent_sender = 1 AND inbound_recipient = 1",
    )
    .bind(provider)
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|row| row.get::<String, _>(0)).collect())
}

/// Mirror of the TS `threadPhoneForRow`.
pub fn thread_phone_for_row(
    direction: &str,
    sender10: Option<&str>,
    recipient10: &str,
    company_senders: &std::collections::HashSet<String>,
) -> String {
    if direction == "outbound" {
        return recipient10.to_string();
    }
    if let Some(s) = sender10 {
        if company_senders.contains(s) {
            return recipient10.to_string();
        }
        return s.to_string();
    }
    recipient10.to_string()
}

/// A phone just flipped to company-sender: historical inbound rows sent from
/// it thread under the recipient now.
pub async fn repair_thread_phone_on_sender_flip(
    pool: &MySqlPool,
    provider: &str,
    company_id: i32,
    phone10: &str,
) -> Result<u64, sqlx::Error> {
    let table = sms_table(provider);
    let result = sqlx::query(&format!(
        "UPDATE {table} SET phone_digits = recipient10 \
         WHERE company_id = ? AND direction = 'inbound' \
           AND sender10 = ? AND phone_digits != recipient10"
    ))
    .bind(company_id)
    .bind(phone10)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Forward echo check: does a sent/pending outbound row already exist that
/// this inbound echoes? Mirrors the TS `findEchoOutboundId`.
pub async fn find_echo_outbound_id(
    pool: &MySqlPool,
    provider: &str,
    company_id: i32,
    text: &str,
    sender10: Option<&str>,
    recipient10: &str,
    phone_digits: &str,
    created_date: &chrono::DateTime<chrono::Utc>,
) -> Result<Option<i64>, sqlx::Error> {
    if text.is_empty() {
        return Ok(None);
    }
    let table = sms_table(provider);
    let row = sqlx::query(&format!(
        "SELECT o.id FROM {table} o \
         WHERE o.company_id = ? \
           AND o.direction = 'outbound' \
           AND o.status IN ('pending', 'sent') \
           AND o.text = ? \
           AND ? != '' \
           AND ABS(TIMESTAMPDIFF(SECOND, o.created_date, ?)) <= {SMS_ECHO_WINDOW_SECONDS} \
           AND ( \
             o.phone_digits = ? \
             OR o.recipient10 = ? \
             OR o.recipient10 = ? \
             OR o.sender10 = ? \
             OR o.sender10 = ? \
           ) \
         LIMIT 1"
    ))
    .bind(company_id)
    .bind(text)
    .bind(text)
    .bind(created_date)
    .bind(phone_digits)
    .bind(sender10)
    .bind(recipient10)
    .bind(sender10)
    .bind(recipient10)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| r.get::<i64, _>(0)))
}

/// Reverse echo check: an outbound row was just stored; hide any earlier
/// inbound rows that echo it (covers out-of-order webhook delivery).
/// Mirrors the TS `markInboundEchoesOfOutbound`.
pub async fn mark_inbound_echoes_of_outbound(
    pool: &MySqlPool,
    provider: &str,
    company_id: i32,
    outbound_id: i64,
    text: &str,
    sender10: Option<&str>,
    recipient10: &str,
    phone_digits: &str,
    created_date: &chrono::DateTime<chrono::Utc>,
) -> Result<u64, sqlx::Error> {
    if text.is_empty() {
        return Ok(0);
    }
    let table = sms_table(provider);
    let result = sqlx::query(&format!(
        "UPDATE {table} inb \
            SET inb.is_echo = 1, inb.echo_of_sms_id = ? \
          WHERE inb.company_id = ? \
            AND inb.direction = 'inbound' \
            AND inb.agent_blank = 1 \
            AND inb.is_echo = 0 \
            AND inb.text = ? \
            AND ABS(TIMESTAMPDIFF(SECOND, inb.created_date, ?)) <= {SMS_ECHO_WINDOW_SECONDS} \
            AND ( \
              ? = inb.phone_digits \
              OR ? = inb.sender10 \
              OR ? = inb.recipient10 \
              OR ? = inb.sender10 \
              OR ? = inb.recipient10 \
            )"
    ))
    .bind(outbound_id)
    .bind(company_id)
    .bind(text)
    .bind(created_date)
    .bind(phone_digits)
    .bind(recipient10)
    .bind(recipient10)
    .bind(sender10)
    .bind(sender10)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Compute every derived field for a row about to be inserted, maintaining
/// `sms_company_phones` on the way. Call BEFORE the INSERT and store the
/// returned fields on the row. Mirrors the TS `computeSmsDerivedFields`.
pub async fn compute_sms_derived_fields(
    pool: &MySqlPool,
    provider: &str,
    company_id: i32,
    sender: Option<&str>,
    recipient: &str,
    text: &str,
    direction: &str,
    agent: Option<&str>,
    created_date: &chrono::DateTime<chrono::Utc>,
) -> Result<SmsDerivedFields, sqlx::Error> {
    let sender10 = sender.and_then(last10_digits);
    let recipient10 = last10_digits(recipient).unwrap_or_default();
    let agent_blank = trim_agent(agent).is_empty();

    let flipped = record_sms_phone_roles(
        pool,
        provider,
        company_id,
        sender10.as_deref(),
        if recipient10.is_empty() {
            None
        } else {
            Some(&recipient10)
        },
        direction,
        agent_blank,
    )
    .await?;
    if let Some(phone) = flipped {
        repair_thread_phone_on_sender_flip(pool, provider, company_id, &phone).await?;
    }

    let company_senders = load_company_sender_phones(pool, provider, company_id).await?;
    let phone_digits = thread_phone_for_row(
        direction,
        sender10.as_deref(),
        &recipient10,
        &company_senders,
    );

    let (is_echo, echo_of_sms_id) =
        if direction == "inbound" && agent_blank {
            let echo_id = find_echo_outbound_id(
                pool,
                provider,
                company_id,
                text,
                sender10.as_deref(),
                &recipient10,
                &phone_digits,
                created_date,
            )
            .await?;
            (echo_id.is_some(), echo_id)
        } else {
            (false, None)
        };

    Ok(SmsDerivedFields {
        sender10,
        recipient10,
        phone_digits,
        agent_blank,
        is_echo,
        echo_of_sms_id,
    })
}
