use crate::ringcentral::schemas::{RingcentralSMS, phone_last10};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use sqlx::MySqlPool;
use sqlx::mysql::MySqlQueryResult;
use std::error::Error;

pub async fn insert_inbound_sms(
    pool: &MySqlPool,
    sms: &RingcentralSMS,
    company_id: i32,
) -> Result<MySqlQueryResult, sqlx::Error> {
    sqlx::query(
        r#"
        INSERT IGNORE INTO ringcentral_sms
            (ringcentral_id, sender, recipient, text, agent, company_id, direction, status)
        VALUES (?, ?, ?, ?, ?, ?, 'inbound', 'received')
        "#,
    )
    .bind(sms.id)
    .bind(sms.sender())
    .bind(sms.recipient())
    .bind(&sms.text.0)
    .bind(&sms.agent)
    .bind(company_id)
    .execute(pool)
    .await
}

pub async fn insert_outbound_sms(
    pool: &MySqlPool,
    sms: &RingcentralSMS,
    company_id: i32,
) -> Result<MySqlQueryResult, sqlx::Error> {
    if let Some(ringcentral_id) = sms.id {
        // Tier 1: exact text match for normal text-only and true-MMS sends.
        // Oldest unclaimed row first: echoes arrive in send order, so it is the one waiting.
        let merged = sqlx::query(
            r#"
            UPDATE ringcentral_sms
               SET ringcentral_id = ?
             WHERE company_id = ?
               AND direction = 'outbound'
               AND ringcentral_id IS NULL
               AND status IN ('pending', 'sent')
               AND recipient = ?
               AND text = ?
               AND created_date >= (NOW() - INTERVAL 10 MINUTE)
             ORDER BY created_date ASC, id ASC
             LIMIT 1
            "#,
        )
        .bind(ringcentral_id)
        .bind(company_id)
        .bind(sms.recipient())
        .bind(&sms.text.0)
        .execute(pool)
        .await?;
        if merged.rows_affected() > 0 {
            return Ok(merged);
        }

        // Tier 2 matches image-send fallback echoes only (see buildFallbackSmsBody); byte-exact
        // LEFT/CONCAT (not LIKE) stops a caption's own '%'/'_' acting as a wildcard, gated on an attachment existing.
        let merged_loose = sqlx::query(
            r#"
            UPDATE ringcentral_sms
               SET ringcentral_id = ?
             WHERE company_id = ?
               AND direction = 'outbound'
               AND ringcentral_id IS NULL
               AND status IN ('pending', 'sent')
               AND recipient = ?
               AND EXISTS (
                   SELECT 1 FROM ringcentral_sms_attachments a
                    WHERE a.ringcentral_sms_id = ringcentral_sms.id
               )
               AND (
                   (text = '' AND ? LIKE 'Photo 1: %')
                   OR (
                       text <> ''
                       AND LEFT(?, CHAR_LENGTH(text) + 1) = CONCAT(text, '\n')
                       AND ? LIKE CONCAT('%', 'Photo 1: ', '%')
                   )
               )
               AND created_date >= (NOW() - INTERVAL 10 MINUTE)
             ORDER BY created_date ASC, id ASC
             LIMIT 1
            "#,
        )
        .bind(ringcentral_id)
        .bind(company_id)
        .bind(sms.recipient())
        .bind(&sms.text.0)
        .bind(&sms.text.0)
        .bind(&sms.text.0)
        .execute(pool)
        .await?;
        if merged_loose.rows_affected() > 0 {
            return Ok(merged_loose);
        }
    }

    sqlx::query(
        r#"
        INSERT IGNORE INTO ringcentral_sms
            (ringcentral_id, sender, recipient, text, agent, company_id, direction, status)
        VALUES (?, ?, ?, ?, ?, ?, 'outbound', 'sent')
        "#,
    )
    .bind(sms.id)
    .bind(sms.sender())
    .bind(sms.recipient())
    .bind(&sms.text.0)
    .bind(&sms.agent)
    .bind(company_id)
    .execute(pool)
    .await
}

#[derive(Debug, sqlx::FromRow)]
pub struct CustomerWithMapping {
    // From customers table
    pub id: i32,
    pub company_id: Option<i32>,
    pub name: Option<String>,
    pub phone: Option<String>,
    pub phone_2: Option<String>,
    pub email: Option<String>,
    pub address: Option<String>,

    // From ringcentral_contacts table (Optional due to LEFT JOIN)
    pub ringcentral_contact_id: Option<i32>,
    pub ringcentral_id: Option<i64>,
}

pub async fn load_customer_with_mapping(
    pool: &MySqlPool,
    customer_id: i32,
) -> Result<Option<CustomerWithMapping>, sqlx::Error> {
    let customer = sqlx::query_as::<_, CustomerWithMapping>(
        r#"
        SELECT
            c.id,
            c.company_id,
            c.name,
            c.phone,
            c.phone_2,
            ce.email,
            c.address,
            cc.id AS ringcentral_contact_id,
            cc.ringcentral_id
        FROM customers c
        LEFT JOIN customers_emails ce ON ce.id = c.email_id
        LEFT JOIN ringcentral_contacts cc ON cc.customer_id = c.id
        WHERE c.id = ? AND c.deleted_at IS NULL
        "#,
    )
    .bind(customer_id)
    .fetch_optional(pool)
    .await?;

    Ok(customer)
}

#[derive(sqlx::FromRow)]
struct RingCentralCompanyCreds {
    ringcentral_client_id: Option<String>,
    ringcentral_client_secret: Option<String>,
    ringcentral_jwt: Option<String>,
}

pub async fn company_has_ring_central(
    pool: &MySqlPool,
    company_id: i32,
) -> Result<bool, sqlx::Error> {
    let row = sqlx::query_as::<_, RingCentralCompanyCreds>(
        r#"
        SELECT ringcentral_client_id, ringcentral_client_secret, ringcentral_jwt
        FROM company
        WHERE id = ?
        "#,
    )
    .bind(company_id)
    .fetch_optional(pool)
    .await?;

    let has_creds = if let Some(r) = row {
        let key_ok = r
            .ringcentral_client_id
            .as_deref()
            .is_some_and(|s| !s.is_empty());
        let secret_ok = r
            .ringcentral_client_secret
            .as_deref()
            .is_some_and(|s| !s.is_empty());
        let jwt_ok = r.ringcentral_jwt.as_deref().is_some_and(|s| !s.is_empty());
        key_ok && secret_ok && jwt_ok
    } else {
        false
    };

    Ok(has_creds)
}

#[derive(sqlx::FromRow)]
struct RingCentralTokenCreds {
    ringcentral_client_id: Option<String>,
    ringcentral_client_secret: Option<String>,
    ringcentral_jwt: Option<String>,
    ringcentral_server_url: Option<String>,
}

pub async fn get_access_token(
    pool: &MySqlPool,
    company_id: u64,
) -> Result<(String, String), Box<dyn Error + Send + Sync>> {
    let row = sqlx::query_as::<_, RingCentralTokenCreds>(
        r#"
        SELECT ringcentral_client_id, ringcentral_client_secret, ringcentral_jwt,
               ringcentral_server_url
        FROM company
        WHERE id = ?
        "#,
    )
    .bind(company_id as i64)
    .fetch_optional(pool)
    .await?;

    let Some(r) = row else {
        return Err("RingCentral API credentials not found".into());
    };
    let client_id = r
        .ringcentral_client_id
        .filter(|s| !s.is_empty())
        .ok_or("RingCentral API credentials not found")?;
    let client_secret = r
        .ringcentral_client_secret
        .filter(|s| !s.is_empty())
        .ok_or("RingCentral API credentials not found")?;
    let jwt = r
        .ringcentral_jwt
        .filter(|s| !s.is_empty())
        .ok_or("RingCentral API credentials not found")?;
    let server_url = r
        .ringcentral_server_url
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "https://platform.ringcentral.com".to_string());

    let basic = STANDARD.encode(format!("{client_id}:{client_secret}"));
    let body = format!(
        "grant_type={}&assertion={}",
        urlencoding::encode("urn:ietf:params:oauth:grant-type:jwt-bearer"),
        urlencoding::encode(&jwt),
    );
    let client = reqwest::Client::new();
    let response = client
        .post(format!(
            "{}/restapi/oauth/token",
            server_url.trim_end_matches('/')
        ))
        .header("Authorization", format!("Basic {basic}"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!("RingCentral token error {status}: {body}").into());
    }

    let json: serde_json::Value = response.json().await?;
    let token = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or("RingCentral token response missing access_token")?
        .to_string();

    Ok((token, server_url))
}

pub async fn upsert_ringcentral_mapping(
    pool: &MySqlPool,
    customer_id: i32,
    company_id: i32,
    ringcentral_id: i64,
    phone1: Option<String>,
    phone2: Option<String>,
) -> Result<MySqlQueryResult, sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO ringcentral_contacts
            (customer_id, company_id, ringcentral_id, phone_e164_1, phone_e164_2)
        VALUES (?, ?, ?, ?, ?)
        ON DUPLICATE KEY UPDATE
            ringcentral_id = VALUES(ringcentral_id),
            phone_e164_1 = VALUES(phone_e164_1),
            phone_e164_2 = VALUES(phone_e164_2),
            last_error = NULL,
            last_synced_at = CURRENT_TIMESTAMP
        "#,
    )
    .bind(customer_id)
    .bind(company_id)
    .bind(ringcentral_id)
    .bind(phone1)
    .bind(phone2)
    .execute(pool)
    .await
}

pub async fn update_ringcentral_phone(
    pool: &MySqlPool,
    phone1: Option<String>,
    phone2: Option<String>,
    ringcentral_id: i64,
) -> Result<MySqlQueryResult, sqlx::Error> {
    sqlx::query(
        r#"
            UPDATE ringcentral_contacts
            SET last_error = NULL, phone_e164_1 = ?, phone_e164_2 = ?
            WHERE id = ?
            "#,
    )
    .bind(phone1)
    .bind(phone2)
    .bind(ringcentral_id)
    .execute(pool)
    .await
}

pub async fn find_local_ringcentral_id_by_phone(
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
        "(SELECT ringcentral_id FROM ringcentral_contacts \
          WHERE company_id = ? AND phone_e164_1 IN ({placeholders}) LIMIT 1) \
         UNION ALL \
         (SELECT ringcentral_id FROM ringcentral_contacts \
          WHERE company_id = ? AND phone_e164_2 IN ({placeholders}) LIMIT 1) \
         LIMIT 1"
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

/// Stops every pending follow-up for this phone in the company, regardless of rep or flow.
pub async fn cancel_flow_enrollments_on_reply(
    pool: &MySqlPool,
    company_id: i32,
    phone_digits: u64,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        r#"UPDATE sms_flow_enrollments
           SET status = 'stopped_by_reply', updated_at = UTC_TIMESTAMP()
           WHERE company_id = ? AND customer_phone_digits = ?
             AND status IN ('active', 'paused')"#,
    )
    .bind(company_id)
    .bind(phone_digits)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

#[derive(sqlx::FromRow)]
struct CustomerPhones {
    phone: Option<String>,
    phone_2: Option<String>,
}

/// Stops every pending follow-up for this customer: matching `customer_id`, or
/// last-10 of `customers.phone` / `phone_2` (skips empty / short numbers so a
/// CAST of 0 cannot false-match).
pub async fn cancel_flow_enrollments_for_customer(
    pool: &MySqlPool,
    company_id: i32,
    customer_id: i32,
) -> Result<u64, sqlx::Error> {
    let phones = sqlx::query_as::<_, CustomerPhones>(
        r#"SELECT phone, phone_2
           FROM customers
           WHERE id = ? AND company_id = ? AND deleted_at IS NULL"#,
    )
    .bind(customer_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;

    let mut affected = sqlx::query(
        r#"UPDATE sms_flow_enrollments
           SET status = 'stopped_by_reply', updated_at = UTC_TIMESTAMP()
           WHERE company_id = ? AND customer_id = ?
             AND status IN ('active', 'paused')"#,
    )
    .bind(company_id)
    .bind(customer_id)
    .execute(pool)
    .await?
    .rows_affected();

    if let Some(row) = phones {
        for raw in [row.phone, row.phone_2] {
            if let Some(digits) = raw.as_deref().and_then(phone_last10) {
                affected += cancel_flow_enrollments_on_reply(pool, company_id, digits).await?;
            }
        }
    }
    Ok(affected)
}

#[cfg(test)]
mod tests {
    use super::insert_outbound_sms;
    use crate::ringcentral::schemas::RingcentralSMS;
    use sqlx::MySqlPool;

    #[sqlx::test(migrations = "../migrations")]
    async fn test_sms_attachments_cascade_delete(pool: MySqlPool) {
        let parent = sqlx::query(
            "INSERT INTO ringcentral_sms \
                (ringcentral_id, sender, recipient, text, agent, company_id, direction, status) \
             VALUES (NULL, NULL, 3173161456, 'caption', '540273', 42, 'outbound', 'sent')",
        )
        .execute(&pool)
        .await
        .unwrap()
        .last_insert_id();

        sqlx::query(
            "INSERT INTO ringcentral_sms_attachments \
                (ringcentral_sms_id, content_type, filename, s3_key, s3_url, width, height, position) \
             VALUES (?, 'image/jpeg', 'a.jpg', '42/u/a.jpg', 's3://gd-sms-attachments/42/u/a.jpg', 800, 600, 0)",
        )
        .bind(parent as i32)
        .execute(&pool)
        .await
        .unwrap();

        let before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ringcentral_sms_attachments")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(before, 1);

        sqlx::query("DELETE FROM ringcentral_sms WHERE id = ?")
            .bind(parent as i32)
            .execute(&pool)
            .await
            .unwrap();

        let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ringcentral_sms_attachments")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            after, 0,
            "attachments must cascade-delete with the parent sms row"
        );
    }

    // Builds an outbound-echo fixture like the real webhook body, without the "[text]"
    // marker (that's only relevant to the raw-HTTP-body capture tests in receive.rs).
    fn echo_fixture(ringcentral_id: i64, text: &str) -> RingcentralSMS {
        let payload = serde_json::json!({
            "id": ringcentral_id,
            "sender": "+16468956758",
            "recipient": "+13173161456",
            "text": text,
            "agent": "540273",
        });
        serde_json::from_value(payload).expect("valid RingcentralSMS fixture")
    }

    // Seeds a CRM-originated outbound row (ringcentral_id NULL, awaiting the
    // RingCentral echo) for recipient 3173161456 / company 42, and returns its id.
    async fn insert_pending_outbound(pool: &MySqlPool, text: &str, status: &str) -> i32 {
        let id = sqlx::query(
            "INSERT INTO ringcentral_sms \
                (ringcentral_id, sender, recipient, text, agent, company_id, direction, status) \
             VALUES (NULL, NULL, 3173161456, ?, '540273', 42, 'outbound', ?)",
        )
        .bind(text)
        .bind(status)
        .execute(pool)
        .await
        .unwrap()
        .last_insert_id();
        i32::try_from(id).unwrap()
    }

    // Seeds an attachment row for `sms_id` (same pattern as the cascade-delete test above);
    // tier 2 requires at least one to exist.
    async fn insert_attachment_for(pool: &MySqlPool, sms_id: i32) {
        sqlx::query(
            "INSERT INTO ringcentral_sms_attachments \
                (ringcentral_sms_id, content_type, filename, s3_key, s3_url, width, height, position) \
             VALUES (?, 'image/jpeg', 'a.jpg', '42/u/a.jpg', 's3://gd-sms-attachments/42/u/a.jpg', 800, 600, 0)",
        )
        .bind(sms_id)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn ringcentral_id_of(pool: &MySqlPool, sms_id: i32) -> Option<i64> {
        sqlx::query_scalar::<_, Option<i64>>(
            "SELECT ringcentral_id FROM ringcentral_sms WHERE id = ?",
        )
        .bind(sms_id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn outbound_row_count(pool: &MySqlPool) -> i64 {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM ringcentral_sms WHERE company_id = 42 AND direction = 'outbound'",
        )
        .fetch_one(pool)
        .await
        .unwrap()
    }

    // (b) Image-only fallback: empty caption means the fallback body is the bare "Photo 1:
    // <url>" line (buildFallbackSmsBody's empty-caption branch); with an attachment, must merge.
    #[sqlx::test(migrations = "../migrations")]
    async fn test_tier2_merges_image_only_fallback_echo(pool: MySqlPool) {
        let row_id = insert_pending_outbound(&pool, "", "sent").await;
        insert_attachment_for(&pool, row_id).await;

        let sms = echo_fixture(2_200_000_101, "Photo 1: https://x/y.jpg");
        insert_outbound_sms(&pool, &sms, 42).await.unwrap();

        assert_eq!(ringcentral_id_of(&pool, row_id).await, Some(2_200_000_101));
        assert_eq!(
            outbound_row_count(&pool).await,
            1,
            "must merge, not duplicate"
        );
    }

    // (c) Caption fallback: body is "<caption>\nPhoto 1: <url>" (buildFallbackSmsBody's
    // non-empty-caption branch); with an attachment present, must merge.
    #[sqlx::test(migrations = "../migrations")]
    async fn test_tier2_merges_caption_fallback_echo(pool: MySqlPool) {
        let row_id = insert_pending_outbound(&pool, "cap", "sent").await;
        insert_attachment_for(&pool, row_id).await;

        let sms = echo_fixture(2_200_000_102, "cap\nPhoto 1: https://x/y.jpg");
        insert_outbound_sms(&pool, &sms, 42).await.unwrap();

        assert_eq!(ringcentral_id_of(&pool, row_id).await, Some(2_200_000_102));
        assert_eq!(
            outbound_row_count(&pool).await,
            1,
            "must merge, not duplicate"
        );
    }

    // (d) An unrelated echo to the same recipient must never be absorbed by an image-only
    // row just because its text is empty; that was the BLOCKER data-loss bug this redesign fixes.
    #[sqlx::test(migrations = "../migrations")]
    async fn test_tier2_does_not_merge_unrelated_echo_into_image_only_row(pool: MySqlPool) {
        let row_id = insert_pending_outbound(&pool, "", "sent").await;
        insert_attachment_for(&pool, row_id).await;

        let sms = echo_fixture(2_200_000_103, "Thanks, see you at 3");
        insert_outbound_sms(&pool, &sms, 42).await.unwrap();

        assert_eq!(
            ringcentral_id_of(&pool, row_id).await,
            None,
            "unrelated echo must not merge into the image-only row"
        );
        assert_eq!(
            outbound_row_count(&pool).await,
            2,
            "unrelated echo must insert its own row"
        );
    }

    // (e) Prefix collision: a plain-text send of "Hi" must not absorb an
    // unrelated echo that merely starts with "Hi" ("Hi there!").
    #[sqlx::test(migrations = "../migrations")]
    async fn test_tier2_does_not_merge_prefix_collision(pool: MySqlPool) {
        let row_id = insert_pending_outbound(&pool, "Hi", "sent").await;
        insert_attachment_for(&pool, row_id).await;

        let sms = echo_fixture(2_200_000_104, "Hi there!");
        insert_outbound_sms(&pool, &sms, 42).await.unwrap();

        assert_eq!(
            ringcentral_id_of(&pool, row_id).await,
            None,
            "prefix collision must not merge"
        );
        assert_eq!(outbound_row_count(&pool).await, 2);
    }

    // (f) A text-only send (no attachment rows) must never tier-2 merge, even against an
    // echo with the exact fallback shape; tier 2 exists solely for image-send fallbacks.
    #[sqlx::test(migrations = "../migrations")]
    async fn test_tier2_never_merges_row_without_attachment(pool: MySqlPool) {
        let row_id = insert_pending_outbound(&pool, "", "sent").await;
        // Deliberately no insert_attachment_for(&pool, row_id) call.

        let sms = echo_fixture(2_200_000_105, "Photo 1: https://x/y.jpg");
        insert_outbound_sms(&pool, &sms, 42).await.unwrap();

        assert_eq!(
            ringcentral_id_of(&pool, row_id).await,
            None,
            "row without an attachment must never tier-2 merge"
        );
        assert_eq!(outbound_row_count(&pool).await, 2);
    }

    // (g1) A literal '%' inside the caption must be treated as an ordinary
    // character (equality, not LIKE), so its real fallback echo still merges.
    #[sqlx::test(migrations = "../migrations")]
    async fn test_tier2_merges_fallback_with_percent_in_caption(pool: MySqlPool) {
        let row_id = insert_pending_outbound(&pool, "save 50% now", "sent").await;
        insert_attachment_for(&pool, row_id).await;

        let sms = echo_fixture(2_200_000_106, "save 50% now\nPhoto 1: https://x/y.jpg");
        insert_outbound_sms(&pool, &sms, 42).await.unwrap();

        assert_eq!(ringcentral_id_of(&pool, row_id).await, Some(2_200_000_106));
    }

    // (g2) An unrelated echo starting with "save 50" must not merge, even though the old
    // `LIKE CONCAT(text, '%')` tier-2 let the caption's own '%' act as a SQL wildcard.
    #[sqlx::test(migrations = "../migrations")]
    async fn test_tier2_does_not_merge_unrelated_echo_with_percent_caption(pool: MySqlPool) {
        let row_id = insert_pending_outbound(&pool, "save 50% now", "sent").await;
        insert_attachment_for(&pool, row_id).await;

        let sms = echo_fixture(2_200_000_107, "save 50 different times right now");
        insert_outbound_sms(&pool, &sms, 42).await.unwrap();

        assert_eq!(
            ringcentral_id_of(&pool, row_id).await,
            None,
            "unrelated echo must not merge just because the stored '%' acted as a wildcard"
        );
    }

    // FIFO: echoes arrive in send order, so the oldest unclaimed candidate
    // row must absorb first, not the newest.
    #[sqlx::test(migrations = "../migrations")]
    async fn test_merge_picks_oldest_unclaimed_row_first(pool: MySqlPool) {
        let older_id = insert_pending_outbound(&pool, "hello", "pending").await;
        let newer_id = insert_pending_outbound(&pool, "hello", "pending").await;
        assert!(
            older_id < newer_id,
            "test setup: older row must have the lower id"
        );

        let sms = echo_fixture(2_200_000_108, "hello");
        insert_outbound_sms(&pool, &sms, 42).await.unwrap();

        assert_eq!(
            ringcentral_id_of(&pool, older_id).await,
            Some(2_200_000_108),
            "the oldest unclaimed row must absorb the echo first"
        );
        assert_eq!(ringcentral_id_of(&pool, newer_id).await, None);
    }
}
