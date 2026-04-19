use crate::cloudtalk::schemas::CloudtalkSMS;
use sqlx::MySqlPool;
use sqlx::mysql::MySqlQueryResult;

pub async fn insert_cloudtalk_sms(
    pool: &MySqlPool,
    sms: &CloudtalkSMS,
) -> Result<MySqlQueryResult, sqlx::Error> {
    sqlx::query!(
        r#"
        INSERT INTO cloudtalk_sms (id, sender, recipient, text, agent)
        VALUES (?, ?, ?, ?, ?)
        "#,
        sms.id,
        sms.sender(),
        sms.recipient(),
        sms.text.0,
        sms.agent,
    )
    .execute(pool)
    .await
}
