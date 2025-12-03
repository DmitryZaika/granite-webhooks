use axum::extract::{Json, State};
use lambda_http::tracing;
use sqlx::MySqlPool;

use crate::amazonses::schemas::{S3Event, SesEvent};
use crate::crud::email::{create_email, create_email_read, get_prior_email};
use crate::libs::constants::{BAD_REQUEST, OK_RESPONSE, internal_error};
use crate::libs::types::BasicResponse;
use aws_sdk_s3::Client;

use crate::amazonses::parse_email::parse_email;

pub async fn read_receipt_handler(
    State(pool): State<MySqlPool>,
    Json(info): Json<SesEvent>,
) -> BasicResponse {
    let message_id = info.detail.mail.message_id;
    let user_agent = info.detail.open.user_agent;
    let ip_address = info.detail.open.ip_address;

    let result = create_email_read(&pool, &message_id, &user_agent, &ip_address).await;
    if let Err(error) = result {
        tracing::error!(
            "Error inserting email read: {} into the db: {}",
            message_id,
            error
        );
        return BAD_REQUEST;
    }
    OK_RESPONSE
}

pub async fn receive_handler(
    State(pool): State<MySqlPool>,
    Json(event): Json<S3Event>,
) -> BasicResponse {
    let config = aws_config::load_from_env().await;
    let client = Client::new(&config);

    let bucket = &event.detail.bucket.name;
    let key = &event.detail.object.key;

    let get_object_output = match client.get_object().bucket(bucket).key(key).send().await {
        Ok(output) => output,
        Err(error) => {
            tracing::error!(
                ?error,
                bucket = bucket,
                key = key,
                "Failed to retrieve email from S3"
            );
            return internal_error("Unable to retrieve email from S3");
        }
    };

    let email_bytes = match get_object_output.body.collect().await {
        Ok(bytes) => bytes.into_bytes(),
        Err(error) => {
            tracing::error!(
                ?error,
                bucket = bucket,
                key = key,
                "Failed to read email content from S3"
            );
            return internal_error("Unable to read email content from S3");
        }
    };
    let parsed = match parse_email(&email_bytes) {
        Ok(email) => email,
        Err(error) => {
            tracing::error!(
                ?error,
                bucket = bucket,
                key = key,
                "Failed to parse email content from S3"
            );
            return internal_error("Unable to parse email content from S3");
        }
    };
    let message_id = match parsed.message_id() {
        Some(id) => id,
        None => {
            tracing::error!(
                bucket = bucket,
                key = key,
                "Failed to extract message ID from email"
            );
            return internal_error("Unable to extract message ID from email");
        }
    };
    let prior = match get_prior_email(&pool, &message_id).await {
        Ok(email) => email,
        Err(error) => {
            tracing::error!(
                ?error,
                bucket = bucket,
                key = key,
                "Failed to retrieve prior email"
            );
            return internal_error("Unable to retrieve prior email");
        }
    };
    let clean_prior = match prior {
        Some(email) => email,
        None => {
            tracing::error!(bucket = bucket, key = key, "No prior email found");
            return internal_error("No prior email found");
        }
    };
    let result = create_email(&pool, &parsed, &clean_prior).await;
    if let Err(error) = result {
        tracing::error!(
            "Error inserting email: {} into the db: {}",
            message_id,
            error
        );
        return internal_error("Failed to insert email into the database");
    }

    OK_RESPONSE
}

#[cfg(test)]
mod local_tests {
    use crate::axum_helpers::axum_app::new_main_app;
    use crate::tests::data::ses_open_json::ses_open_event_json;
    use crate::tests::data::ses_received::ses_received_json;
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
            INSERT INTO emails (sender_user_id, subject, body, message_id)
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

    #[sqlx::test]
    async fn test_ses_received_success(pool: MySqlPool) {
        let app = new_test_app(pool.clone());
        let message_id = "010f019ab18dd4f1-e4d8dbab-6e05-466a-9cdb-5c9ccde5f3de-000000";

        insert_email(&pool, message_id).await.unwrap();

        let response = app
            .post("/ses/read-receipt")
            .json(&ses_received_json())
            .await;

        assert_eq!(response.status_code(), StatusCode::OK);

        let result = check_db_email_reads(&pool).await.unwrap();
    }
}
