use crate::axum_helpers::access_token::Transitional;
use crate::axum_helpers::guards::RemixBackend;
use crate::axum_helpers::guards::{RingCentralNotificationUser, RingCentralWebhookUser};
use crate::crud::deals::{
    AUTO_CONTACTED_MIN_TALKING_SECONDS, find_customer_id_by_phone_last10,
    maybe_move_deal_on_inbound_call, maybe_move_deal_on_inbound_sms,
    maybe_move_deal_on_outbound_call,
};
use crate::crud::ringcentral::{
    cancel_flow_enrollments_for_customer, cancel_flow_enrollments_on_reply, insert_inbound_sms,
    insert_outbound_sms,
};
use crate::libs::app_request::{SmsFollowupCallCheckBody, spawn_sms_followup_call_check};
use crate::libs::constants::{BAD_REQUEST, ERR_DB, OK_RESPONSE, internal_error};
use crate::libs::types::BasicResponse;
use crate::ringcentral::api::sync_customer_to_ring_central;
use crate::ringcentral::media::store_inbound_media;
use crate::ringcentral::schemas::{
    NotifiedSms, RingcentralSMS, RingcentralSmsNotification,
    inbound_customer_phone_from_call_payload, outbound_call_followup_check,
};
use axum::body::{Body, Bytes};
use axum::extract::Request;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use lambda_http::tracing;
use sqlx::MySqlPool;

/// Answers `RingCentral`'s subscription handshake.
///
/// A request with a `Validation-Token` gets it back (200) before any auth or
/// handler runs, so the webhook address can be verified. Layered on the
/// routes `RingCentral` calls.
pub async fn handshake(request: Request, next: Next) -> Response {
    let Some(token) = request.headers().get("Validation-Token").cloned() else {
        return next.run(request).await;
    };
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert("Validation-Token", token);
    // RingCentral rejects the handshake (SUB-525) without a JSON content type.
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
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
    _: Transitional<RingCentralWebhookUser>,
    State(pool): State<MySqlPool>,
    Path(company_id): Path<i32>,
    body: Bytes,
) -> Response {
    sms_received_inner(pool, company_id, body)
        .await
        .into_response()
}

async fn sms_received_inner(pool: MySqlPool, company_id: i32, body: Bytes) -> BasicResponse {
    let Some(form) = parse_ringcentral_sms(&body, "received") else {
        return BAD_REQUEST;
    };
    store_inbound_sms(&pool, company_id, &form).await
}

async fn store_inbound_sms(
    pool: &MySqlPool,
    company_id: i32,
    form: &RingcentralSMS,
) -> BasicResponse {
    match insert_inbound_sms(pool, form, company_id).await {
        Ok(result) => {
            let rows_affected = result.rows_affected();
            if rows_affected > 0 {
                if let Some(message_id) = form.id {
                    store_inbound_media(
                        company_id,
                        result.last_insert_id(),
                        message_id,
                        form.agent.as_deref(),
                        &form.media,
                    )
                    .await;
                }
                if let Err(error) =
                    cancel_flow_enrollments_on_reply(pool, company_id, form.sender()).await
                {
                    tracing::error!(
                        ?error,
                        company_id,
                        "Failed to cancel sms flow enrollments on reply"
                    );
                }

                maybe_move_deal_on_inbound_sms(pool, company_id, form.sender()).await;
            } else {
                // 0 rows: INSERT IGNORE deduped a redelivered webhook — don't cancel
                // or move deals again. Never log message text or phone numbers here.
                tracing::info!(
                    company_id,
                    rows_affected,
                    "Skipped sms flow enrollment cancel and deal move: deduped inbound sms delivery"
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
    _: Transitional<RingCentralWebhookUser>,
    State(pool): State<MySqlPool>,
    Path(company_id): Path<i32>,
    body: Bytes,
) -> Response {
    call_received_inner(pool, company_id, body)
        .await
        .into_response()
}

async fn call_received_inner(pool: MySqlPool, company_id: i32, body: Bytes) -> BasicResponse {
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
        if check.talking_time > AUTO_CONTACTED_MIN_TALKING_SECONDS {
            maybe_move_deal_on_outbound_call(&pool, company_id, check.phone_digits).await;
        }
        spawn_sms_followup_call_check(
            pool.clone(),
            SmsFollowupCallCheckBody {
                company_id,
                phone_digits: check.phone_digits,
                call_id: check.call_id,
                talking_time: check.talking_time,
                is_voicemail: check.is_voicemail,
                recording_link: check.recording_link,
            },
        );
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

    maybe_move_deal_on_inbound_call(&pool, company_id, phone_digits).await;

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
    _: Transitional<RingCentralWebhookUser>,
    State(pool): State<MySqlPool>,
    Path(company_id): Path<i32>,
    body: Bytes,
) -> Response {
    sms_sent_inner(pool, company_id, body).await.into_response()
}

async fn sms_sent_inner(pool: MySqlPool, company_id: i32, body: Bytes) -> BasicResponse {
    let Some(form) = parse_ringcentral_sms(&body, "sent") else {
        return BAD_REQUEST;
    };
    store_outbound_sms(&pool, company_id, &form).await
}

async fn store_outbound_sms(
    pool: &MySqlPool,
    company_id: i32,
    form: &RingcentralSMS,
) -> BasicResponse {
    match insert_outbound_sms(pool, form, company_id).await {
        Ok(result) => {
            // A new row is a text sent from the RingCentral app: keep its pictures too.
            // A merged CRM send (UPDATE, no insert id) already has its attachments.
            if result.rows_affected() > 0
                && result.last_insert_id() > 0
                && let Some(message_id) = form.id
            {
                store_inbound_media(
                    company_id,
                    result.last_insert_id(),
                    message_id,
                    form.agent.as_deref(),
                    &form.media,
                )
                .await;
            }
            OK_RESPONSE
        }
        // Two pushes for the same text raced; the other one stored it.
        Err(sqlx::Error::Database(error)) if error.is_unique_violation() => OK_RESPONSE,
        Err(error) => {
            tracing::error!("Error inserting sms sent into the database: {}", error);
            internal_error(ERR_DB)
        }
    }
}

/// How far back a `message-store` push looks for texts sent from the
/// `RingCentral` app; ones already stored are skipped.
const SENT_LOOKBACK_MINUTES: i64 = 10;

/// Texts `extension` sent lately, stored like the CRM's own sends (a text the
/// CRM sent itself is already there under its `RingCentral` id).
async fn store_recent_outbound(
    pool: &MySqlPool,
    company_id: i32,
    extension: &str,
) -> BasicResponse {
    let since = (chrono::Utc::now() - chrono::Duration::minutes(SENT_LOOKBACK_MINUTES))
        .format("%Y-%m-%dT%H:%M:%S%.3fZ");
    let path = format!(
        "account/~/extension/{extension}/message-store?messageType=SMS&direction=Outbound&dateFrom={since}&perPage=100"
    );
    let client = common::http::client();
    let messages = match common::telephony::ringcentral::request(
        pool,
        &client,
        company_id,
        reqwest::Method::GET,
        &path,
        &common::telephony::ringcentral::Body::Empty,
        None,
    )
    .await
    {
        Ok(messages) => messages,
        Err(error) => {
            tracing::error!(
                company_id,
                status = error.status,
                "Could not load sent ringcentral texts"
            );
            // A 200 still: RingCentral would only retry the same push.
            return OK_RESPONSE;
        }
    };
    let records = messages["records"].as_array().cloned().unwrap_or_default();
    for record in records {
        if let Some(NotifiedSms::Outbound(sms)) =
            RingcentralSmsNotification::from_record(extension, record)
        {
            store_outbound_sms(pool, company_id, &sms).await;
        }
    }
    OK_RESPONSE
}

/// `RingCentral`'s own webhook, registered by the CRM when a company connects:
/// `message-store/instant?type=SMS` brings inbound texts, `message-store` says
/// a text was sent (from the CRM or the `RingCentral` app). Same storage as
/// the `/ringcentral/sms` routes.
pub async fn sms_notification(
    _: Transitional<RingCentralNotificationUser>,
    State(pool): State<MySqlPool>,
    Path(company_id): Path<i32>,
    body: Bytes,
) -> Response {
    let notification = match serde_json::from_slice::<RingcentralSmsNotification>(&body) {
        Ok(notification) => notification,
        Err(error) => {
            tracing::error!(
                route = "notify",
                category = ?error.classify(),
                line = error.line(),
                column = error.column(),
                "Error parsing ringcentral notification payload"
            );
            return BAD_REQUEST.into_response();
        }
    };
    if let Some(extension) = notification.new_sms_extension() {
        return store_recent_outbound(&pool, company_id, &extension)
            .await
            .into_response();
    }
    match notification.into_sms() {
        Some(NotifiedSms::Inbound(sms)) => store_inbound_sms(&pool, company_id, &sms).await,
        Some(NotifiedSms::Outbound(sms)) => store_outbound_sms(&pool, company_id, &sms).await,
        None => {
            tracing::info!(
                company_id,
                "Skipped ringcentral notification: not a one-to-one SMS"
            );
            OK_RESPONSE
        }
    }
    .into_response()
}

pub async fn sync_ringcentral(
    _: Transitional<RemixBackend>,
    State(pool): State<MySqlPool>,
    Path((_company_id, customer_id)): Path<(i32, i32)>,
) -> BasicResponse {
    let client = common::http::client();
    sync_customer_to_ring_central(&pool, &client, customer_id).await
}

/// `DELETE /ringcentral/sync/{company_id}/{customer_id}`: drop the contact of
/// a deleted customer.
pub async fn unsync_ringcentral(
    _: Transitional<RemixBackend>,
    State(pool): State<MySqlPool>,
    Path((_company_id, customer_id)): Path<(i32, i32)>,
) -> BasicResponse {
    let client = common::http::client();
    crate::ringcentral::api::delete_customer_from_ring_central(&pool, &client, customer_id).await
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

    #[test_db::test]
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

    #[test_db::test]
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

    #[test_db::test]
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

    #[test_db::test]
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

    #[test_db::test]
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

    #[test_db::test]
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

    #[test_db::test]
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

    #[test_db::test]
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

        // Tier 2 is gated on an attachment existing (it exists solely for attachment-link
        // fallbacks); seed one the same way the cascade-delete test in crud/ringcentral.rs does.
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

    #[test_db::test]
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

    #[test_db::test]
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

    #[test_db::test]
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

    #[test_db::test]
    async fn inbound_call_moves_deal_from_first_list(pool: MySqlPool) {
        let company = sqlx::query!(r#"INSERT INTO company (name) VALUES ('Call Move Co')"#)
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
            r#"INSERT INTO customers (name, company_id, phone, source) VALUES ('Lead', ?, '5551234567', 'leads')"#,
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
            .post(&format!("/ringcentral/call/{company_id}"))
            .authorization_bearer(CORRECT_ID.to_string())
            .json(&serde_json::json!({
                "external_number": "+15551234567",
                "type": "incoming"
            }))
            .await;
        assert_eq!(response.status_code(), StatusCode::OK);

        let list_id = sqlx::query_scalar!(r#"SELECT list_id FROM deals WHERE id = ?"#, deal_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(list_id, second_list_id);
    }

    #[test_db::test]
    async fn test_notify_stores_inbound_sms(pool: MySqlPool) {
        let app = new_test_app(pool.clone());
        let body: serde_json::Value =
            serde_json::from_slice(crate::tests::ringcentral::NOTIFY_INBOUND_SMS).unwrap();

        let response = app
            .post("/ringcentral/notify/42")
            .add_header("Verification-Token", CORRECT_ID.to_string())
            .json(&body)
            .await;
        assert_eq!(response.status_code(), StatusCode::OK);

        let smss = get_sms_received(&pool).await;
        assert_eq!(smss.len(), 1);
        assert_eq!(smss[0].sender, Some(6468956758));
        assert_eq!(smss[0].recipient, 3173161456);
        assert_eq!(smss[0].text, "Is the slab still available?".to_string());
        assert_eq!(smss[0].agent, Some("540273".to_string()));
        assert_eq!(smss[0].company_id, Some(42));

        // Redelivery of the same RingCentral message id is deduped.
        let again = app
            .post("/ringcentral/notify/42")
            .add_header("Verification-Token", CORRECT_ID.to_string())
            .json(&body)
            .await;
        assert_eq!(again.status_code(), StatusCode::OK);
        assert_eq!(get_sms_received(&pool).await.len(), 1);
    }

    #[test_db::test]
    async fn test_notify_rejects_missing_token_and_echoes_validation(pool: MySqlPool) {
        let app = new_test_app(pool.clone());
        let body: serde_json::Value =
            serde_json::from_slice(crate::tests::ringcentral::NOTIFY_INBOUND_SMS).unwrap();

        let forbidden = app.post("/ringcentral/notify/42").json(&body).await;
        assert_eq!(forbidden.status_code(), StatusCode::FORBIDDEN);

        let handshake = app
            .post("/ringcentral/notify/42")
            .add_header("Validation-Token", "abc-123")
            .await;
        assert_eq!(handshake.status_code(), StatusCode::OK);
        assert_eq!(handshake.header("Validation-Token"), "abc-123");
        assert!(get_sms_received(&pool).await.is_empty());
    }
}
