//! Shared `CloudTalk` webhook payloads. Phones are fixture numbers, never real ones.

/// Inbound SMS carrying text, with the `[sender]`/`[text]` suffixes an older mapping produced.
pub const INBOUND_SMS: &[u8] = b"{\"id\":null,\"sender\":\"+16468956758[sender]\",\"recipient\":\"+13173161456[recipient]\",\"text\":\"[text]\xd0\x9d\xd0\xb5 \xd0\xbf\xd0\xb8\xd1\x88\xd0\xb8 \xd1\x81\xd1\x8e\xd0\xb4\xd0\xb0\",\"agent\":\"540273\"}";

/// Shape captured in production 2026-08-04: a photo-only MMS sends text as JSON null.
pub const INBOUND_MMS_NULL_TEXT: &[u8] = b"{\"id\":51753924,\"sender\":\"+16468956758\",\"recipient\":\"+13173161456\",\"text\":null,\"agent\":null,\"media\":null,\"attachments\":null,\"media_urls\":null}";

#[cfg(test)]
mod flow_enrollment_tests {
    use super::INBOUND_SMS;
    use crate::axum_helpers::guards::CORRECT_ID;
    use crate::crud::cloudtalk::cancel_flow_enrollments_on_reply;
    use crate::tests::utils::new_test_app;
    use axum::http::StatusCode;
    use sqlx::MySqlPool;

    // Seeds one sms_flow_enrollments row with the given company/phone/status, using
    // flow_id 1 and user_id 1 (no FK enforced on this table) and anchor_at = now.
    async fn insert_enrollment(pool: &MySqlPool, company_id: i32, phone_digits: u64, status: &str) {
        sqlx::query!(
            r#"
            INSERT INTO sms_flow_enrollments
                (flow_id, company_id, customer_phone_digits, user_id, status, anchor_at)
            VALUES (1, ?, ?, 1, ?, UTC_TIMESTAMP())
            "#,
            company_id,
            phone_digits,
            status,
        )
        .execute(pool)
        .await
        .unwrap();
    }

    async fn status_of(pool: &MySqlPool, company_id: i32, phone_digits: u64, status: &str) -> i64 {
        sqlx::query!(
            r#"
            SELECT COUNT(*) AS c FROM sms_flow_enrollments
            WHERE company_id = ? AND customer_phone_digits = ? AND status = ?
            "#,
            company_id,
            phone_digits,
            status,
        )
        .fetch_one(pool)
        .await
        .unwrap()
        .c
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn cancel_flow_enrollments_on_reply_stops_active_and_paused(pool: MySqlPool) {
        // Matching company/phone: active + paused must both stop.
        insert_enrollment(&pool, 1, 5551234567, "active").await;
        insert_enrollment(&pool, 1, 5551234567, "paused").await;
        // Same company/phone but already terminal: must not be touched or counted.
        insert_enrollment(&pool, 1, 5551234567, "completed").await;
        // Same company, different phone: must not be touched.
        insert_enrollment(&pool, 1, 5559999999, "active").await;
        // Different company, same phone: must not be touched.
        insert_enrollment(&pool, 2, 5551234567, "active").await;

        let affected = cancel_flow_enrollments_on_reply(&pool, 1, 5551234567)
            .await
            .unwrap();
        assert_eq!(affected, 2);

        assert_eq!(status_of(&pool, 1, 5551234567, "stopped_by_reply").await, 2);
        assert_eq!(status_of(&pool, 1, 5551234567, "completed").await, 1);
        assert_eq!(status_of(&pool, 1, 5559999999, "active").await, 1);
        assert_eq!(status_of(&pool, 2, 5551234567, "active").await, 1);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn inbound_sms_webhook_cancels_matching_enrollments(pool: MySqlPool) {
        // INBOUND_SMS's sender is +16468956758 -> last-10-digit 6468956758.
        insert_enrollment(&pool, 42, 6468956758, "active").await;

        let app = new_test_app(pool.clone());
        let body: serde_json::Value = serde_json::from_slice(INBOUND_SMS).expect("parse fixture");
        let response = app
            .post("/cloudtalk/sms/42")
            .authorization_bearer(CORRECT_ID.to_string())
            .json(&body)
            .await;
        assert_eq!(response.status_code(), StatusCode::OK);

        assert_eq!(
            status_of(&pool, 42, 6468956758, "stopped_by_reply").await,
            1,
            "the matching enrollment must be stopped by the inbound reply"
        );
    }

    // Same shape as INBOUND_SMS but carries a cloudtalk_id, so redelivering it collides
    // with the (company_id, cloudtalk_id) unique key and INSERT IGNORE inserts zero rows.
    const INBOUND_SMS_WITH_ID: &[u8] = b"{\"id\":2200000200,\"sender\":\"+16468956758[sender]\",\"recipient\":\"+13173161456[recipient]\",\"text\":\"[text]hello\",\"agent\":\"540273\"}";

    #[sqlx::test(migrations = "../migrations")]
    async fn redelivered_inbound_sms_does_not_cancel_enrollment_started_after_original(
        pool: MySqlPool,
    ) {
        let app = new_test_app(pool.clone());
        let body: serde_json::Value =
            serde_json::from_slice(INBOUND_SMS_WITH_ID).expect("parse fixture");

        // Original delivery: no enrollment exists yet, so there is nothing to cancel; this
        // just establishes the row the redelivery below will collide with.
        let first = app
            .post("/cloudtalk/sms/42")
            .authorization_bearer(CORRECT_ID.to_string())
            .json(&body)
            .await;
        assert_eq!(first.status_code(), StatusCode::OK);

        // A rep starts a new flow after the original reply was processed.
        insert_enrollment(&pool, 42, 6468956758, "active").await;

        // CloudTalk redelivers the same webhook (e.g. it missed the original 200 response).
        let second = app
            .post("/cloudtalk/sms/42")
            .authorization_bearer(CORRECT_ID.to_string())
            .json(&body)
            .await;
        assert_eq!(second.status_code(), StatusCode::OK);

        assert_eq!(
            status_of(&pool, 42, 6468956758, "active").await,
            1,
            "redelivery of an already-stored webhook must not cancel a newer enrollment"
        );
        assert_eq!(
            status_of(&pool, 42, 6468956758, "stopped_by_reply").await,
            0,
            "no new reply occurred on the redelivery; nothing should be stopped"
        );
    }
}
