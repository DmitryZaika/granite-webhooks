use crate::axum_helpers::guards::MarketingUser;
use crate::cloudtalk::schemas::CloudtalkSMS;
use crate::libs::types::BasicResponse;
use axum::extract::{Json, State};
use axum::http::StatusCode;
use sqlx::MySqlPool;

pub async fn sms_received(
    _: MarketingUser,
    State(_pool): State<MySqlPool>,
    Json(_form): Json<CloudtalkSMS>,
) -> BasicResponse {
    (StatusCode::NOT_IMPLEMENTED, "")
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::utils::new_test_app;
    use sqlx::MySqlPool;

    fn sms_json() -> serde_json::Value {
        serde_json::json!({
            "id": 1,
            "sender": "1234567890",
            "recipient": "0987654321",
            "text": "Hello, world!",
            "agent": "CloudTalk"
        })
    }

    #[sqlx::test]
    async fn test_basic_sms(pool: MySqlPool) {
        let app = new_test_app(pool.clone());

        let response = app.post("/cloudtalk/sms").json(&sms_json()).await;
        assert_eq!(response.status_code(), StatusCode::NOT_IMPLEMENTED);
    }
}
