use crate::axum_helpers::guards::CloudTalkWebhookUser;
use crate::cloudtalk::schemas::CloudtalkSMS;
use crate::crud::cloudtalk::{insert_inbound_sms, insert_outbound_sms};
use crate::libs::constants::{ERR_DB, OK_RESPONSE, internal_error};
use crate::libs::types::BasicResponse;
use axum::extract::{Json, Path, State};
use lambda_http::tracing;
use sqlx::MySqlPool;

pub async fn sms_received(
    _: CloudTalkWebhookUser,
    State(pool): State<MySqlPool>,
    Path(company_id): Path<i32>,
    Json(form): Json<CloudtalkSMS>,
) -> BasicResponse {
    match insert_inbound_sms(&pool, &form, company_id).await {
        Ok(_) => OK_RESPONSE,
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
    Json(form): Json<CloudtalkSMS>,
) -> BasicResponse {
    match insert_outbound_sms(&pool, &form, company_id).await {
        Ok(_) => OK_RESPONSE,
        Err(error) => {
            tracing::error!("Error inserting sms sent into the database: {}", error);
            internal_error(ERR_DB)
        }
    }
}
#[cfg(test)]
mod tests {
    use crate::axum_helpers::guards::CORRECT_ID;
    use crate::tests::utils::new_test_app;
    use axum::http::StatusCode;
    use sqlx::MySqlPool;
    const MESSAGE_1: &[u8] = b"{\"id\":null,\"sender\":\"+16468956758[sender]\",\"recipient\":\"+13173161456[recipient]\",\"text\":\"[text]\xd0\x9d\xd0\xb5 \xd0\xbf\xd0\xb8\xd1\x88\xd0\xb8 \xd1\x81\xd1\x8e\xd0\xb4\xd0\xb0\",\"agent\":\"540273\"}";

    fn sms_json() -> serde_json::Value {
        serde_json::from_slice(MESSAGE_1).expect("Failed to parse JSON")
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
    async fn test_sms_rejected_without_bearer_token(pool: MySqlPool) {
        let app = new_test_app(pool.clone());
        let response = app.post("/cloudtalk/sms/42").json(&sms_json()).await;
        assert_eq!(response.status_code(), StatusCode::FORBIDDEN);
        let smss = get_sms_received(&pool).await;
        assert_eq!(smss.len(), 0, "unauthenticated webhook must not insert a row");
    }

    // App-originated send (no CRM row to merge) → a fresh outbound row is inserted.
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
        assert_eq!(row.cnt, 1, "app-originated send should insert one outbound row");
    }

    // CRM-originated send: the CRM already inserted an outbound row with a NULL
    // cloudtalk_id; the CloudTalk echo should merge into it, not duplicate.
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
        assert_eq!(smss.len(), 1, "echo must merge into the CRM row, not duplicate");

        let merged = sqlx::query!(
            "SELECT cloudtalk_id FROM cloudtalk_sms WHERE company_id = 42 AND direction = 'outbound'"
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(merged.cloudtalk_id, Some(2200000000));
    }
}
