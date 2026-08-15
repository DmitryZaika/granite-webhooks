use crate::axum_helpers::guards::{CloudTalkWebhookUser, NotificationsTelegramBot};
use crate::cloudtalk::api::sync_customer_to_cloud_talk;
use crate::cloudtalk::schemas::CloudtalkSMS;
use crate::crud::cloudtalk::{
    cancel_flow_enrollments_on_reply, insert_inbound_sms, insert_outbound_sms,
};
use crate::crud::users::get_user_id_by_cloudtalk_agent;
use crate::libs::constants::{BAD_REQUEST, ERR_DB, OK_RESPONSE, internal_error};
use crate::libs::types::BasicResponse;
use crate::telegram::crm::{InboundSmsTelegramNotify, send_inbound_sms_telegram_notification};
use axum::body::Bytes;
use axum::extract::{Path, State};
use lambda_http::tracing;
use reqwest::Client;
use sqlx::MySqlPool;

fn parse_cloudtalk_sms(body: &Bytes, route: &'static str) -> Option<CloudtalkSMS> {
    match serde_json::from_slice::<CloudtalkSMS>(body) {
        Ok(form) => Some(form),
        Err(error) => {
            // Structured position/category only: never the serde Display message, which for some
            // payload shapes echoes fragments of the offending value.
            tracing::error!(
                route,
                category = ?error.classify(),
                line = error.line(),
                column = error.column(),
                "Error parsing cloudtalk sms payload"
            );
            None
        }
    }
}

pub async fn sms_received(
    _: CloudTalkWebhookUser,
    State(pool): State<MySqlPool>,
    Path(company_id): Path<i32>,
    body: Bytes,
) -> BasicResponse {
    // TEMP MMS capture (inbound spike): remove after the real payload is captured.
    // `Bytes` (not `String`) so a non-UTF-8 body still reaches the parse-failure path below,
    // rather than being rejected earlier by axum's `String` extractor.
    let digits = std::env::var("MMS_CAPTURE_DIGITS").unwrap_or_default();
    if !digits.is_empty() {
        let body_lossy = String::from_utf8_lossy(&body);
        if body_lossy.contains(digits.as_str()) {
            tracing::info!("MMS_CAPTURE company_id={company_id} body={body_lossy}");
        }
    }

    let Some(form) = parse_cloudtalk_sms(&body, "received") else {
        return BAD_REQUEST;
    };

    match insert_inbound_sms(&pool, &form, company_id).await {
        Ok(result) => {
            let rows_affected = result.rows_affected();
            if rows_affected > 0 {
                if let Err(error) =
                    cancel_flow_enrollments_on_reply(&pool, company_id, form.sender()).await
                {
                    tracing::error!(
                        ?error,
                        company_id,
                        "Failed to cancel sms flow enrollments on reply"
                    );
                }

                if let Some(agent) = form.agent.as_deref() {
                    if let Ok(Some(user_id)) =
                        get_user_id_by_cloudtalk_agent(&pool, company_id, agent).await
                    {
                        let sender_phone = form.sender().to_string();
                        let payload = InboundSmsTelegramNotify {
                            receiver_user_id: user_id,
                            sender_phone,
                            message: form.text.0.clone(),
                        };
                        let bot = NotificationsTelegramBot::default();
                        if let Err(error) =
                            send_inbound_sms_telegram_notification(&pool, &bot, &payload).await
                        {
                            tracing::error!(
                                ?error,
                                user_id = user_id,
                                company_id = company_id,
                                "Failed to send inbound sms telegram notification"
                            );
                        }
                    }
                }
            } else {
                // INSERT IGNORE hit the (company_id, cloudtalk_id) unique key: this is a
                // redelivery of an already-processed webhook, not a new reply. Cancelling
                // enrollments or re-notifying the rep here would be a duplicate action on
                // an already-handled delivery. Never log message text or the full phone
                // number.
                tracing::info!(
                    company_id,
                    rows_affected,
                    "Skipped sms flow enrollment cancel and telegram notify: deduped inbound sms delivery"
                );
            }
            OK_RESPONSE
        }
        Err(error) => {
            tracing::error!("Error inserting sms received into the database: {}", error);
            internal_error(ERR_DB)
        }
    }
}

pub async fn sms_sent(
    _: CloudTalkWebhookUser,
    State(pool): State<MySqlPool>,
    Path(company_id): Path<i32>,
    body: Bytes,
) -> BasicResponse {
    let Some(form) = parse_cloudtalk_sms(&body, "sent") else {
        return BAD_REQUEST;
    };

    match insert_outbound_sms(&pool, &form, company_id).await {
        Ok(_) => OK_RESPONSE,
        Err(error) => {
            tracing::error!("Error inserting sms sent into the database: {}", error);
            internal_error(ERR_DB)
        }
    }
}
pub async fn sync_cloudtalk(
    _: crate::axum_helpers::guards::RemixBackend,
    State(pool): State<MySqlPool>,
    Path((_company_id, customer_id)): Path<(i32, i32)>,
) -> BasicResponse {
    let client = Client::new();
    sync_customer_to_cloud_talk(&pool, &client, customer_id).await
}

#[cfg(test)]
mod tests {
    use super::parse_cloudtalk_sms;
    use crate::axum_helpers::guards::CORRECT_ID;
    use crate::tests::cloudtalk::{INBOUND_MMS_NULL_TEXT, INBOUND_SMS};
    use crate::tests::utils::new_test_app;
    use axum::body::Bytes;
    use axum::http::StatusCode;
    use lambda_http::tracing;
    use sqlx::MySqlPool;
    use tracing_test::traced_test;

    fn sms_json() -> serde_json::Value {
        serde_json::from_slice(INBOUND_SMS).expect("Failed to parse JSON")
    }

    struct CloudtalkReceivedSMS {
        pub sender: Option<i64>,
        pub recipient: i64,
        pub text: String,
        pub agent: Option<String>,
        pub company_id: Option<i32>,
    }

    async fn get_sms_received(pool: &MySqlPool) -> Vec<CloudtalkReceivedSMS> {
        sqlx::query_as!(
            CloudtalkReceivedSMS,
            "SELECT sender, recipient, text, agent, company_id FROM cloudtalk_sms"
        )
        .fetch_all(pool)
        .await
        .unwrap()
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn test_basic_sms(pool: MySqlPool) {
        let app = new_test_app(pool.clone());

        let response = app
            .post("/cloudtalk/sms/42")
            .authorization_bearer(CORRECT_ID.to_string())
            .json(&sms_json())
            .await;
        assert_eq!(response.status_code(), StatusCode::OK);

        let smss = get_sms_received(&pool).await;
        assert_eq!(smss.len(), 1);
        assert_eq!(smss[0].sender, Some(6468956758));
        assert_eq!(smss[0].recipient, 3173161456);
        assert_eq!(smss[0].text, "Не пиши сюда".to_string());
        assert_eq!(smss[0].agent, Some("540273".to_string()));
        assert_eq!(smss[0].company_id, Some(42));
    }

    const MESSAGE_WITH_ID: &[u8] = b"{\"id\":2200000000,\"sender\":\"+16468956758[sender]\",\"recipient\":\"+13173161456[recipient]\",\"text\":\"[text]hello\",\"agent\":\"540273\"}";

    fn sms_with_id_json() -> serde_json::Value {
        serde_json::from_slice(MESSAGE_WITH_ID).expect("Failed to parse JSON")
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn test_echo_dedupe_via_cloudtalk_id(pool: MySqlPool) {
        let app = new_test_app(pool.clone());

        let first = app
            .post("/cloudtalk/sms/42")
            .authorization_bearer(CORRECT_ID.to_string())
            .json(&sms_with_id_json())
            .await;
        assert_eq!(first.status_code(), StatusCode::OK);

        let second = app
            .post("/cloudtalk/sms/42")
            .authorization_bearer(CORRECT_ID.to_string())
            .json(&sms_with_id_json())
            .await;
        assert_eq!(second.status_code(), StatusCode::OK);

        let smss = get_sms_received(&pool).await;
        assert_eq!(smss.len(), 1, "duplicate cloudtalk_id should be ignored");
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn test_sms_received_malformed_json(pool: MySqlPool) {
        let app = new_test_app(pool.clone());

        let response = app
            .post("/cloudtalk/sms/42")
            .authorization_bearer(CORRECT_ID.to_string())
            .text("{not json")
            .await;
        assert_eq!(response.status_code(), StatusCode::BAD_REQUEST);

        let smss = get_sms_received(&pool).await;
        assert_eq!(smss.len(), 0, "malformed payload must not insert a row");
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn test_sms_received_null_text_is_stored(pool: MySqlPool) {
        let app = new_test_app(pool.clone());
        let body: serde_json::Value = serde_json::from_slice(INBOUND_MMS_NULL_TEXT).expect("parse");

        let response = app
            .post("/cloudtalk/sms/42")
            .authorization_bearer(CORRECT_ID.to_string())
            .json(&body)
            .await;
        assert_eq!(response.status_code(), StatusCode::OK);

        let smss = get_sms_received(&pool).await;
        assert_eq!(smss.len(), 1, "photo-only MMS must not be dropped");
        assert_eq!(smss[0].text, String::new());
        assert_eq!(smss[0].sender, Some(6468956758));
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn test_sms_rejected_without_bearer_token(pool: MySqlPool) {
        let app = new_test_app(pool.clone());
        let response = app.post("/cloudtalk/sms/42").json(&sms_json()).await;
        assert_eq!(response.status_code(), StatusCode::FORBIDDEN);
        let smss = get_sms_received(&pool).await;
        assert_eq!(
            smss.len(),
            0,
            "unauthenticated webhook must not insert a row"
        );
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn test_sms_sent_inserts_outbound(pool: MySqlPool) {
        let app = new_test_app(pool.clone());

        let response = app
            .post("/cloudtalk/sms/sent/42")
            .authorization_bearer(CORRECT_ID.to_string())
            .json(&sms_with_id_json())
            .await;
        assert_eq!(response.status_code(), StatusCode::OK);

        let row = sqlx::query!(
            "SELECT COUNT(*) AS cnt FROM cloudtalk_sms \
             WHERE direction = 'outbound' AND status = 'sent' AND cloudtalk_id = 2200000000"
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            row.cnt, 1,
            "app-originated send should insert one outbound row"
        );
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn test_sms_sent_merges_crm_outbound_row(pool: MySqlPool) {
        sqlx::query!(
            "INSERT INTO cloudtalk_sms \
                (cloudtalk_id, sender, recipient, text, agent, company_id, direction, status) \
             VALUES (NULL, NULL, 3173161456, 'hello', '540273', 42, 'outbound', 'pending')"
        )
        .execute(&pool)
        .await
        .unwrap();

        let app = new_test_app(pool.clone());
        let response = app
            .post("/cloudtalk/sms/sent/42")
            .authorization_bearer(CORRECT_ID.to_string())
            .json(&sms_with_id_json())
            .await;
        assert_eq!(response.status_code(), StatusCode::OK);

        let smss = get_sms_received(&pool).await;
        assert_eq!(
            smss.len(),
            1,
            "echo must merge into the CRM row, not duplicate"
        );

        let merged = sqlx::query!(
            "SELECT cloudtalk_id FROM cloudtalk_sms WHERE company_id = 42 AND direction = 'outbound'"
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(merged.cloudtalk_id, Some(2200000000));
    }

    // Fallback echo: CRM stored 'cap' but the sent body (caption + links) differs.
    // Tier-2 merge must absorb the echo, not duplicate the row.
    const MESSAGE_FALLBACK_ECHO: &[u8] = b"{\"id\":2200000099,\"sender\":\"+16468956758[sender]\",\"recipient\":\"+13173161456[recipient]\",\"text\":\"[text]cap\\nPhoto 1: https://x/y.jpg\",\"agent\":\"540273\"}";

    #[sqlx::test(migrations = "../migrations")]
    async fn test_sms_sent_merges_crm_row_with_differing_text(pool: MySqlPool) {
        let parent = sqlx::query!(
            "INSERT INTO cloudtalk_sms \
                (cloudtalk_id, sender, recipient, text, agent, company_id, direction, status) \
             VALUES (NULL, NULL, 3173161456, 'cap', '540273', 42, 'outbound', 'sent')"
        )
        .execute(&pool)
        .await
        .unwrap()
        .last_insert_id();

        // Tier 2 is gated on an attachment existing (it exists solely for image-send fallbacks);
        // seed one the same way the cascade-delete test in crud/cloudtalk.rs does.
        sqlx::query!(
            "INSERT INTO cloudtalk_sms_attachments \
                (cloudtalk_sms_id, content_type, filename, s3_key, s3_url, width, height, position) \
             VALUES (?, 'image/jpeg', 'a.jpg', '42/u/a.jpg', 's3://gd-sms-attachments/42/u/a.jpg', 800, 600, 0)",
            i32::try_from(parent).expect("test id fits i32"),
        )
        .execute(&pool)
        .await
        .unwrap();

        let app = new_test_app(pool.clone());
        let body: serde_json::Value = serde_json::from_slice(MESSAGE_FALLBACK_ECHO).expect("parse");
        let response = app
            .post("/cloudtalk/sms/sent/42")
            .authorization_bearer(CORRECT_ID.to_string())
            .json(&body)
            .await;
        assert_eq!(response.status_code(), StatusCode::OK);

        let smss = get_sms_received(&pool).await;
        assert_eq!(
            smss.len(),
            1,
            "differing-text echo must merge via tier 2, not duplicate"
        );
        let merged = sqlx::query!(
            "SELECT cloudtalk_id FROM cloudtalk_sms WHERE company_id = 42 AND direction = 'outbound'"
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(merged.cloudtalk_id, Some(2200000099));
    }

    const MESSAGE_SENT_NULL_TEXT: &[u8] = b"{\"id\":2200000123,\"sender\":\"+16468956758\",\"recipient\":\"+13173161456\",\"text\":null,\"agent\":\"540273\",\"media\":null,\"attachments\":null,\"media_urls\":null}";

    #[sqlx::test(migrations = "../migrations")]
    async fn test_sms_sent_null_text_merges_crm_row(pool: MySqlPool) {
        sqlx::query!(
            "INSERT INTO cloudtalk_sms \
                (cloudtalk_id, sender, recipient, text, agent, company_id, direction, status) \
             VALUES (NULL, NULL, 3173161456, '', '540273', 42, 'outbound', 'pending')"
        )
        .execute(&pool)
        .await
        .unwrap();

        let app = new_test_app(pool.clone());
        let body: serde_json::Value =
            serde_json::from_slice(MESSAGE_SENT_NULL_TEXT).expect("parse");
        let response = app
            .post("/cloudtalk/sms/sent/42")
            .authorization_bearer(CORRECT_ID.to_string())
            .json(&body)
            .await;
        assert_eq!(response.status_code(), StatusCode::OK);

        let smss = get_sms_received(&pool).await;
        assert_eq!(
            smss.len(),
            1,
            "null-text echo must merge, not strand the row"
        );

        let merged = sqlx::query!(
            "SELECT cloudtalk_id FROM cloudtalk_sms WHERE company_id = 42 AND direction = 'outbound'"
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(merged.cloudtalk_id, Some(2200000123));
    }

    // A null recipient is genuinely unstorable, so it must still be rejected, but through the
    // handler's own logged parse branch rather than a silent extractor rejection.
    const MESSAGE_SENT_NULL_RECIPIENT: &[u8] =
        b"{\"id\":2200000124,\"sender\":\"+16468956758\",\"recipient\":null,\"text\":\"hi\",\"agent\":null}";

    #[test]
    #[traced_test]
    fn test_parse_failure_is_logged_with_position_only() {
        let body = Bytes::from_static(MESSAGE_SENT_NULL_RECIPIENT);

        assert!(parse_cloudtalk_sms(&body, "sent").is_none());

        assert!(logs_contain("Error parsing cloudtalk sms payload"));
        assert!(logs_contain("route=\"sent\""));
        assert!(!logs_contain("6468956758"), "payload must never be logged");
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn test_sms_sent_null_recipient_returns_bad_request(pool: MySqlPool) {
        let app = new_test_app(pool.clone());
        let body: serde_json::Value =
            serde_json::from_slice(MESSAGE_SENT_NULL_RECIPIENT).expect("parse");

        let response = app
            .post("/cloudtalk/sms/sent/42")
            .authorization_bearer(CORRECT_ID.to_string())
            .json(&body)
            .await;
        assert_eq!(response.status_code(), StatusCode::BAD_REQUEST);

        let smss = get_sms_received(&pool).await;
        assert_eq!(smss.len(), 0, "unparseable payload must not insert a row");
    }
}
