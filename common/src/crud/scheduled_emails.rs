use chrono::{Duration, Utc};
use sqlx::MySqlPool;
use sqlx::mysql::MySqlQueryResult;

use crate::crud::email_template::EmailTemplate;

pub struct ScheduledEmail {
    id: i32,
    template_body: String,
    customer_id: i32,
    email: String,
}

pub async fn insert_scheduled_email(
    pool: &MySqlPool,
    template: EmailTemplate,
    deal_id: u64,
    customer_id: i32,
    user_id: i32,
    company_id: i32,
) -> Result<MySqlQueryResult, sqlx::Error> {
    let hour_delay: i64 = template.hour_delay.unwrap_or(0).into();
    let send_at = Utc::now() + Duration::hours(hour_delay);
    sqlx::query!(
        r#"
        INSERT INTO scheduled_emails (template_id, deal_id, customer_id, user_id, company_id, send_at)
        VALUES (?, ?, ?, ?, ?, ?)
        "#,
        template.id,
        deal_id,
        customer_id,
        user_id,
        company_id,
        send_at
    )
    .execute(pool)
    .await
}

pub async fn get_ready_scheduled_emails(
    pool: &MySqlPool,
) -> Result<Vec<ScheduledEmail>, sqlx::Error> {
    sqlx::query_as!(
        ScheduledEmail,
        r#"
        SELECT scheduled_emails.id, template_body, customer_id, email
        FROM scheduled_emails
        JOIN users ON scheduled_emails.user_id = users.id
        JOIN email_templates ON scheduled_emails.template_id = email_templates.id
        WHERE send_at <= NOW() and sent_at IS NULL
        "#
    )
    .fetch_all(pool)
    .await
}

pub async fn mark_scheduled_email_as_sent(
    pool: &MySqlPool,
    id: i32,
) -> Result<MySqlQueryResult, sqlx::Error> {
    sqlx::query!(
        r#"
        UPDATE scheduled_emails
        SET sent_at = NOW()
        WHERE id = ?
        "#,
        id
    )
    .execute(pool)
    .await
}
