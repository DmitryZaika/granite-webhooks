use crate::cloudtalk::schemas::CloudtalkSMS;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use sqlx::MySqlPool;
use sqlx::mysql::MySqlQueryResult;
use std::error::Error;

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
        // Tier 1: exact text match for normal text-only and true-MMS sends.
        // Oldest unclaimed row first: echoes arrive in send order, so it is the one waiting.
        let merged = sqlx::query!(
            r#"
            UPDATE cloudtalk_sms
               SET cloudtalk_id = ?
             WHERE company_id = ?
               AND direction = 'outbound'
               AND cloudtalk_id IS NULL
               AND status IN ('pending', 'sent')
               AND recipient = ?
               AND text = ?
               AND created_date >= (NOW() - INTERVAL 10 MINUTE)
             ORDER BY created_date ASC, id ASC
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

        // Tier 2 matches image-send fallback echoes only (see buildFallbackSmsBody); byte-exact
        // LEFT/CONCAT (not LIKE) stops a caption's own '%'/'_' acting as a wildcard, gated on an attachment existing.
        let merged_loose = sqlx::query!(
            r#"
            UPDATE cloudtalk_sms
               SET cloudtalk_id = ?
             WHERE company_id = ?
               AND direction = 'outbound'
               AND cloudtalk_id IS NULL
               AND status IN ('pending', 'sent')
               AND recipient = ?
               AND EXISTS (
                   SELECT 1 FROM cloudtalk_sms_attachments a
                    WHERE a.cloudtalk_sms_id = cloudtalk_sms.id
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
            cloudtalk_id,
            company_id,
            sms.recipient(),
            sms.text.0,
            sms.text.0,
            sms.text.0,
        )
        .execute(pool)
        .await?;
        if merged_loose.rows_affected() > 0 {
            return Ok(merged_loose);
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
            ce.email,
            c.address,
            cc.id AS cloudtalk_contact_id,
            cc.cloudtalk_id
        FROM customers c
        LEFT JOIN customers_emails ce ON ce.id = c.email_id
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
                format!("{key}:{secret}")
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
          WHERE company_id = ? AND phone_e164_1 IN ({placeholders}) LIMIT 1) \
         UNION ALL \
         (SELECT cloudtalk_id FROM cloudtalk_contacts \
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
    let result = sqlx::query!(
        r#"UPDATE sms_flow_enrollments
           SET status = 'stopped_by_reply', updated_at = UTC_TIMESTAMP()
           WHERE company_id = ? AND customer_phone_digits = ?
             AND status IN ('active', 'paused')"#,
        company_id,
        phone_digits
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::insert_outbound_sms;
    use crate::cloudtalk::schemas::CloudtalkSMS;
    use sqlx::MySqlPool;

    #[sqlx::test(migrations = "../migrations")]
    async fn test_sms_attachments_cascade_delete(pool: MySqlPool) {
        let parent = sqlx::query!(
            "INSERT INTO cloudtalk_sms \
                (cloudtalk_id, sender, recipient, text, agent, company_id, direction, status) \
             VALUES (NULL, NULL, 3173161456, 'caption', '540273', 42, 'outbound', 'sent')"
        )
        .execute(&pool)
        .await
        .unwrap()
        .last_insert_id();

        sqlx::query!(
            "INSERT INTO cloudtalk_sms_attachments \
                (cloudtalk_sms_id, content_type, filename, s3_key, s3_url, width, height, position) \
             VALUES (?, 'image/jpeg', 'a.jpg', '42/u/a.jpg', 's3://gd-sms-attachments/42/u/a.jpg', 800, 600, 0)",
            parent as i32,
        )
        .execute(&pool)
        .await
        .unwrap();

        let before = sqlx::query!("SELECT COUNT(*) AS c FROM cloudtalk_sms_attachments")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(before.c, 1);

        sqlx::query!("DELETE FROM cloudtalk_sms WHERE id = ?", parent as i32)
            .execute(&pool)
            .await
            .unwrap();

        let after = sqlx::query!("SELECT COUNT(*) AS c FROM cloudtalk_sms_attachments")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            after.c, 0,
            "attachments must cascade-delete with the parent sms row"
        );
    }

    // Builds an outbound-echo fixture like the real webhook body, without the "[text]"
    // marker (that's only relevant to the raw-HTTP-body capture tests in receive.rs).
    fn echo_fixture(cloudtalk_id: i64, text: &str) -> CloudtalkSMS {
        let payload = serde_json::json!({
            "id": cloudtalk_id,
            "sender": "+16468956758",
            "recipient": "+13173161456",
            "text": text,
            "agent": "540273",
        });
        serde_json::from_value(payload).expect("valid CloudtalkSMS fixture")
    }

    // Seeds a CRM-originated outbound row (cloudtalk_id NULL, awaiting the
    // CloudTalk echo) for recipient 3173161456 / company 42, and returns its id.
    async fn insert_pending_outbound(pool: &MySqlPool, text: &str, status: &str) -> i32 {
        let id = sqlx::query!(
            "INSERT INTO cloudtalk_sms \
                (cloudtalk_id, sender, recipient, text, agent, company_id, direction, status) \
             VALUES (NULL, NULL, 3173161456, ?, '540273', 42, 'outbound', ?)",
            text,
            status,
        )
        .execute(pool)
        .await
        .unwrap()
        .last_insert_id();
        i32::try_from(id).unwrap()
    }

    // Seeds an attachment row for `sms_id` (same pattern as the cascade-delete test above);
    // tier 2 requires at least one to exist.
    async fn insert_attachment_for(pool: &MySqlPool, sms_id: i32) {
        sqlx::query!(
            "INSERT INTO cloudtalk_sms_attachments \
                (cloudtalk_sms_id, content_type, filename, s3_key, s3_url, width, height, position) \
             VALUES (?, 'image/jpeg', 'a.jpg', '42/u/a.jpg', 's3://gd-sms-attachments/42/u/a.jpg', 800, 600, 0)",
            sms_id,
        )
        .execute(pool)
        .await
        .unwrap();
    }

    async fn cloudtalk_id_of(pool: &MySqlPool, sms_id: i32) -> Option<i64> {
        sqlx::query!(
            "SELECT cloudtalk_id FROM cloudtalk_sms WHERE id = ?",
            sms_id
        )
        .fetch_one(pool)
        .await
        .unwrap()
        .cloudtalk_id
    }

    async fn outbound_row_count(pool: &MySqlPool) -> i64 {
        sqlx::query!(
            "SELECT COUNT(*) AS c FROM cloudtalk_sms WHERE company_id = 42 AND direction = 'outbound'"
        )
        .fetch_one(pool)
        .await
        .unwrap()
        .c
    }

    // (b) Image-only fallback: empty caption means the fallback body is the bare "Photo 1:
    // <url>" line (buildFallbackSmsBody's empty-caption branch); with an attachment, must merge.
    #[sqlx::test(migrations = "../migrations")]
    async fn test_tier2_merges_image_only_fallback_echo(pool: MySqlPool) {
        let row_id = insert_pending_outbound(&pool, "", "sent").await;
        insert_attachment_for(&pool, row_id).await;

        let sms = echo_fixture(2_200_000_101, "Photo 1: https://x/y.jpg");
        insert_outbound_sms(&pool, &sms, 42).await.unwrap();

        assert_eq!(cloudtalk_id_of(&pool, row_id).await, Some(2_200_000_101));
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

        assert_eq!(cloudtalk_id_of(&pool, row_id).await, Some(2_200_000_102));
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
            cloudtalk_id_of(&pool, row_id).await,
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
            cloudtalk_id_of(&pool, row_id).await,
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
            cloudtalk_id_of(&pool, row_id).await,
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

        assert_eq!(cloudtalk_id_of(&pool, row_id).await, Some(2_200_000_106));
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
            cloudtalk_id_of(&pool, row_id).await,
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
            cloudtalk_id_of(&pool, older_id).await,
            Some(2_200_000_108),
            "the oldest unclaimed row must absorb the echo first"
        );
        assert_eq!(cloudtalk_id_of(&pool, newer_id).await, None);
    }
}
