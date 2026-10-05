use axum::extract::{Json, State};
use axum::http::header::RETRY_AFTER;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use lambda_http::tracing;
use sqlx::MySqlPool;
use uuid::Uuid;

use crate::amazon::bucket::{CustomClient, S3Bucket};
use crate::amazonses::parse_email::parse_email;
use crate::amazonses::process::{EmailInfo, process_reply_email};
use crate::amazonses::schemas::{S3Event, SesEvent};
use crate::amazonses::unprocessed::{PostHogUnprocessedEmailReporter, UnprocessedEmailReporter};
use crate::crud::email::{create_email_read, email_exists, get_full_message_id};
use crate::crud::email_claims::{EmailClaim, claim_email_processing, release_email_processing};
use crate::libs::constants::{BAD_REQUEST, OK_RESPONSE, internal_error};
use crate::libs::types::BasicResponse;

fn is_missing_s3_object(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("nosuchkey")
        || lower.contains("the specified key does not exist")
        || lower.contains("nosuchbucket")
        || lower.contains("the specified bucket does not exist")
}

/// Another invocation is processing this email right now. `EventBridge` API
/// destinations retry a 429 later; by then the email is stored (the retry is
/// a no-op) or the claim has expired (the retry takes over). Not a 5xx, so the
/// request logger does not report it to `PostHog` as an exception.
const EMAIL_IN_PROGRESS: BasicResponse = (
    StatusCode::TOO_MANY_REQUESTS,
    "Email is being processed by another invocation",
);

/// Seconds a turned-away delivery is asked to wait (`Retry-After`).
const EMAIL_IN_PROGRESS_RETRY_AFTER: &str = "30";

/// A fixed, personal-data-free name for each `parse_email` failure.
fn parse_error_kind(error: &str) -> &'static str {
    match error {
        "Failed to parse email" => "unreadable_message",
        "Failed to parse message ID" => "missing_message_id",
        "Failed to parse sender email" => "missing_sender",
        "Failed to parse receiver email" => "missing_receiver",
        _ => "other",
    }
}

pub async fn read_receipt_handler(
    State(pool): State<MySqlPool>,
    Json(info): Json<SesEvent>,
) -> BasicResponse {
    let message_id = info.detail.mail.message_id;
    let user_agent = info.detail.open.user_agent;
    let ip_address = info.detail.open.ip_address;

    let final_message_id = match get_full_message_id(&pool, &message_id).await {
        Ok(Some(message_id)) => message_id,
        Ok(None) => return OK_RESPONSE,
        Err(error) => {
            tracing::error!(
                "Error fetching email read: {} from the db: {}",
                message_id,
                error
            );
            return BAD_REQUEST;
        }
    };
    let result = create_email_read(&pool, &final_message_id, &user_agent, &ip_address).await;
    if let Err(error) = result {
        tracing::error!(
            "Error inserting email read: {} into the db: {}",
            final_message_id,
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
    process_ses_received_event_with(pool, client, event, &PostHogUnprocessedEmailReporter).await
}

pub async fn process_ses_received_event_with<C, R>(
    pool: &MySqlPool,
    client: C,
    event: &S3Event,
    reporter: &R,
) -> BasicResponse
where
    C: S3Bucket + Send + Sync + 'static,
    R: UnprocessedEmailReporter,
{
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
            if is_missing_s3_object(&error) {
                return OK_RESPONSE;
            }
            return internal_error("Unable to read email content from S3");
        }
    };

    let (parsed, attachments) = match parse_email(&email_bytes) {
        Ok(email) => email,
        Err(error) => {
            // No retry can fix a parse error, so answer 200 and stop the
            // retries. The raw email stays in the bucket; the event names it.
            let error_kind = parse_error_kind(&error);
            tracing::error!(
                error_kind,
                bucket = bucket,
                key = key,
                "Inbound email cannot be parsed; not processed"
            );
            reporter.report(bucket, key, error_kind).await;
            return OK_RESPONSE;
        }
    };
    // EventBridge redelivers an email when the first run takes longer than its
    // 5 s timeout, even if that run stored it. A redelivery must repeat no side
    // effect: no attachment upload, no deal move, no Telegram message.
    match email_exists(pool, &parsed.message_id).await {
        Ok(true) => {
            tracing::info!(
                bucket = bucket,
                key = key,
                "Inbound email already stored; skipping redelivery"
            );
            return OK_RESPONSE;
        }
        Ok(false) => {}
        Err(error) => {
            tracing::warn!(
                ?error,
                bucket = bucket,
                key = key,
                "Failed to check for an already stored email"
            );
        }
    }
    // `email_exists` misses a delivery that overlaps a run still in progress
    // (the slowest emails). The claim turns that overlap away before any
    // attachment work; the overlapping call gets 429 (retry later), not 200,
    // because the running invocation may still be killed by the Lambda timeout.
    let token = Uuid::new_v4().to_string();
    let claimed = match claim_email_processing(pool, &parsed.message_id, &token).await {
        EmailClaim::Acquired => {
            // Another run may have stored the email between the check above
            // and this claim (it releases its claim right after the insert).
            if matches!(email_exists(pool, &parsed.message_id).await, Ok(true)) {
                release_claim(pool, &parsed.message_id, &token, bucket, key).await;
                tracing::info!(
                    bucket = bucket,
                    key = key,
                    "Inbound email stored by another invocation; skipping"
                );
                return OK_RESPONSE;
            }
            true
        }
        EmailClaim::HeldByAnother => {
            tracing::info!(
                bucket = bucket,
                key = key,
                "Inbound email is being processed by another invocation; retry later"
            );
            return EMAIL_IN_PROGRESS;
        }
        EmailClaim::Unavailable => false,
    };
    let email_info = EmailInfo {
        parsed: &parsed,
        attachments,
        bucket,
        key,
    };
    // Always attempt threading. `In-Reply-To` / `References` come first;
    // `process_reply_email` also matches a recent outbound with the same
    // subject and addresses (Yahoo/iPhone sometimes omits those headers)
    // and falls back to a new thread when nothing matches.
    let response = process_reply_email(pool, client, email_info).await;
    if claimed {
        release_claim(pool, &parsed.message_id, &token, bucket, key).await;
    }
    response
}

async fn release_claim(pool: &MySqlPool, message_id: &str, token: &str, bucket: &str, key: &str) {
    if let Err(error) = release_email_processing(pool, message_id, token).await {
        tracing::warn!(
            ?error,
            bucket = bucket,
            key = key,
            "Failed to release the inbound email claim; it expires on its own"
        );
    }
}

/// Adds `Retry-After` to the "in progress" answer.
fn into_http_response(response: BasicResponse) -> Response {
    let mut http_response = response.into_response();
    if response.0 == StatusCode::TOO_MANY_REQUESTS {
        http_response.headers_mut().insert(
            RETRY_AFTER,
            HeaderValue::from_static(EMAIL_IN_PROGRESS_RETRY_AFTER),
        );
    }
    http_response
}

pub async fn receive_handler(
    State(pool): State<MySqlPool>,
    Json(event): Json<S3Event>,
) -> Response {
    let custom_client = CustomClient {};
    into_http_response(process_ses_received_event(&pool, custom_client, &event).await)
}

#[cfg(test)]
mod local_tests {
    use super::*;
    use crate::posthog::PostHogEvent;
    use crate::tests::data::ses_open_json::ses_open_event_json;
    use crate::tests::data::ses_received::ses_received_json;
    use crate::tests::utils::{
        MockClient, MockUnprocessedReporter, get_emails, insert_email, insert_user, new_test_app,
        read_file_as_bytes,
    };
    use axum::http::StatusCode;
    use sqlx::MySqlPool;

    #[test]
    fn missing_s3_object_errors_are_detected() {
        assert!(is_missing_s3_object(
            "service error: NoSuchKey: The specified key does not exist."
        ));
        assert!(is_missing_s3_object(
            "NoSuchBucket: The specified bucket does not exist"
        ));
        assert!(!is_missing_s3_object("AccessDenied"));
        assert!(!is_missing_s3_object("timeout connecting to S3"));
    }

    struct ReadDb {
        message_id: String,
        user_agent: Option<String>,
        ip_address: Option<String>,
    }

    pub struct Attachment {
        content_type: String,
        content_subtype: Option<String>,
        filename: String,
        url: String,
    }

    const BUCKET_NAME: Option<&str> =
        Some("s3://granite-ses-inbound-emails/p51f95lgdaa8rpcjp0q7loemss3a17avpnc48ug1");

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

    #[sqlx::test(migrations = "../migrations")]
    async fn open_event_success(pool: MySqlPool) {
        let app = new_test_app(pool.clone());

        let message_id =
            "010f019a9974b389-60efe038-3845-92e7-45c43cdc6ca2-000000@us-east-2.amazonses.com";
        let expected_user_agent = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/42.0.2311.135 Safari/537.36 Edge/12.246 Mozilla/5.0";
        let expected_ip = "108.177.2.32";

        insert_email(&pool, message_id).await.unwrap();

        let response = app
            .post("/ses/read-receipt")
            .json(&ses_open_event_json())
            .await;

        assert_eq!(response.status_code(), StatusCode::OK);

        let result = check_db_email_reads(&pool).await.unwrap();

        assert_eq!(result.message_id, message_id);
        assert_eq!(result.user_agent.unwrap(), expected_user_agent);
        assert_eq!(result.ip_address.unwrap(), expected_ip);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn open_event_unknown_message_is_ok(pool: MySqlPool) {
        let app = new_test_app(pool.clone());
        let response = app
            .post("/ses/read-receipt")
            .json(&ses_open_event_json())
            .await;
        assert_eq!(response.status_code(), StatusCode::OK);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn received_success(pool: MySqlPool) {
        let message_id =
            "010f019ab18dd4f1-e4d8dbab-6e05-466a-9cdb-5c9ccde5f3de-000000@us-east-2.amazonses.com";

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

    /// The whole point of `email_participants`: an inbound message must land
    /// with a `from` row, every `To:` and every `Cc:` — not just one receiver.
    #[sqlx::test(migrations = "../migrations")]
    async fn received_writes_all_participants(pool: MySqlPool) {
        const CLIENT_EMAIL: &str = "colin.delahunty@granite-manager.com";
        insert_user(&pool, CLIENT_EMAIL, None).await.unwrap();

        let mock_client = MockClient::new("src/tests/data/multi_recipient.eml");
        let data: S3Event = ses_received_json();
        let response = process_ses_received_event(&pool, mock_client, &data).await;
        assert_eq!(response, OK_RESPONSE);

        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT type, email FROM email_participants ORDER BY type, position, email",
        )
        .fetch_all(&pool)
        .await
        .unwrap();

        let of = |wanted: &str| -> Vec<String> {
            rows.iter()
                .filter(|(t, _)| t == wanted)
                .map(|(_, e)| e.clone())
                .collect()
        };

        assert_eq!(of("from"), vec!["customer@example.com".to_string()]);
        assert_eq!(
            of("to"),
            vec![
                CLIENT_EMAIL.to_string(),
                "shared@granite-manager.com".to_string()
            ]
        );
        assert_eq!(
            of("cc"),
            vec![
                "manager@granite-manager.com".to_string(),
                "spouse@example.com".to_string()
            ]
        );
        assert_eq!(
            of("bcc"),
            vec![
                "silent@granite-manager.com".to_string(),
                "quiet@example.com".to_string()
            ]
        );
    }

    /// The resolved receiver's company must be stamped on the row, so tenancy
    /// no longer depends on address matching at query time.
    #[sqlx::test(migrations = "../migrations")]
    async fn received_stamps_company_id(pool: MySqlPool) {
        const CLIENT_EMAIL: &str = "colin.delahunty@granite-manager.com";
        insert_user(&pool, CLIENT_EMAIL, None).await.unwrap();

        let expected: Option<i32> = sqlx::query_scalar("SELECT company_id FROM users LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();

        let mock_client = MockClient::new("src/tests/data/multi_recipient.eml");
        let data: S3Event = ses_received_json();
        assert_eq!(
            process_ses_received_event(&pool, mock_client, &data).await,
            OK_RESPONSE
        );

        let company_id: Option<i32> =
            sqlx::query_scalar("SELECT company_id FROM emails ORDER BY id DESC LIMIT 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(company_id, expected);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn received_reply_invalid_message_id(pool: MySqlPool) {
        const CLIENT_EMAIL: &str = "colin.delahunty@granite-manager.com";
        insert_user(&pool, CLIENT_EMAIL, None).await.unwrap();
        let mock_client = MockClient::new("src/tests/data/reply_email1.eml");

        let data: S3Event = ses_received_json();
        let response = process_ses_received_event(&pool, mock_client, &data).await;

        assert_eq!(response, OK_RESPONSE);

        // TODO: Check that correct email was added into the db

        let result = get_emails(&pool).await.unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].subject, Some("Re: COLINS TEST".to_string()));
        const EMAIL_BODY: &str = "Please respond.";
        assert_eq!(result[0].body.clone().unwrap(), EMAIL_BODY);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn received_success_backwards_compatible(pool: MySqlPool) {
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

    /// Customer is the first To. Employees later on the same header still receive it.
    #[sqlx::test(migrations = "../migrations")]
    async fn received_when_customer_is_first_to(pool: MySqlPool) {
        const LIZA: &str = "liza@granitedepotindy.com";
        const MASHA: &str = "masha@granitedepotindy.com";
        let liza_id = insert_user(&pool, LIZA, None).await.unwrap();
        let masha_id = insert_user(&pool, MASHA, None).await.unwrap();
        let mock_client = MockClient::new("src/tests/data/customer_first_to.eml");
        let data: S3Event = ses_received_json();

        let response = process_ses_received_event(&pool, mock_client, &data).await;
        assert_eq!(response, OK_RESPONSE);

        let result = get_emails(&pool).await.unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].receiver_user_id.unwrap(), liza_id);
        assert_eq!(
            result[0].receiver_email.as_deref(),
            Some("pdekemper58@gmail.com")
        );

        let participant_users: Vec<(String, Option<i32>)> = sqlx::query_as(
            "SELECT email, user_id FROM email_participants WHERE type = 'to' AND user_id IS NOT NULL ORDER BY position",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            participant_users,
            vec![
                (LIZA.to_string(), Some(liza_id)),
                (MASHA.to_string(), Some(masha_id)),
            ]
        );
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn received_no_start_email(pool: MySqlPool) {
        let mock_client = MockClient::new("src/tests/data/external1.eml");
        let data: S3Event = ses_received_json();
        let response = process_ses_received_event(&pool, mock_client, &data).await;

        assert_eq!(response, OK_RESPONSE);

        let result = get_emails(&pool).await.unwrap();
        assert_eq!(result.len(), 0);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn received_first(pool: MySqlPool) {
        const CLIENT_EMAIL: &str = "info@granitedepotindy.com";
        let user_id = insert_user(&pool, CLIENT_EMAIL, Some(456)).await.unwrap();
        let mock_client = MockClient::new("src/tests/data/external1.eml");
        let data: S3Event = ses_received_json();

        let response = process_ses_received_event(&pool, mock_client, &data).await;
        assert_eq!(response.0, StatusCode::OK);

        let result = get_emails(&pool).await.unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].receiver_user_id.unwrap(), user_id);
        assert_eq!(result[0].receiver_email, Some(CLIENT_EMAIL.to_string()));
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn received_first_forward_to_user_only(pool: MySqlPool) {
        const CLIENT_EMAIL: &str = "dema.gdindy@gmail.com";
        let user_id = insert_user(&pool, CLIENT_EMAIL, None).await.unwrap();
        let mock_client = MockClient::new("src/tests/data/forwarded.eml");
        let data: S3Event = ses_received_json();

        let response = process_ses_received_event(&pool, mock_client, &data).await;
        assert_eq!(response.0, StatusCode::OK);

        let result = get_emails(&pool).await.unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].receiver_user_id.unwrap(), user_id);
        assert_eq!(result[0].receiver_email, Some(CLIENT_EMAIL.to_string()));
        assert_eq!(result[0].bucket.as_deref(), BUCKET_NAME);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn received_first_forward_forward_user_only(pool: MySqlPool) {
        const CLIENT_EMAIL: &str = "dema@granitedepotindy.com";
        let user_id = insert_user(&pool, CLIENT_EMAIL, None).await.unwrap();
        let mock_client = MockClient::new("src/tests/data/forwarded.eml");
        let data: S3Event = ses_received_json();

        let response = process_ses_received_event(&pool, mock_client, &data).await;
        assert_eq!(response.0, StatusCode::OK);

        let result = get_emails(&pool).await.unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].receiver_user_id.unwrap(), user_id);
        assert_eq!(result[0].receiver_email, Some(CLIENT_EMAIL.to_string()));
        assert_eq!(result[0].bucket.as_deref(), BUCKET_NAME);
    }
    #[sqlx::test(migrations = "../migrations")]
    async fn received_first_forward_both_users(pool: MySqlPool) {
        const CLIENT_EMAIL: &str = "dema.gdindy@gmail.com";
        const CLIENT_EMAIL2: &str = "dema@granitedepotindy.com";
        let user_id = insert_user(&pool, CLIENT_EMAIL, None).await.unwrap();
        insert_user(&pool, CLIENT_EMAIL2, None).await.unwrap();
        let mock_client = MockClient::new("src/tests/data/forwarded.eml");
        let data: S3Event = ses_received_json();

        let response = process_ses_received_event(&pool, mock_client, &data).await;
        assert_eq!(response.0, StatusCode::OK);

        let result = get_emails(&pool).await.unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].receiver_user_id.unwrap(), user_id);
        assert_eq!(result[0].receiver_email, Some(CLIENT_EMAIL.to_string()));
        assert_eq!(result[0].bucket.as_deref(), BUCKET_NAME);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn received_first_forward_from_user(pool: MySqlPool) {
        const CLIENT_EMAIL: &str = "dema@granitedepotindy.com";
        let user_id = insert_user(&pool, CLIENT_EMAIL, None).await.unwrap();
        let mock_client = MockClient::new("src/tests/data/forwarded_from_user.eml");
        let data: S3Event = ses_received_json();
        let response = process_ses_received_event(&pool, mock_client, &data).await;
        assert_eq!(response.0, StatusCode::OK);
        let result = get_emails(&pool).await.unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].receiver_user_id.unwrap(), user_id);
        assert_eq!(result[0].receiver_email, Some(CLIENT_EMAIL.to_string()));
        assert_eq!(result[0].bucket.as_deref(), BUCKET_NAME);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn received_not_a_reply_user(pool: MySqlPool) {
        let message_id = "010f019ab18dd4f1-e4d8dbab-6e05-466a-9cdb-5c9ccde5f3de-000000";

        let admin_id = insert_user(&pool, "info@granitedepotindy.com", Some(456))
            .await
            .unwrap();
        insert_email(&pool, message_id).await.unwrap();

        let mock_client = MockClient::new("src/tests/data/external1.eml");

        let data: S3Event = ses_received_json();
        let response = process_ses_received_event(&pool, mock_client, &data).await;

        assert_eq!(response, OK_RESPONSE);

        let result = get_emails(&pool).await.unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(&result[1].receiver_user_id.unwrap(), &admin_id);
        assert_eq!(result[1].thread_id.clone().unwrap().len(), 36);
        assert_eq!(result[1].bucket.as_deref(), BUCKET_NAME);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn received_not_a_reply_no_user(pool: MySqlPool) {
        let message_id = "010f019ab18dd4f1-e4d8dbab-6e05-466a-9cdb-5c9ccde5f3de-000000";

        insert_email(&pool, message_id).await.unwrap();

        let mock_client = MockClient::new("src/tests/data/external1.eml");

        let data: S3Event = ses_received_json();
        let response = process_ses_received_event(&pool, mock_client, &data).await;

        assert_eq!(response, OK_RESPONSE);

        let result = get_emails(&pool).await.unwrap();
        assert_eq!(result.len(), 1);
    }
    #[sqlx::test(migrations = "../migrations")]
    async fn dima_no_receiver_found(pool: MySqlPool) {
        let mock_client = MockClient::new("src/tests/data/failed_1.eml");
        const CLIENT_EMAIL: &str = "dema@granitedepotindy.com";
        let user_id = insert_user(&pool, CLIENT_EMAIL, None).await.unwrap();

        let message_id =
            "010f019dad3defa2-120b60d6-0844-43a4-900d-56cc99b643cf-000000@us-east-2.amazonses.com";
        insert_email(&pool, message_id).await.unwrap();

        let data: S3Event = ses_received_json();
        let response = process_ses_received_event(&pool, mock_client, &data).await;

        assert_eq!(response.0, StatusCode::OK);

        let result = get_emails(&pool).await.unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[1].receiver_email.as_deref().unwrap(), CLIENT_EMAIL);
        assert_eq!(result[1].receiver_user_id, Some(user_id));
        assert_eq!(result[1].bucket.as_deref(), BUCKET_NAME);
    }
    #[sqlx::test(migrations = "../migrations")]
    async fn dima_attachment_no_body(pool: MySqlPool) {
        let mock_client = MockClient::new("src/tests/data/image_only.eml");
        const CLIENT_EMAIL: &str = "dema@granitedepotindy.com";
        insert_user(&pool, CLIENT_EMAIL, None).await.unwrap();

        let data: S3Event = ses_received_json();
        let response = process_ses_received_event(&pool, mock_client, &data).await;

        assert_eq!(response.0, StatusCode::OK);

        let result = get_emails(&pool).await.unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].body, Some("".to_string()));
        let attachments = get_email_attachments(&pool, result[0].id as u64)
            .await
            .unwrap();
        assert_eq!(attachments.len(), 1);
    }

    /// An email that still cannot be parsed (no recipient in any header or in
    /// the SES envelope) is answered 200, so `EventBridge` does not retry what
    /// no retry can fix, and reported once as `inbound_email_unprocessed`.
    #[sqlx::test(migrations = "../migrations")]
    async fn unparseable_email_is_reported_and_not_retried(pool: MySqlPool) {
        let mock_client = MockClient::new("src/tests/data/no_recipient_anywhere.eml");
        let reporter = MockUnprocessedReporter::default();
        let data: S3Event = ses_received_json();

        let response =
            process_ses_received_event_with(&pool, mock_client.clone(), &data, &reporter).await;

        assert_eq!(response, OK_RESPONSE);
        assert_eq!(
            reporter.reports(),
            vec![(
                "granite-ses-inbound-emails".to_string(),
                "p51f95lgdaa8rpcjp0q7loemss3a17avpnc48ug1".to_string(),
                "missing_receiver",
            )]
        );
        assert!(mock_client.uploaded_keys().is_empty());
        assert_eq!(get_emails(&pool).await.unwrap().len(), 0);
    }

    /// A transient S3 read error is still a 500: a retry can fix it.
    #[sqlx::test(migrations = "../migrations")]
    async fn s3_read_error_is_still_retried(pool: MySqlPool) {
        let mock_client = MockClient::new("src/tests/data/does_not_exist.eml");
        let reporter = MockUnprocessedReporter::default();
        let data: S3Event = ses_received_json();

        let response = process_ses_received_event_with(&pool, mock_client, &data, &reporter).await;

        assert_eq!(response.0, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(reporter.reports().is_empty());
    }

    #[test]
    fn parse_error_kinds_name_each_parse_email_failure() {
        assert_eq!(
            parse_error_kind("Failed to parse email"),
            "unreadable_message"
        );
        assert_eq!(
            parse_error_kind("Failed to parse message ID"),
            "missing_message_id"
        );
        assert_eq!(
            parse_error_kind("Failed to parse sender email"),
            "missing_sender"
        );
        assert_eq!(
            parse_error_kind("Failed to parse receiver email"),
            "missing_receiver"
        );
        assert_eq!(parse_error_kind("anything else"), "other");
    }

    /// The overlapping delivery of an email another invocation is still
    /// processing gets 429 (`EventBridge` retries later) and does no
    /// attachment work. Once that claim expires, a retry takes over.
    #[sqlx::test(migrations = "../migrations")]
    async fn overlapping_delivery_is_told_to_retry_later(pool: MySqlPool) {
        insert_user(&pool, "dema@granitedepotindy.com", None)
            .await
            .unwrap();
        let mock_client = MockClient::new("src/tests/data/image_only.eml");
        let data: S3Event = ses_received_json();
        let email_bytes = read_file_as_bytes("src/tests/data/image_only.eml").unwrap();
        let (parsed, _) = parse_email(&email_bytes).unwrap();
        let first_run = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        assert_eq!(
            claim_email_processing(&pool, &parsed.message_id, first_run).await,
            EmailClaim::Acquired
        );

        let overlap = process_ses_received_event(&pool, mock_client.clone(), &data).await;
        assert_eq!(overlap.0, StatusCode::TOO_MANY_REQUESTS);
        assert!(mock_client.uploaded_keys().is_empty());
        assert_eq!(get_emails(&pool).await.unwrap().len(), 0);

        // The first run was killed by the Lambda timeout: its claim expires.
        sqlx::query(
            "UPDATE email_processing_claims SET claimed_at = claimed_at - INTERVAL 41 SECOND",
        )
        .execute(&pool)
        .await
        .unwrap();
        let retry = process_ses_received_event(&pool, mock_client.clone(), &data).await;
        assert_eq!(retry, OK_RESPONSE);
        assert_eq!(mock_client.uploaded_keys().len(), 1);
        assert_eq!(get_emails(&pool).await.unwrap().len(), 1);
        assert_eq!(
            claim_rows(&pool).await,
            0,
            "the finished run releases its claim"
        );
    }

    /// `EventBridge` retries a 429, but the request logger reports only 5xx
    /// to `PostHog`, so a turned-away overlap is not an exception there.
    #[test]
    fn overlap_response_is_retried_but_not_reported_to_posthog() {
        assert_eq!(EMAIL_IN_PROGRESS.0, StatusCode::TOO_MANY_REQUESTS);
        assert!(!PostHogEvent::should_report_http_exception(
            EMAIL_IN_PROGRESS.0
        ));
    }

    #[test]
    fn overlap_response_asks_to_retry_after_30_seconds() {
        let response = into_http_response(EMAIL_IN_PROGRESS);
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok()),
            Some("30")
        );
        let ok = into_http_response(OK_RESPONSE);
        assert_eq!(ok.status(), StatusCode::OK);
        assert!(ok.headers().get(axum::http::header::RETRY_AFTER).is_none());
    }

    /// Another run stores the email between our `email_exists` check and our
    /// claim (simulated by a trigger on the claims table). The second check
    /// after the claim catches it: 200, no upload, claim released.
    #[sqlx::test(migrations = "../migrations")]
    async fn email_stored_while_claiming_is_not_processed_again(pool: MySqlPool) {
        insert_user(&pool, "dema@granitedepotindy.com", None)
            .await
            .unwrap();
        sqlx::raw_sql(
            "CREATE TRIGGER other_run_stores_email AFTER INSERT ON email_processing_claims \
             FOR EACH ROW INSERT INTO emails (subject, body, message_id, thread_id) \
             VALUES ('Stored by the other run', 'body', NEW.message_id, UUID())",
        )
        .execute(&pool)
        .await
        .unwrap();
        let mock_client = MockClient::new("src/tests/data/image_only.eml");
        let data: S3Event = ses_received_json();

        let response = process_ses_received_event(&pool, mock_client.clone(), &data).await;

        assert_eq!(response, OK_RESPONSE);
        assert!(mock_client.uploaded_keys().is_empty());
        assert_eq!(get_emails(&pool).await.unwrap().len(), 1);
        assert_eq!(claim_rows(&pool).await, 0);
    }

    /// Every finished run releases its claim, so a later redelivery is
    /// answered by the `email_exists` check, not held off by a stale claim.
    #[sqlx::test(migrations = "../migrations")]
    async fn finished_run_releases_its_claim(pool: MySqlPool) {
        let data: S3Event = ses_received_json();
        let mock_client = MockClient::new("src/tests/data/image_only.eml");

        // Unknown receiver: handled, nothing stored, and the claim released.
        let unknown = process_ses_received_event(&pool, mock_client.clone(), &data).await;
        assert_eq!(unknown, OK_RESPONSE);
        assert_eq!(get_emails(&pool).await.unwrap().len(), 0);
        assert_eq!(claim_rows(&pool).await, 0);

        insert_user(&pool, "dema@granitedepotindy.com", None)
            .await
            .unwrap();
        let stored = process_ses_received_event(&pool, mock_client.clone(), &data).await;
        assert_eq!(stored, OK_RESPONSE);
        assert_eq!(get_emails(&pool).await.unwrap().len(), 1);
        assert_eq!(claim_rows(&pool).await, 0);
    }

    /// Lambda deployed before the migration: emails are processed as before.
    #[sqlx::test(migrations = "../migrations")]
    async fn missing_claims_table_still_stores_the_email(pool: MySqlPool) {
        sqlx::query("DROP TABLE email_processing_claims")
            .execute(&pool)
            .await
            .unwrap();
        insert_user(&pool, "dema@granitedepotindy.com", None)
            .await
            .unwrap();
        let mock_client = MockClient::new("src/tests/data/image_only.eml");
        let data: S3Event = ses_received_json();

        let response = process_ses_received_event(&pool, mock_client.clone(), &data).await;
        assert_eq!(response, OK_RESPONSE);
        assert_eq!(mock_client.uploaded_keys().len(), 1);
        assert_eq!(get_emails(&pool).await.unwrap().len(), 1);
    }

    async fn claim_rows(pool: &MySqlPool) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM email_processing_claims")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// `EventBridge` redelivers an email whose first run took over 5 s. The
    /// redelivery must not upload the attachments again (each upload used to
    /// leave an orphan copy in `gd-email-attachments`) or store a second row.
    #[sqlx::test(migrations = "../migrations")]
    async fn redelivered_email_uploads_nothing(pool: MySqlPool) {
        insert_user(&pool, "dema@granitedepotindy.com", None)
            .await
            .unwrap();
        let mock_client = MockClient::new("src/tests/data/image_only.eml");
        let data: S3Event = ses_received_json();

        let first = process_ses_received_event(&pool, mock_client.clone(), &data).await;
        assert_eq!(first, OK_RESPONSE);
        assert_eq!(mock_client.uploaded_keys().len(), 1);

        let redelivery = process_ses_received_event(&pool, mock_client.clone(), &data).await;
        assert_eq!(redelivery, OK_RESPONSE);
        assert_eq!(mock_client.uploaded_keys().len(), 1);

        let result = get_emails(&pool).await.unwrap();
        assert_eq!(result.len(), 1);
        let email_id = u64::try_from(result[0].id).unwrap();
        let attachments = get_email_attachments(&pool, email_id).await.unwrap();
        assert_eq!(attachments.len(), 1);
    }

    /// Same for a reply that joins a thread: a redelivery uploads nothing.
    #[sqlx::test(migrations = "../migrations")]
    async fn redelivered_reply_uploads_nothing(pool: MySqlPool) {
        let parent_id = "CAG6QthaOtf0GWH6Ba9eOfRkfbviRi-RJw_vVnRc4U5cW_9GPmA@mail.gmail.com";
        insert_email(&pool, parent_id).await.unwrap();
        let mock_client = MockClient::new("src/tests/data/reply_attachment_2.eml");
        let data: S3Event = ses_received_json();

        assert_eq!(
            process_ses_received_event(&pool, mock_client.clone(), &data).await,
            OK_RESPONSE
        );
        let first_keys = mock_client.uploaded_keys();
        assert_eq!(first_keys.len(), 4);

        assert_eq!(
            process_ses_received_event(&pool, mock_client.clone(), &data).await,
            OK_RESPONSE
        );
        assert_eq!(mock_client.uploaded_keys(), first_keys);
        assert_eq!(get_emails(&pool).await.unwrap().len(), 2);
    }

    /// An email nobody in the CRM receives is not stored, so its attachments
    /// must not reach S3 either.
    #[sqlx::test(migrations = "../migrations")]
    async fn unknown_receiver_uploads_nothing(pool: MySqlPool) {
        let mock_client = MockClient::new("src/tests/data/image_only.eml");
        let data: S3Event = ses_received_json();

        let response = process_ses_received_event(&pool, mock_client.clone(), &data).await;
        assert_eq!(response, OK_RESPONSE);
        assert!(mock_client.uploaded_keys().is_empty());
        assert_eq!(get_emails(&pool).await.unwrap().len(), 0);
    }

    /// Our employee was BCC'd: the only `To:` is an outside address and no
    /// `Bcc:` header is left. SES's top `Received` still names the employee.
    #[sqlx::test(migrations = "../migrations")]
    async fn bcc_employee_found_through_the_ses_envelope(pool: MySqlPool) {
        let user_id = insert_user(&pool, "rep@granitedepotcolumbus.com", None)
            .await
            .unwrap();
        let mock_client = MockClient::new("src/tests/data/customer_to_envelope_user.eml");
        let data: S3Event = ses_received_json();

        let response = process_ses_received_event(&pool, mock_client, &data).await;
        assert_eq!(response, OK_RESPONSE);

        let result = get_emails(&pool).await.unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].receiver_user_id, Some(user_id));
        assert_eq!(
            result[0].receiver_email.as_deref(),
            Some("contractor@example.org")
        );
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn envelope_that_is_not_a_user_is_still_dropped(pool: MySqlPool) {
        let mock_client = MockClient::new("src/tests/data/customer_to_envelope_user.eml");
        let data: S3Event = ses_received_json();

        let response = process_ses_received_event(&pool, mock_client, &data).await;
        assert_eq!(response, OK_RESPONSE);
        assert_eq!(get_emails(&pool).await.unwrap().len(), 0);
    }

    /// `To: undisclosed-recipients:;` used to fail parsing (500, lost after
    /// three tries). It is now stored for the SES envelope recipient.
    #[sqlx::test(migrations = "../migrations")]
    async fn undisclosed_recipients_forward_is_stored(pool: MySqlPool) {
        let user_id = insert_user(&pool, "sales@granitedepotcolumbus.com", None)
            .await
            .unwrap();
        let mock_client = MockClient::new("src/tests/data/undisclosed_forwarded.eml");
        let data: S3Event = ses_received_json();

        let response = process_ses_received_event(&pool, mock_client, &data).await;
        assert_eq!(response, OK_RESPONSE);

        let result = get_emails(&pool).await.unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].receiver_user_id, Some(user_id));
        assert_eq!(
            result[0].receiver_email.as_deref(),
            Some("sales@granitedepotcolumbus.com")
        );
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn response_to_received_success(pool: MySqlPool) {
        let message_id =
            "010f019b278e838b-4026f591-7b73-451a-a540-7e70c8bd5c84-000000@us-east-2.amazonses.com";

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
        assert_eq!(result[1].bucket.as_deref(), BUCKET_NAME);
        assert_eq!(result[2].bucket.as_deref(), BUCKET_NAME);
    }
    #[sqlx::test(migrations = "../migrations")]
    async fn four_attachments(pool: MySqlPool) {
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

    #[sqlx::test(migrations = "../migrations")]
    async fn yahoo_thank_you_reply_joins_truncated_ses_message_id(pool: MySqlPool) {
        insert_user(&pool, "dema@granitedepotindy.com", None)
            .await
            .unwrap();
        const FULL_ID: &str =
            "010f01a01a13f5b0-4f13d0d2-4e15-41f9-abfe-2b297e4c650d-000000@us-east-2.amazonses.com";
        let truncated: String = FULL_ID.chars().take(72).collect();
        insert_email(&pool, &truncated).await.unwrap();

        let mock_client = MockClient::new("src/tests/data/yahoo_iphone_thank_you_reply.eml");
        let data: S3Event = ses_received_json();
        let response = process_ses_received_event(&pool, mock_client, &data).await;
        assert_eq!(response, OK_RESPONSE);

        let result = get_emails(&pool).await.unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(
            result[1].subject,
            Some("Re: Thank You for Your Request".to_string())
        );
        assert_eq!(
            result[1].thread_id.clone().unwrap(),
            result[0].thread_id.clone().unwrap()
        );
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn yahoo_thank_you_reply_joins_uuid_backfilled_drip(pool: MySqlPool) {
        let user_id = insert_user(&pool, "dema@granitedepotindy.com", None)
            .await
            .unwrap();
        let thread_id = uuid::Uuid::new_v4().to_string();
        let drip_message_id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            r#"
            INSERT INTO emails (
                sender_user_id, subject, body, message_id, thread_id,
                sender_email, receiver_email
            )
            VALUES (?, 'Thank You for Your Request', 'Thanks', ?, ?, ?, ?)
            "#,
        )
        .bind(user_id)
        .bind(&drip_message_id)
        .bind(&thread_id)
        .bind("dema@granitedepotindy.com")
        .bind("dicemoon@sbcglobal.net")
        .execute(&pool)
        .await
        .unwrap();

        let mock_client = MockClient::new("src/tests/data/yahoo_iphone_thank_you_reply.eml");
        let data: S3Event = ses_received_json();
        let response = process_ses_received_event(&pool, mock_client, &data).await;
        assert_eq!(response, OK_RESPONSE);

        let result = get_emails(&pool).await.unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[1].thread_id.as_deref(), Some(thread_id.as_str()));
        assert_eq!(
            result[1].subject,
            Some("Re: Thank You for Your Request".to_string())
        );
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn yahoo_reply_without_parent_keeps_quoted_original(pool: MySqlPool) {
        insert_user(&pool, "dema@granitedepotindy.com", None)
            .await
            .unwrap();
        let mock_client = MockClient::new("src/tests/data/yahoo_iphone_thank_you_reply.eml");
        let data: S3Event = ses_received_json();
        let response = process_ses_received_event(&pool, mock_client, &data).await;
        assert_eq!(response, OK_RESPONSE);

        let result = get_emails(&pool).await.unwrap();
        assert_eq!(result.len(), 1);
        let body = result[0].body.as_deref().unwrap_or("");
        assert!(body.contains("I liked the glacier white leather granite."));
        assert!(
            body.contains("Thank you for your request"),
            "Expected the unmatched reply to keep the quoted original, got: {body}"
        );
    }
}
