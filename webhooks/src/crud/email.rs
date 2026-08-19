use crate::{
    amazonses::parse_email::{ParsedEmail, ParsedRecipient, UploadedAttachment, normalize_address},
    crud::users::ReceivingEmail,
};
use lambda_http::tracing;
use sqlx::{MySqlPool, mysql::MySqlQueryResult};
use std::collections::HashMap;
use uuid::Uuid;

pub async fn get_full_message_id(
    pool: &sqlx::MySqlPool,
    message_id: &str,
) -> Result<Option<String>, sqlx::Error> {
    // Create the search pattern (e.g., "abc" becomes "abc%")
    let pattern = format!("{message_id}%");

    sqlx::query_scalar!(
        r#"
        SELECT message_id
        FROM emails
        WHERE message_id LIKE ?
        LIMIT 1
        "#,
        pattern
    )
    .fetch_optional(pool)
    .await
    .map(std::option::Option::flatten)
}

pub async fn create_email_read(
    pool: &MySqlPool,
    message_id: &str,
    user_agent: &str,
    ip_address: &str,
) -> Result<MySqlQueryResult, sqlx::Error> {
    sqlx::query!(
        r#"
        INSERT INTO email_reads (message_id, user_agent, ip_address)
        VALUES (?, ?, ?)
        "#,
        message_id,
        user_agent,
        ip_address,
    )
    .execute(pool)
    .await
}

pub struct PriorEmail {
    pub thread_id: Option<String>,
    pub receiver_user_id: Option<i32>,
}

pub async fn get_prior_email(
    pool: &MySqlPool,
    message_id: &str,
) -> Result<Option<PriorEmail>, sqlx::Error> {
    sqlx::query_as!(
        PriorEmail,
        r#"
        SELECT thread_id, receiver_user_id FROM emails WHERE message_id = ?
        "#,
        message_id
    )
    .fetch_optional(pool)
    .await
}

/// Match a stored `emails.message_id` that was truncated (VARCHAR(72) era /
/// `scheduled_emails`) or stored as the SES local-part while the reply still
/// carries the full `@us-east-2.amazonses.com` id.
pub async fn get_prior_email_by_message_id_prefix(
    pool: &MySqlPool,
    message_id: &str,
) -> Result<Option<PriorEmail>, sqlx::Error> {
    if message_id.len() < 32 {
        return Ok(None);
    }
    sqlx::query_as!(
        PriorEmail,
        r#"
        SELECT thread_id, receiver_user_id
        FROM emails
        WHERE message_id IS NOT NULL
          AND CHAR_LENGTH(message_id) >= 32
          AND (
            message_id LIKE CONCAT(?, '%')
            OR ? LIKE CONCAT(message_id, '%')
          )
        ORDER BY sent_at DESC
        LIMIT 1
        "#,
        message_id,
        message_id
    )
    .fetch_optional(pool)
    .await
}

#[allow(dead_code)]
struct PriorEmailContextRow {
    thread_id: Option<String>,
    receiver_user_id: Option<i32>,
    subject: Option<String>,
    sender_email: Option<String>,
    receiver_email: Option<String>,
}

fn normalize_thread_subject(raw: &str) -> String {
    let mut subject = raw.trim().to_lowercase();
    loop {
        let stripped = subject
            .strip_prefix("re:")
            .or_else(|| subject.strip_prefix("fwd:"))
            .or_else(|| subject.strip_prefix("fw:"));
        match stripped {
            Some(rest) => subject = rest.trim_start().to_string(),
            None => break,
        }
    }
    subject.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Last-resort threading when `In-Reply-To` / `References` do not match a
/// stored Message-ID (CRM backfill used a UUID, or the drip row is missing
/// the SES id). Same people + same subject, recent outbound only.
pub async fn get_prior_email_by_reply_context(
    pool: &MySqlPool,
    inbound_sender: &str,
    inbound_receiver: &str,
    subject: Option<&str>,
) -> Result<Option<PriorEmail>, sqlx::Error> {
    let want_subject = subject
        .map(normalize_thread_subject)
        .filter(|value| !value.is_empty());
    let Some(want_subject) = want_subject else {
        return Ok(None);
    };
    let customer = normalize_address(inbound_sender);
    let employee = normalize_address(inbound_receiver);
    if customer.is_empty() || employee.is_empty() {
        return Ok(None);
    }

    let rows = sqlx::query_as!(
        PriorEmailContextRow,
        r#"
        SELECT thread_id, receiver_user_id, subject, sender_email, receiver_email
        FROM emails
        WHERE sender_user_id IS NOT NULL
          AND deleted_at IS NULL
          AND sent_at >= DATE_SUB(UTC_TIMESTAMP(), INTERVAL 45 DAY)
          AND (
            LOWER(receiver_email) = ?
            OR LOWER(receiver_email) LIKE CONCAT('%<', ?, '>%')
          )
        ORDER BY sent_at DESC
        LIMIT 40
        "#,
        customer,
        customer
    )
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().find_map(|row| {
        let stored_subject = normalize_thread_subject(row.subject.as_deref().unwrap_or(""));
        if stored_subject != want_subject {
            return None;
        }
        let stored_sender = normalize_address(row.sender_email.as_deref().unwrap_or(""));
        if stored_sender != employee {
            return None;
        }
        Some(PriorEmail {
            thread_id: row.thread_id,
            receiver_user_id: row.receiver_user_id,
        })
    }))
}

pub async fn insert_email_attachment(
    pool: &MySqlPool,
    email_id: u64,
    attachment: &UploadedAttachment,
) -> Result<MySqlQueryResult, sqlx::Error> {
    sqlx::query!(
        r#"
        INSERT INTO email_attachments (
            email_id,
            content_type,
            content_subtype,
            filename,
            url
        )
        VALUES (?, ?, ?, ?, ?)
        "#,
        email_id,
        attachment.content_type,
        attachment.content_subtype,
        attachment.filename,
        attachment.url,
    )
    .execute(pool)
    .await
}

pub struct SendEmail {
    subject: Option<String>,
    body: String,
    html_body: Option<String>,
    thread_id: String,
    receiver_user_id: Option<i32>,
    sender_email: String,
    pub receiver_email: Option<String>,
    message_id: String,
    /// Owning company, resolved from the receiver user. `None` when the
    /// receiver could not be attributed — such rows stay legacy-visible.
    pub company_id: Option<i32>,
    to_recipients: Vec<ParsedRecipient>,
    cc_recipients: Vec<ParsedRecipient>,
    bcc_recipients: Vec<ParsedRecipient>,
}

impl SendEmail {
    pub fn new(
        email: &ParsedEmail,
        thread_id: Option<String>,
        receiver_id: Option<ReceivingEmail>,
    ) -> Self {
        let final_thread_id = thread_id.unwrap_or_else(|| Uuid::new_v4().to_string());
        let receiver_email = match receiver_id {
            Some(ReceivingEmail::To(_)) => Some(email.receiver_email.clone()),
            Some(ReceivingEmail::Forward(_)) => email.forward_to_email.clone(),
            None => None,
        };
        let receiver_user_id = receiver_id.map(super::users::ReceivingEmail::inner);
        Self {
            subject: email.subject.clone(),
            body: email.body.clone(),
            html_body: email.html_body.clone(),
            thread_id: final_thread_id,
            receiver_user_id,
            sender_email: email.sender_email.clone(),
            receiver_email,
            message_id: email.message_id.clone(),
            company_id: None,
            to_recipients: email.to_recipients.clone(),
            cc_recipients: email.cc_recipients.clone(),
            bcc_recipients: email.bcc_recipients.clone(),
        }
    }

    #[must_use]
    pub const fn with_company_id(mut self, company_id: Option<i32>) -> Self {
        self.company_id = company_id;
        self
    }

    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    pub fn receiver_user_id(&self) -> Option<i32> {
        self.receiver_user_id
    }

    pub fn subject(&self) -> Option<&str> {
        self.subject.as_deref()
    }

    pub fn sender_email(&self) -> &str {
        &self.sender_email
    }
}

pub struct InboundEmailNotifyContext {
    pub deal_id: Option<u64>,
    pub customer_name: Option<String>,
    pub sender_email: Option<String>,
}

pub fn resolve_inbound_customer_name(
    customer_name: Option<String>,
    sender_email: Option<&str>,
) -> Option<String> {
    if let Some(name) = customer_name {
        let trimmed = name.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }

    let Some(sender) = sender_email
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return None;
    };

    if let Some(lt_idx) = sender.find('<') {
        let display = sender[..lt_idx].trim().trim_matches('"');
        if !display.is_empty() {
            return Some(display.to_string());
        }
        let address = sender[lt_idx + 1..].trim_end_matches('>').trim();
        let normalized = normalize_email_address(address);
        if !normalized.is_empty() {
            return Some(normalized);
        }
    }

    let normalized = normalize_email_address(sender);
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

fn normalize_email_address(email: &str) -> String {
    email.trim().trim_matches('"').to_lowercase()
}

pub async fn get_inbound_email_notify_context(
    pool: &MySqlPool,
    thread_id: &str,
    receiver_user_id: i32,
) -> Result<Option<InboundEmailNotifyContext>, sqlx::Error> {
    sqlx::query_as!(
        InboundEmailNotifyContext,
        r#"
        SELECT
            COALESCE(e.deal_id, td.deal_id) AS deal_id,
            COALESCE(
                c.name,
                (
                    SELECT c2.name
                    FROM customers c2
                    INNER JOIN customers_emails ce ON ce.customer_id = c2.id
                    INNER JOIN users u ON u.id = ?
                    WHERE c2.company_id = u.company_id
                      AND c2.deleted_at IS NULL
                      AND LOWER(TRIM(SUBSTRING_INDEX(SUBSTRING_INDEX(ce.email, '<', -1), '>', 1))) =
                          LOWER(TRIM(SUBSTRING_INDEX(SUBSTRING_INDEX(e.sender_email, '<', -1), '>', 1)))
                    LIMIT 1
                )
            ) AS customer_name,
            e.sender_email AS sender_email
        FROM emails e
        LEFT JOIN (
            SELECT thread_id, MAX(deal_id) AS deal_id
            FROM emails
            WHERE deleted_at IS NULL AND thread_id IS NOT NULL AND deal_id IS NOT NULL
            GROUP BY thread_id
        ) td ON td.thread_id = e.thread_id
        LEFT JOIN deals d ON d.id = COALESCE(e.deal_id, td.deal_id) AND d.deleted_at IS NULL
        LEFT JOIN customers c ON c.id = d.customer_id
        WHERE e.deleted_at IS NULL
          AND e.thread_id = ?
        ORDER BY e.sent_at DESC, e.id DESC
        LIMIT 1
        "#,
        receiver_user_id,
        thread_id
    )
    .fetch_optional(pool)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_inbound_customer_name_prefers_database_name() {
        assert_eq!(
            resolve_inbound_customer_name(Some("Chew Customer".to_string()), Some("x@y.com")),
            Some("Chew Customer".to_string())
        );
    }

    #[test]
    fn resolve_inbound_customer_name_uses_sender_display_name() {
        assert_eq!(
            resolve_inbound_customer_name(None, Some("Chew <chew@example.com>")),
            Some("Chew".to_string())
        );
    }

    #[test]
    fn resolve_inbound_customer_name_uses_sender_email_address() {
        assert_eq!(
            resolve_inbound_customer_name(None, Some("chew@example.com")),
            Some("chew@example.com".to_string())
        );
    }

    #[test]
    fn normalize_thread_subject_strips_reply_prefixes() {
        assert_eq!(
            normalize_thread_subject("Re: Thank You for Your Request"),
            "thank you for your request"
        );
        assert_eq!(
            normalize_thread_subject("RE: Fwd:  Thank You for Your Request"),
            "thank you for your request"
        );
    }
}

/// Resolve many addresses to CRM users in one round trip, so the query count
/// does not grow with the recipient count.
async fn resolve_user_ids(
    pool: &MySqlPool,
    addresses: &[String],
) -> Result<HashMap<String, i32>, sqlx::Error> {
    if addresses.is_empty() {
        return Ok(HashMap::new());
    }
    let mut builder = sqlx::QueryBuilder::<sqlx::MySql>::new(
        "SELECT LOWER(TRIM(email)) AS email, id FROM users WHERE is_deleted = 0 \
         AND LOWER(TRIM(email)) IN (",
    );
    let mut separated = builder.separated(", ");
    for address in addresses {
        separated.push_bind(address);
    }
    separated.push_unseparated(")");

    let rows: Vec<(String, i32)> = builder.build_query_as().fetch_all(pool).await?;
    Ok(rows.into_iter().collect())
}

async fn resolve_customer_ids(
    pool: &MySqlPool,
    addresses: &[String],
    company_id: Option<i32>,
) -> Result<HashMap<String, i32>, sqlx::Error> {
    if addresses.is_empty() {
        return Ok(HashMap::new());
    }
    let mut builder = sqlx::QueryBuilder::<sqlx::MySql>::new(
        "SELECT LOWER(TRIM(SUBSTRING_INDEX(SUBSTRING_INDEX(ce.email, '<', -1), '>', 1))) AS email, \
         c.id FROM customers c JOIN customers_emails ce ON ce.customer_id = c.id \
         WHERE c.deleted_at IS NULL AND \
         LOWER(TRIM(SUBSTRING_INDEX(SUBSTRING_INDEX(ce.email, '<', -1), '>', 1))) IN (",
    );
    let mut separated = builder.separated(", ");
    for address in addresses {
        separated.push_bind(address);
    }
    separated.push_unseparated(")");
    if let Some(company_id) = company_id {
        builder.push(" AND c.company_id = ");
        builder.push_bind(company_id);
    }

    let rows: Vec<(String, i32)> = builder.build_query_as().fetch_all(pool).await?;
    Ok(rows.into_iter().collect())
}

/// Store every participant of an inbound message.
///
/// The sender is written as `from`, plus every `To:`, `Cc:` and `Bcc:` address.
/// Before this table existed only the first `To:` survived, which is why
/// reply-all was impossible.
pub async fn insert_email_participants(
    pool: &MySqlPool,
    email_id: u64,
    send: &SendEmail,
) -> Result<(), sqlx::Error> {
    let sender = ParsedRecipient {
        address: normalize_address(&send.sender_email),
        display_name: None,
    };
    let from_recipients = if sender.address.is_empty() {
        Vec::new()
    } else {
        vec![sender]
    };

    let groups: [(&str, &Vec<ParsedRecipient>); 4] = [
        ("from", &from_recipients),
        ("to", &send.to_recipients),
        ("cc", &send.cc_recipients),
        ("bcc", &send.bcc_recipients),
    ];

    let all_addresses: Vec<String> = groups
        .iter()
        .flat_map(|(_, list)| list.iter().map(|r| r.address.clone()))
        .collect();
    let user_ids = resolve_user_ids(pool, &all_addresses).await?;
    let customer_ids = resolve_customer_ids(pool, &all_addresses, send.company_id).await?;

    for (participant_type, recipients) in groups {
        for (index, recipient) in recipients.iter().enumerate() {
            let position = i32::try_from(index).unwrap_or(0);
            sqlx::query(
                "INSERT INTO email_participants \
                 (email_id, type, email, display_name, user_id, customer_id, position) \
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(email_id)
            .bind(participant_type)
            .bind(&recipient.address)
            .bind(recipient.display_name.as_deref())
            .bind(user_ids.get(&recipient.address))
            .bind(customer_ids.get(&recipient.address))
            .bind(position)
            .execute(pool)
            .await?;
        }
    }
    Ok(())
}

pub async fn create_email_with_attachments(
    pool: &MySqlPool,
    send: &SendEmail,
    location: &str,
    attachments: &[UploadedAttachment],
) -> Result<MySqlQueryResult, sqlx::Error> {
    let result = sqlx::query!(
        r#"
        INSERT INTO emails (subject, body, thread_id, receiver_user_id, sender_email, receiver_email, message_id, bucket, company_id)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
        send.subject,
        send.body,
        send.thread_id,
        send.receiver_user_id,
        send.sender_email,
        send.receiver_email,
        send.message_id,
        location,
        send.company_id
    )
    .execute(pool)
    .await?;

    let email_id = result.last_insert_id();

    // Recipient rows are best-effort: a failure here must not lose the message
    // itself, which is already committed above.
    if let Err(error) = insert_email_participants(pool, email_id, send).await {
        tracing::warn!(?error, email_id, "Failed to store email participants");
    }

    if let Some(html_body) = send.html_body.as_deref().filter(|value| !value.is_empty()) {
        if let Err(error) = sqlx::query("UPDATE emails SET html_body = ? WHERE id = ?")
            .bind(html_body)
            .bind(email_id)
            .execute(pool)
            .await
        {
            tracing::warn!(?error, email_id, "Failed to store email html_body");
        }
    }

    for attachment in attachments {
        insert_email_attachment(pool, email_id, attachment).await?;
    }
    Ok(result)
}
