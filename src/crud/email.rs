use crate::amazonses::parse_email::{ParsedEmail, UploadedAttachment};
use sqlx::{MySqlPool, mysql::MySqlQueryResult};

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
    thread_id: Option<String>,
    receiver_user_id: Option<i32>,
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

pub async fn create_email_with_attachments(
    pool: &MySqlPool,
    email: &ParsedEmail,
    prior: &PriorEmail,
    attachments: &[UploadedAttachment],
) -> Result<MySqlQueryResult, sqlx::Error> {
    let result = sqlx::query!(
        r#"
        INSERT INTO emails (subject, body, thread_id, receiver_user_id, sender_email, receiver_email, message_id)
        VALUES (?, ?, ?, ?, ?, ?, ?)
        "#,
        email.subject,
        email.body,
        prior.thread_id,
        prior.receiver_user_id,
        email.sender_email,
        email.receiver_email,
        email.message_id
    )
    .execute(pool)
    .await?;

    let email_id = result.last_insert_id();

    for attachment in attachments {
        insert_email_attachment(pool, email_id, attachment).await?;
    }
    Ok(result)
}
