use crate::cloudtalk::schemas::CloudtalkSMS;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use sqlx::MySqlPool;
use sqlx::mysql::MySqlQueryResult;
use std::error::Error;

pub async fn insert_cloudtalk_sms(
    pool: &MySqlPool,
    sms: &CloudtalkSMS,
    company_id: i32,
) -> Result<MySqlQueryResult, sqlx::Error> {
    sqlx::query!(
        r#"
        INSERT INTO cloudtalk_sms (id, sender, recipient, text, agent, company_id)
        VALUES (?, ?, ?, ?, ?, ?)
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
            cc.id AS cloudtalk_contact_id,
            cc.cloudtalk_id
        FROM customers c
        LEFT JOIN cloudtalk_contacts cc ON cc.customer_id = c.id
        WHERE c.id = ? AND c.deleted_at IS NULL
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

pub async fn get_auth_string(
    pool: &MySqlPool,
    company_id: u64,
) -> Result<String, Box<dyn Error + Send + Sync>> {
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

    // 2. Validate row existence and JS-style truthiness (not None, not empty string)
    let creds = match row {
        Some(r) => match (r.cloudtalk_access_key, r.cloudtalk_access_secret) {
            (Some(key), Some(secret)) if !key.is_empty() && !secret.is_empty() => {
                format!("{}:{}", key, secret)
            }
            _ => return Err("CloudTalk API credentials not found".into()),
        },
        None => return Err("CloudTalk API credentials not found".into()),
    };

    // 3. Base64 encode the combined string (equivalent to btoa)
    let auth_string = STANDARD.encode(creds);

    Ok(auth_string)
}

pub async fn update_cloudtalk_phone(
    pool: &MySqlPool,
    phone1: Option<String>,
    phone2: Option<String>,
    cloudtalk_id: i64,
) -> Result<MySqlQueryResult, sqlx::Error> {
    sqlx::query!(
        r#"
            UPDATE cloudtalk_contacts
            SET last_error = NULL, phone_e164_1 = ?, phone_e164_2 = ?
            WHERE id = ?
            "#,
        phone1,
        phone2,
        cloudtalk_id
    )
    .execute(pool)
    .await
}

pub async fn find_local_cloudtalk_id_by_phone(
    pool: &MySqlPool,
    company_id: u64,
    e164_phones: &[String],
) -> Result<Option<i32>, sqlx::Error> {
    if e164_phones.is_empty() {
        return Ok(None);
    }

    // Dynamic placeholders generation for sqlx dynamic bindings
    let placeholders = vec!["?"; e164_phones.len()].join(",");
    let sql = format!(
        "(SELECT cloudtalk_id FROM cloudtalk_contacts \
          WHERE company_id = ? AND phone_e164_1 IN ({}) LIMIT 1) \
         UNION ALL \
         (SELECT cloudtalk_id FROM cloudtalk_contacts \
          WHERE company_id = ? AND phone_e164_2 IN ({}) LIMIT 1) \
         LIMIT 1",
        placeholders, placeholders
    );

    let mut query = sqlx::query_scalar::<_, i32>(&sql);

    // Bind parameters sequentially for UNION parts
    query = query.bind(company_id);
    for phone in e164_phones {
        query = query.bind(phone);
    }
    query = query.bind(company_id);
    for phone in e164_phones {
        query = query.bind(phone);
    }

    query.fetch_optional(pool).await
}
