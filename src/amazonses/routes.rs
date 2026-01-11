use axum::extract::{Json, State};
use axum::http::StatusCode;
use lambda_http::tracing;
use sqlx::MySqlPool;

use crate::amazon::bucket::{CustomClient, S3Bucket};
use crate::amazonses::parse_email::parse_email;
use crate::amazonses::schemas::{S3Event, SesEvent};
use crate::amazonses::upload::upload_attachments;
use crate::crud::email::{create_email_read, create_email_with_attachments, get_prior_email};
use crate::libs::constants::{ACCEPTED_RESPONSE, BAD_REQUEST, OK_RESPONSE, internal_error};
use crate::libs::types::BasicResponse;

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

pub async fn process_ses_received_event<C: S3Bucket + Send + Sync + 'static>(
    pool: &MySqlPool,
    client: C,
    event: &S3Event,
) -> BasicResponse {
    let bucket = &event.detail.bucket.name;
    let key = &event.detail.object.key;

    let email_bytes = match client.read_bytes(bucket, key).await {
        Ok(bytes) => bytes,
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

    let (parsed, attachments) = match parse_email(&email_bytes) {
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
    let Some(message_id) = parsed.reply_message_id() else {
        tracing::error!(
            bucket = bucket,
            key = key,
            "Failed to extract message ID from email"
        );
        return ACCEPTED_RESPONSE;
    };
    let prior = match get_prior_email(pool, &message_id).await {
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
    let Some(clean_prior) = prior else {
        tracing::error!(bucket = bucket, key = key, "No prior email found");
        return (StatusCode::BAD_REQUEST, "No prior email found");
    };

    let uploaded_attachments = match upload_attachments(client, attachments).await {
        Ok(attachments) => attachments,
        Err(error) => {
            tracing::error!(
                ?error,
                bucket = bucket,
                key = key,
                "Failed to upload attachments"
            );
            return internal_error("Failed to upload attachments");
        }
    };
    let result =
        create_email_with_attachments(pool, &parsed, &clean_prior, &uploaded_attachments).await;
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

pub async fn receive_handler(
    State(pool): State<MySqlPool>,
    Json(event): Json<S3Event>,
) -> BasicResponse {
    let custom_client = CustomClient {};
    process_ses_received_event(&pool, custom_client, &event).await
}

#[cfg(test)]
mod local_tests {
    use super::*;
    use crate::libs::constants::ACCEPTED_RESPONSE;
    use crate::tests::data::ses_open_json::ses_open_event_json;
    use crate::tests::data::ses_received::ses_received_json;
    use crate::tests::utils::{new_test_app, read_file_as_bytes};
    use axum::http::StatusCode;
    use bytes::Bytes;
    use sqlx::MySqlPool;
    use sqlx::mysql::MySqlQueryResult;
    use std::path::PathBuf;
    use uuid::Uuid;

    struct ReadDb {
        message_id: String,
        user_agent: Option<String>,
        ip_address: Option<String>,
    }

    #[derive(Clone)]
    pub struct MockClient {
        pub path: PathBuf,
    }

    pub struct Email {
        pub sender_user_id: Option<i32>,
        pub subject: Option<String>,
        pub body: Option<String>,
        pub message_id: Option<String>,
        pub thread_id: Option<String>,
    }

    pub struct Attachment {
        content_type: String,
        content_subtype: Option<String>,
        filename: String,
        url: String,
    }

    impl MockClient {
        pub fn new<P: Into<PathBuf>>(path: P) -> Self {
            Self { path: path.into() }
        }
    }

    impl S3Bucket for MockClient {
        async fn read_bytes(&self, _bucket: &str, _key: &str) -> Result<Bytes, String> {
            read_file_as_bytes(&self.path).map_err(|e| e.to_string())
        }
        fn send_file<'a>(
            &'a self,
            bucket: &'a str,
            key: &'a str,
            data: Bytes,
        ) -> impl Future<Output = Result<String, String>> + Send + 'a {
            async move { Ok(format!("s3://{bucket}/{key}")) }
        }
    }

    async fn insert_email(
        pool: &MySqlPool,
        message_id: &str,
    ) -> Result<MySqlQueryResult, sqlx::Error> {
        let uuid: Uuid = Uuid::new_v4();
        sqlx::query!(
            r#"
            INSERT INTO emails (sender_user_id, subject, body, message_id, thread_id)
            VALUES (?, ?, ?, ?, ?)
            "#,
            1,
            "Test Subject",
            "Test Body",
            message_id,
            uuid.to_string()
        )
        .execute(pool)
        .await
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

    async fn get_emails(pool: &MySqlPool) -> Result<Vec<Email>, sqlx::Error> {
        sqlx::query_as!(
            Email,
            r#"
            SELECT sender_user_id, subject, body, message_id, thread_id
            FROM emails
            ORDER BY id ASC
            LIMIT 10
            "#,
        )
        .fetch_all(pool)
        .await
    }
    async fn get_email_attachments(
        pool: &MySqlPool,
        email_id: u64,
    ) -> Result<Vec<Attachment>, sqlx::Error> {
        sqlx::query_as!(
            Attachment,
            r#"
            SELECT content_type, content_subtype, filename, url
            FROM email_attachments
            WHERE email_id = ?
            ORDER BY id ASC
            "#,
            email_id
        )
        .fetch_all(pool)
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
        let message_id = "010f019ab18dd4f1-e4d8dbab-6e05-466a-9cdb-5c9ccde5f3de-000000";

        insert_email(&pool, message_id).await.unwrap();

        let mock_client = MockClient::new("src/tests/data/reply_email1.eml");

        let data: S3Event = ses_received_json();
        let response = process_ses_received_event(&pool, mock_client, &data).await;

        assert_eq!(response, OK_RESPONSE);

        // TODO: Check that correct email was added into the db

        let result = get_emails(&pool).await.unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[1].subject, Some("Re: COLINS TEST".to_string()));
        const EMAIL_BODY: &str = "Please respond.";
        assert_eq!(result[1].body.clone().unwrap(), EMAIL_BODY);
        assert_eq!(result[1].sender_user_id, None);
        assert_eq!(
            result[1].thread_id.clone().unwrap(),
            result[0].thread_id.clone().unwrap()
        );
        const MESSAGE_ID: &str =
            "CAG6QthbVR6eOBoEFup=bnuuBw=_JQWfP1rLzAjwDUGCpNV_wyg@mail.gmail.com";
        assert_eq!(result[1].message_id, Some(MESSAGE_ID.to_string()));
    }

    #[sqlx::test]
    async fn test_ses_received_accepted(pool: MySqlPool) {
        let message_id = "010f019ab18dd4f1-e4d8dbab-6e05-466a-9cdb-5c9ccde5f3de-000000";

        insert_email(&pool, message_id).await.unwrap();

        let mock_client = MockClient::new("src/tests/data/external1.eml");

        let data: S3Event = ses_received_json();
        let response = process_ses_received_event(&pool, mock_client, &data).await;

        assert_eq!(response, ACCEPTED_RESPONSE);

        let result = get_emails(&pool).await.unwrap();
        assert_eq!(result.len(), 1);
    }

    #[sqlx::test]
    async fn test_ses_response_to_received_success(pool: MySqlPool) {
        let message_id = "010f019b278e838b-4026f591-7b73-451a-a540-7e70c8bd5c84-000000";

        insert_email(&pool, message_id).await.unwrap();

        let response1 = MockClient::new("src/tests/data/reply_attachment_1.eml");
        let response2 = MockClient::new("src/tests/data/reply_attachment_2.eml");

        let data: S3Event = ses_received_json();
        let answer1 = process_ses_received_event(&pool, response1, &data).await;
        let answer2 = process_ses_received_event(&pool, response2, &data).await;

        assert_eq!(answer1, OK_RESPONSE);
        assert_eq!(answer2, OK_RESPONSE);

        let result = get_emails(&pool).await.unwrap();
        assert_eq!(
            result[0].thread_id.clone().unwrap(),
            result[1].thread_id.clone().unwrap()
        );
        assert_eq!(
            result[1].thread_id.clone().unwrap(),
            result[2].thread_id.clone().unwrap()
        );
    }
    #[sqlx::test]
    async fn test_ses_received_no_sent(pool: MySqlPool) {
        let mock_client = MockClient::new("src/tests/data/reply_email1.eml");

        let data: S3Event = ses_received_json();
        let response = process_ses_received_event(&pool, mock_client, &data).await;

        const BAD: BasicResponse = (StatusCode::BAD_REQUEST, "No prior email found");
        assert_eq!(response, BAD);
    }
    #[sqlx::test]
    async fn test_see_four_attachments(pool: MySqlPool) {
        let message_id = "CAG6QthaOtf0GWH6Ba9eOfRkfbviRi-RJw_vVnRc4U5cW_9GPmA@mail.gmail.com";

        let email_result = insert_email(&pool, message_id).await.unwrap();

        let response1 = MockClient::new("src/tests/data/reply_attachment_2.eml");

        let data: S3Event = ses_received_json();
        let answer1 = process_ses_received_event(&pool, response1, &data).await;

        assert_eq!(answer1, OK_RESPONSE);

        let wrong_result = get_email_attachments(&pool, email_result.last_insert_id())
            .await
            .unwrap();
        assert_eq!(wrong_result.len(), 0);
        let result = get_email_attachments(&pool, email_result.last_insert_id() + 1)
            .await
            .unwrap();
        assert_eq!(result.len(), 4);

        let expected = [
            ("image", "png", "img_0.png"),
            ("image", "jpeg", "img_1.jpg"),
            ("image", "png", "img_1.png"),
            ("image", "jpeg", "img_0.jpg"),
        ];
        for (attachment, (content_type, content_subtype, filename)) in result.iter().zip(expected) {
            assert_eq!(attachment.content_type, content_type);
            assert_eq!(
                attachment.content_subtype.as_ref().unwrap(),
                content_subtype
            );
            assert_eq!(attachment.filename, filename);
            assert!(attachment.url.starts_with("s3://"));
            let extension = attachment.url.split('.').last().unwrap();
            assert_eq!(extension, filename.split('.').last().unwrap());
        }
    }
}
