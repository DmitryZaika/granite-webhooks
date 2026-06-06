use crate::cloudtalk::schemas::CloudtalkSMS;
use sqlx::MySqlPool;
use sqlx::mysql::MySqlQueryResult;

pub async fn insert_inbound_sms(
    pool: &MySqlPool,
    sms: &CloudtalkSMS,
    company_id: i32,
) -> Result<MySqlQueryResult, sqlx::Error> {
    sqlx::query!(
        r#"
        INSERT IGNORE INTO cloudtalk_sms
            (cloudtalk_id, sender, recipient, text, agent, company_id, direction, status)
        VALUES (?, ?, ?, ?, ?, ?, 'inbound', 'received')
        "#,
        sms.id,
        sms.sender(),
        sms.recipient(),
        sms.text.0,
        sms.agent,
        company_id,
    )
    .execute(pool)
    .await
}

pub async fn insert_outbound_sms(
    pool: &MySqlPool,
    sms: &CloudtalkSMS,
    company_id: i32,
) -> Result<MySqlQueryResult, sqlx::Error> {
    if let Some(cloudtalk_id) = sms.id {
        let merged = sqlx::query!(
            r#"
            UPDATE cloudtalk_sms
               SET cloudtalk_id = ?
             WHERE company_id = ?
               AND direction = 'outbound'
               AND cloudtalk_id IS NULL
               AND recipient = ?
               AND text = ?
               AND created_date >= (NOW() - INTERVAL 10 MINUTE)
             ORDER BY created_date DESC
             LIMIT 1
            "#,
            cloudtalk_id,
            company_id,
            sms.recipient(),
            sms.text.0,
        )
        .execute(pool)
        .await?;
        if merged.rows_affected() > 0 {
            return Ok(merged);
        }
    }

    sqlx::query!(
        r#"
        INSERT IGNORE INTO cloudtalk_sms
            (cloudtalk_id, sender, recipient, text, agent, company_id, direction, status)
        VALUES (?, ?, ?, ?, ?, ?, 'outbound', 'sent')
        "#,
        sms.id,
        sms.sender(),
        sms.recipient(),
        sms.text.0,
        sms.agent,
        company_id,
    )
    .execute(pool)
    .await
}

pub struct CustomerWithMapping {
    // From customers table
    pub id: i32,
    pub company_id: Option<i32>,
    pub name: Option<String>,
    pub phone: Option<String>,
    pub phone_2: Option<String>,
    pub email: Option<String>,
    pub address: Option<String>,
    pub deleted_at: Option<sqlx::types::chrono::NaiveDateTime>, // Adjust based on your actual datetime crate

    // From cloudtalk_contacts table (Optional due to LEFT JOIN)
    pub cloudtalk_contact_id: Option<i32>,
    pub cloudtalk_id: Option<i64>,
}

pub async fn load_customer_with_mapping(
    pool: &MySqlPool,
    customer_id: i32,
) -> Result<Option<CustomerWithMapping>, sqlx::Error> {
    let customer = sqlx::query_as!(
        CustomerWithMapping,
        r#"
        SELECT
            c.id,
            c.company_id,
            c.name,
            c.phone,
            c.phone_2,
            c.email,
            c.address,
            c.deleted_at,
            cc.id AS cloudtalk_contact_id,
            cc.cloudtalk_id
        FROM customers c
        LEFT JOIN cloudtalk_contacts cc ON cc.customer_id = c.id
        WHERE c.id = ?
        "#,
        customer_id
    )
    .fetch_optional(pool)
    .await?;

    Ok(customer)
}

pub async fn company_has_cloud_talk(
    pool: &MySqlPool,
    company_id: i32,
) -> Result<bool, sqlx::Error> {
    // 1. Query the database directly
    let row = sqlx::query!(
        r#"
        SELECT cloudtalk_access_key, cloudtalk_access_secret
        FROM company
        WHERE id = ?
        "#,
        company_id
    )
    .fetch_optional(pool)
    .await?;

    // 2. Evaluate JS-style truthiness (checks if present and not an empty string)
    let has_creds = if let Some(r) = row {
        let key_is_valid = r
            .cloudtalk_access_key
            .as_deref()
            .is_some_and(|s| !s.is_empty());
        let secret_is_valid = r
            .cloudtalk_access_secret
            .as_deref()
            .is_some_and(|s| !s.is_empty());

        key_is_valid && secret_is_valid
    } else {
        false
    };

    Ok(has_creds)
}
