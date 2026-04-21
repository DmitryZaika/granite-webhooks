use crate::axum_helpers::guards::MarketingUser;
use crate::cloudtalk::schemas::CloudtalkSMS;
use crate::crud::cloudtalk::insert_cloudtalk_sms;
use crate::libs::constants::{ERR_DB, OK_RESPONSE, internal_error};
use crate::libs::types::BasicResponse;
use axum::extract::{Json, State};
use lambda_http::tracing;
use sqlx::MySqlPool;

pub async fn sms_received(
    _: MarketingUser,
    State(pool): State<MySqlPool>,
    Json(form): Json<CloudtalkSMS>,
) -> BasicResponse {
    match insert_cloudtalk_sms(&pool, &form).await {
        Ok(_) => OK_RESPONSE,
        Err(error) => {
            tracing::error!("Error inserting sms received into the database: {}", error);
            internal_error(ERR_DB)
        }
    }
}
#[cfg(test)]
mod tests {
    use crate::tests::utils::new_test_app;
    use axum::http::StatusCode;
    use sqlx::MySqlPool;
    const MESSAGE_1: &[u8] = b"{\"id\":null,\"sender\":\"+16468956758[sender]\",\"recipient\":\"+13173161456[recipient]\",\"text\":\"[text]\xd0\x9d\xd0\xb5 \xd0\xbf\xd0\xb8\xd1\x88\xd0\xb8 \xd1\x81\xd1\x8e\xd0\xb4\xd0\xb0\",\"agent\":\"540273\"}";

    fn sms_json() -> serde_json::Value {
        serde_json::from_slice(MESSAGE_1).expect("Failed to parse JSON")
    }

    struct CloudtalkReceivedSMS {
        pub sender: i64,
        pub recipient: i64,
        pub text: String,
        pub agent: Option<String>,
    }

    async fn get_sms_received(pool: &MySqlPool) -> Vec<CloudtalkReceivedSMS> {
        sqlx::query_as!(
            CloudtalkReceivedSMS,
            "SELECT sender, recipient, text, agent FROM cloudtalk_sms"
        )
        .fetch_all(pool)
        .await
        .unwrap()
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn test_basic_sms(pool: MySqlPool) {
        let app = new_test_app(pool.clone());

        let response = app.post("/cloudtalk/sms/1").json(&sms_json()).await;
        assert_eq!(response.status_code(), StatusCode::OK);

        let smss = get_sms_received(&pool).await;
        assert_eq!(smss.len(), 1);
        assert_eq!(smss[0].sender, 6468956758);
        assert_eq!(smss[0].recipient, 3173161456);
        assert_eq!(smss[0].text, "Не пиши сюда".to_string());
        assert_eq!(smss[0].agent, Some("540273".to_string()));
    }
}
