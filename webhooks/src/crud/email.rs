use crate::{
    amazonses::parse_email::{ParsedEmail, UploadedAttachment},
    crud::users::ReceivingEmail,
};
use lambda_http::tracing;
use sqlx::{MySqlPool, mysql::MySqlQueryResult};
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
        }
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

    let Some(sender) = sender_email.map(str::trim).filter(|value| !value.is_empty()) else {
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
}

pub async fn create_email_with_attachments(
    pool: &MySqlPool,
    send: &SendEmail,
    location: &str,
    attachments: &[UploadedAttachment],
) -> Result<MySqlQueryResult, sqlx::Error> {
    let result = sqlx::query!(
        r#"
        INSERT INTO emails (subject, body, thread_id, receiver_user_id, sender_email, receiver_email, message_id, bucket)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?)
        "#,
        send.subject,
        send.body,
        send.thread_id,
        send.receiver_user_id,
        send.sender_email,
        send.receiver_email,
        send.message_id,
        location
    )
    .execute(pool)
    .await?;

    let email_id = result.last_insert_id();

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
