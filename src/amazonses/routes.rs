use axum::extract::{Json, State};
use lambda_http::tracing;
use sqlx::MySqlPool;

use crate::amazonses::schemas::SesEvent;
use crate::libs::constants::{BAD_REQUEST, OK_RESPONSE};
use crate::libs::types::BasicResponse;

pub async fn read_receipt_handler(
    State(pool): State<MySqlPool>,
    Json(info): Json<SesEvent>,
) -> BasicResponse {
    let message_id = info.detail.mail.message_id;
    let user_agent = info.detail.open.user_agent;
    let ip_address = info.detail.open.ip_address;
    let result = sqlx::query!(
        r#"
        INSERT INTO email_reads (message_id, user_agent, ip_address)
        VALUES (?, ?, ?)
        "#,
        message_id,
        user_agent,
        ip_address,
    )
    .execute(&pool)
    .await;

    if let Err(error) = result {
        tracing::error!(
            "Error inserting email: {} into the db: {}",
            message_id,
            error
        );
        return BAD_REQUEST;
    }
    OK_RESPONSE
}

#[cfg(test)]
mod local_tests {
    use crate::axum_helpers::axum_app::new_main_app;
    use crate::tests::data::ses_open_json::ses_open_event_json;
    use axum::http::StatusCode;
    use axum_test::TestServer;
    use sqlx::MySqlPool;

    struct ReadDb {
        message_id: String,
        user_agent: Option<String>,
        ip_address: Option<String>,
    }

    fn new_test_app(pool: MySqlPool) -> TestServer {
        let app = new_main_app(pool);
        TestServer::builder().build(app).unwrap()
    }

    async fn insert_email(pool: &MySqlPool, message_id: &str) -> Result<(), sqlx::Error> {
        sqlx::query!(
            r#"
            INSERT INTO emails (user_id, subject, body, message_id)
            VALUES (?, ?, ?, ?)
            "#,
            1,
            "Test Subject",
            "Test Body",
            message_id
        )
        .execute(pool)
        .await?;
        Ok(())
    }

    async fn check_db_email_reads(pool: &MySqlPool) -> Result<ReadDb, sqlx::Error> {
        sqlx::query_as!(
            ReadDb,
            r#"
            SELECT message_id, user_agent, ip_address
            FROM email_reads
            ORDER BY id DESC
            LIMIT 1
            "#,
        )
        .fetch_one(pool)
        .await
    }

    #[sqlx::test]
    async fn test_ses_open_event_success(pool: MySqlPool) {
        let app = new_test_app(pool.clone());

        let expected_message_id = "010f019a9974b389-60efe038-3845-92e7-45c43cdc6ca2-000000";
        let expected_user_agent = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/42.0.2311.135 Safari/537.36 Edge/12.246 Mozilla/5.0";
        let expected_ip = "108.177.2.32";

        insert_email(&pool, expected_message_id).await.unwrap();

        let response = app
            .post("/ses/read-receipt")
            .json(&ses_open_event_json())
            .await;

        assert_eq!(response.status_code(), StatusCode::OK);

        let result = check_db_email_reads(&pool).await.unwrap();

        assert_eq!(result.message_id, expected_message_id);
        assert_eq!(result.user_agent.unwrap(), expected_user_agent);
        assert_eq!(result.ip_address.unwrap(), expected_ip);
    }
}
