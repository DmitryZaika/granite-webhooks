use crate::axum_helpers::guards::{NotificationsTelegramBot, RingCentralWebhookUser};
use crate::crud::deals::{find_customer_id_by_phone_last10, maybe_move_deal_on_inbound_sms};
use crate::crud::ringcentral::{
    cancel_flow_enrollments_for_customer, cancel_flow_enrollments_on_reply, insert_inbound_sms,
    insert_outbound_sms,
};
use crate::crud::users::get_user_id_by_ringcentral_agent;
use crate::libs::app_request::{SmsFollowupCallCheckBody, spawn_sms_followup_call_check};
use crate::libs::constants::{BAD_REQUEST, ERR_DB, OK_RESPONSE, internal_error};
use crate::libs::types::BasicResponse;
use crate::ringcentral::api::sync_customer_to_ring_central;
use crate::ringcentral::schemas::{
    RingcentralSMS, inbound_customer_phone_from_call_payload, outbound_call_followup_check,
};
use crate::telegram::crm::{InboundSmsTelegramNotify, send_inbound_sms_telegram_notification};
use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use lambda_http::tracing;
use reqwest::Client;
use sqlx::MySqlPool;

fn validation_token_response(headers: &HeaderMap) -> Option<Response> {
    let token = headers.get("Validation-Token")?;
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::OK;
    response
        .headers_mut()
        .insert("Validation-Token", token.clone());
    Some(response)
}

fn parse_ringcentral_sms(body: &Bytes, route: &'static str) -> Option<RingcentralSMS> {
    match serde_json::from_slice::<RingcentralSMS>(body) {
        Ok(form) => Some(form),
        Err(error) => {
            // Structured position/category only: never the serde Display message, which for some
            // payload shapes echoes fragments of the offending value.
            tracing::error!(
                route,
                category = ?error.classify(),
                line = error.line(),
                column = error.column(),
                "Error parsing ringcentral sms payload"
            );
            None
        }
    }
}

pub async fn sms_received(
    headers: HeaderMap,
    _: RingCentralWebhookUser,
    State(pool): State<MySqlPool>,
    Path(company_id): Path<i32>,
    body: Bytes,
) -> Response {
    if let Some(response) = validation_token_response(&headers) {
        return response;
    }
    sms_received_inner(pool, company_id, body).await.into_response()
}

async fn sms_received_inner(
    pool: MySqlPool,
    company_id: i32,
    body: Bytes,
) -> BasicResponse {
    let Some(form) = parse_ringcentral_sms(&body, "received") else {
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

                maybe_move_deal_on_inbound_sms(&pool, company_id, form.sender()).await;

                if let Some(agent) = form.agent.as_deref() {
                    if let Ok(Some(user_id)) =
                        get_user_id_by_ringcentral_agent(&pool, company_id, agent).await
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
                // 0 rows: INSERT IGNORE deduped a redelivered webhook — don't cancel,
                // move deals or notify again. Never log message text or phone numbers here.
                tracing::info!(
                    company_id,
                    rows_affected,
                    "Skipped sms flow enrollment cancel, deal move and telegram notify: deduped inbound sms delivery"
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

pub async fn call_received(
    headers: HeaderMap,
    _: RingCentralWebhookUser,
    State(pool): State<MySqlPool>,
    Path(company_id): Path<i32>,
    body: Bytes,
) -> Response {
    if let Some(response) = validation_token_response(&headers) {
        return response;
    }
    call_received_inner(pool, company_id, body).await.into_response()
}

async fn call_received_inner(
    pool: MySqlPool,
    company_id: i32,
    body: Bytes,
) -> BasicResponse {
    let payload: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => {
            tracing::error!(
                route = "call",
                category = ?error.classify(),
                line = error.line(),
                column = error.column(),
                "Error parsing ringcentral call payload"
            );
            return BAD_REQUEST;
        }
    };

    if let Some(check) = outbound_call_followup_check(&payload) {
        tracing::info!(
            company_id,
            call_id = check.call_id,
            talking_time = check.talking_time,
            "Enqueueing sms follow-up call check for outbound call"
        );
        spawn_sms_followup_call_check(SmsFollowupCallCheckBody {
            company_id,
            phone_digits: check.phone_digits,
            call_id: check.call_id,
            talking_time: check.talking_time,
            is_voicemail: check.is_voicemail,
            recording_link: check.recording_link,
        });
        return OK_RESPONSE;
    }

    let Some(phone_digits) = inbound_customer_phone_from_call_payload(&payload) else {
        tracing::info!(
            company_id,
            "Skipped sms flow cancel on call webhook: outgoing, internal, or no customer phone"
        );
        return OK_RESPONSE;
    };

    if let Err(error) = cancel_flow_enrollments_on_reply(&pool, company_id, phone_digits).await {
        tracing::error!(
            ?error,
            company_id,
            "Failed to cancel sms flow enrollments on inbound call"
        );
    }

    let last10 = phone_digits.to_string();
    match find_customer_id_by_phone_last10(&pool, company_id, &last10).await {
        Ok(Some(customer_id)) => {
            if let Err(error) =
                cancel_flow_enrollments_for_customer(&pool, company_id, customer_id).await
            {
                tracing::error!(
                    ?error,
                    company_id,
                    "Failed to cancel sms flow enrollments for customer on inbound call"
                );
            }
        }
        Ok(None) => {}
        Err(error) => {
            tracing::error!(
                ?error,
                company_id,
                "Failed to resolve customer for inbound call sms flow cancel"
            );
        }
    }

    OK_RESPONSE
}

pub async fn sms_sent(
    headers: HeaderMap,
    _: RingCentralWebhookUser,
    State(pool): State<MySqlPool>,
    Path(company_id): Path<i32>,
    body: Bytes,
) -> Response {
    if let Some(response) = validation_token_response(&headers) {
        return response;
    }
    sms_sent_inner(pool, company_id, body).await.into_response()
}

async fn sms_sent_inner(
    pool: MySqlPool,
    company_id: i32,
    body: Bytes,
) -> BasicResponse {
    let Some(form) = parse_ringcentral_sms(&body, "sent") else {
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
pub async fn sync_ringcentral(
    _: crate::axum_helpers::guards::RemixBackend,
    State(pool): State<MySqlPool>,
    Path((_company_id, customer_id)): Path<(i32, i32)>,
) -> BasicResponse {
    let client = Client::new();
    sync_customer_to_ring_central(&pool, &client, customer_id).await
}

#[cfg(test)]
mod tests {
    use super::parse_ringcentral_sms;
    use crate::axum_helpers::guards::CORRECT_ID;
    use crate::tests::ringcentral::{INBOUND_NULL_TEXT, INBOUND_SMS};
    use crate::tests::utils::{insert_group_list, new_test_app};
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
            "SELECT sender, recipient, text, agent, company_id FROM ringcentral_sms"
        )
        .fetch_all(pool)
        .await
        .unwrap()
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn test_basic_sms(pool: MySqlPool) {
        let app = new_test_app(pool.clone());

        let response = app
            .post("/ringcentral/sms/42")
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
    async fn test_echo_dedupe_via_ringcentral_id(pool: MySqlPool) {
        let app = new_test_app(pool.clone());

        let first = app
            .post("/ringcentral/sms/42")
            .authorization_bearer(CORRECT_ID.to_string())
            .json(&sms_with_id_json())
            .await;
        assert_eq!(first.status_code(), StatusCode::OK);

        let second = app
            .post("/ringcentral/sms/42")
            .authorization_bearer(CORRECT_ID.to_string())
            .json(&sms_with_id_json())
            .await;
        assert_eq!(second.status_code(), StatusCode::OK);

        let smss = get_sms_received(&pool).await;
        assert_eq!(smss.len(), 1, "duplicate ringcentral_id should be ignored");
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn test_sms_received_malformed_json(pool: MySqlPool) {
        let app = new_test_app(pool.clone());

        let response = app
            .post("/ringcentral/sms/42")
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
        let body: serde_json::Value = serde_json::from_slice(INBOUND_NULL_TEXT).expect("parse");

        let response = app
            .post("/ringcentral/sms/42")
            .authorization_bearer(CORRECT_ID.to_string())
            .json(&body)
            .await;
        assert_eq!(response.status_code(), StatusCode::OK);

        let smss = get_sms_received(&pool).await;
        assert_eq!(smss.len(), 1, "null-text inbound sms must not be dropped");
        assert_eq!(smss[0].text, String::new());
        assert_eq!(smss[0].sender, Some(6468956758));
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn test_sms_rejected_without_bearer_token(pool: MySqlPool) {
        let app = new_test_app(pool.clone());
        let response = app.post("/ringcentral/sms/42").json(&sms_json()).await;
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
            .post("/ringcentral/sms/sent/42")
            .authorization_bearer(CORRECT_ID.to_string())
            .json(&sms_with_id_json())
            .await;
        assert_eq!(response.status_code(), StatusCode::OK);

        let row = sqlx::query!(
            "SELECT COUNT(*) AS cnt FROM ringcentral_sms \
             WHERE direction = 'outbound' AND status = 'sent' AND ringcentral_id = 2200000000"
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
            "INSERT INTO ringcentral_sms \
                (ringcentral_id, sender, recipient, text, agent, company_id, direction, status) \
             VALUES (NULL, NULL, 3173161456, 'hello', '540273', 42, 'outbound', 'pending')"
        )
        .execute(&pool)
        .await
        .unwrap();

        let app = new_test_app(pool.clone());
        let response = app
            .post("/ringcentral/sms/sent/42")
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
            "SELECT ringcentral_id FROM ringcentral_sms WHERE company_id = 42 AND direction = 'outbound'"
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(merged.ringcentral_id, Some(2200000000));
    }

    // Fallback echo: CRM stored 'cap' but the sent body (caption + links) differs.
    // Tier-2 merge must absorb the echo, not duplicate the row.
    const MESSAGE_FALLBACK_ECHO: &[u8] = b"{\"id\":2200000099,\"sender\":\"+16468956758[sender]\",\"recipient\":\"+13173161456[recipient]\",\"text\":\"[text]cap\\nPhoto 1: https://x/y.jpg\",\"agent\":\"540273\"}";

    #[sqlx::test(migrations = "../migrations")]
    async fn test_sms_sent_merges_crm_row_with_differing_text(pool: MySqlPool) {
        let parent = sqlx::query!(
            "INSERT INTO ringcentral_sms \
                (ringcentral_id, sender, recipient, text, agent, company_id, direction, status) \
             VALUES (NULL, NULL, 3173161456, 'cap', '540273', 42, 'outbound', 'sent')"
        )
        .execute(&pool)
        .await
        .unwrap()
        .last_insert_id();

        // Tier 2 is gated on an attachment existing (it exists solely for image-send fallbacks);
        // seed one the same way the cascade-delete test in crud/ringcentral.rs does.
        sqlx::query!(
            "INSERT INTO ringcentral_sms_attachments \
                (ringcentral_sms_id, content_type, filename, s3_key, s3_url, width, height, position) \
             VALUES (?, 'image/jpeg', 'a.jpg', '42/u/a.jpg', 's3://gd-sms-attachments/42/u/a.jpg', 800, 600, 0)",
            i32::try_from(parent).expect("test id fits i32"),
        )
        .execute(&pool)
        .await
        .unwrap();

        let app = new_test_app(pool.clone());
        let body: serde_json::Value = serde_json::from_slice(MESSAGE_FALLBACK_ECHO).expect("parse");
        let response = app
            .post("/ringcentral/sms/sent/42")
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
            "SELECT ringcentral_id FROM ringcentral_sms WHERE company_id = 42 AND direction = 'outbound'"
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(merged.ringcentral_id, Some(2200000099));
    }

    const MESSAGE_SENT_NULL_TEXT: &[u8] = b"{\"id\":2200000123,\"sender\":\"+16468956758\",\"recipient\":\"+13173161456\",\"text\":null,\"agent\":\"540273\",\"media\":null,\"attachments\":null,\"media_urls\":null}";

    #[sqlx::test(migrations = "../migrations")]
    async fn test_sms_sent_null_text_merges_crm_row(pool: MySqlPool) {
        sqlx::query!(
            "INSERT INTO ringcentral_sms \
                (ringcentral_id, sender, recipient, text, agent, company_id, direction, status) \
             VALUES (NULL, NULL, 3173161456, '', '540273', 42, 'outbound', 'pending')"
        )
        .execute(&pool)
        .await
        .unwrap();

        let app = new_test_app(pool.clone());
        let body: serde_json::Value =
            serde_json::from_slice(MESSAGE_SENT_NULL_TEXT).expect("parse");
        let response = app
            .post("/ringcentral/sms/sent/42")
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
            "SELECT ringcentral_id FROM ringcentral_sms WHERE company_id = 42 AND direction = 'outbound'"
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(merged.ringcentral_id, Some(2200000123));
    }

    // A null recipient is genuinely unstorable, so it must still be rejected, but through the
    // handler's own logged parse branch rather than a silent extractor rejection.
    const MESSAGE_SENT_NULL_RECIPIENT: &[u8] =
        b"{\"id\":2200000124,\"sender\":\"+16468956758\",\"recipient\":null,\"text\":\"hi\",\"agent\":null}";

    #[test]
    #[traced_test]
    fn test_parse_failure_is_logged_with_position_only() {
        let body = Bytes::from_static(MESSAGE_SENT_NULL_RECIPIENT);

        assert!(parse_ringcentral_sms(&body, "sent").is_none());

        assert!(logs_contain("Error parsing ringcentral sms payload"));
        assert!(logs_contain("route=\"sent\""));
        assert!(!logs_contain("6468956758"), "payload must never be logged");
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn test_sms_sent_null_recipient_returns_bad_request(pool: MySqlPool) {
        let app = new_test_app(pool.clone());
        let body: serde_json::Value =
            serde_json::from_slice(MESSAGE_SENT_NULL_RECIPIENT).expect("parse");

        let response = app
            .post("/ringcentral/sms/sent/42")
            .authorization_bearer(CORRECT_ID.to_string())
            .json(&body)
            .await;
        assert_eq!(response.status_code(), StatusCode::BAD_REQUEST);

        let smss = get_sms_received(&pool).await;
        assert_eq!(smss.len(), 0, "unparseable payload must not insert a row");
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn inbound_sms_moves_deal_from_first_list(pool: MySqlPool) {
        let company = sqlx::query!(r#"INSERT INTO company (name) VALUES ('Sms Move Co')"#)
            .execute(&pool)
            .await
            .unwrap();
        let company_id = i32::try_from(company.last_insert_id()).unwrap();
        let group_id = insert_group_list(&pool, company_id).await.unwrap();
        let first = sqlx::query!(
            r#"INSERT INTO deals_list (name, group_id, position) VALUES ('Not Contacted Yet', ?, 0)"#,
            group_id
        )
        .execute(&pool)
        .await
        .unwrap();
        let first_list_id = i32::try_from(first.last_insert_id()).unwrap();
        let second = sqlx::query!(
            r#"INSERT INTO deals_list (name, group_id, position) VALUES ('Contacted', ?, 1)"#,
            group_id
        )
        .execute(&pool)
        .await
        .unwrap();
        let second_list_id = i32::try_from(second.last_insert_id()).unwrap();
        let customer = sqlx::query!(
            r#"INSERT INTO customers (name, company_id, phone, source) VALUES ('Lead', ?, '6468956758', 'leads')"#,
            company_id
        )
        .execute(&pool)
        .await
        .unwrap();
        let customer_id = i32::try_from(customer.last_insert_id()).unwrap();
        let deal = sqlx::query!(
            r#"INSERT INTO deals (customer_id, status, list_id, position) VALUES (?, 'Not Contacted Yet', ?, 0)"#,
            customer_id,
            first_list_id
        )
        .execute(&pool)
        .await
        .unwrap();
        let deal_id = deal.last_insert_id();

        let app = new_test_app(pool.clone());
        let response = app
            .post(&format!("/ringcentral/sms/{company_id}"))
            .authorization_bearer(CORRECT_ID.to_string())
            .json(&sms_json())
            .await;
        assert_eq!(response.status_code(), StatusCode::OK);

        let list_id = sqlx::query_scalar!(r#"SELECT list_id FROM deals WHERE id = ?"#, deal_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(list_id, second_list_id);
    }
}
