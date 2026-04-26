use chrono::{Duration, Utc};
use sqlx::MySqlPool;
use sqlx::mysql::MySqlQueryResult;

use crate::crud::email_template::EmailTemplate;

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
