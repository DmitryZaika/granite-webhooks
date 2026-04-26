use crate::{
    amazonses::parse_email::{ParsedEmail, UploadedAttachment},
    crud::users::ReceivingEmail,
};
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
            thread_id: final_thread_id,
            receiver_user_id,
            sender_email: email.sender_email.clone(),
            receiver_email,
            message_id: email.message_id.clone(),
        }
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

    for attachment in attachments {
        insert_email_attachment(pool, email_id, attachment).await?;
    }
    Ok(result)
}
