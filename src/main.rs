#![warn(clippy::all, clippy::pedantic, clippy::nursery)]
#![allow(
    clippy::option_if_let_else,
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    clippy::missing_panics_doc
)]
use amazonses::routes::read_receipt_handler;
use axum::{
    Router,
    response::IntoResponse,
    routing::{get, post},
};
use lambda_http::{Error, run, tracing};
use libs::constants::OK_RESPONSE;
use middleware::request_logger::print_request_body;
use sqlx::MySqlPool;
use std::env::set_var;
use telegram::receive::webhook_handler;
use webhooks::receivers::{documenso, facebook_contact_form, wordpress_contact_form};

use crate::webhooks::receivers::new_lead_form;

pub mod amazon;
pub mod amazonses;
pub mod axum_helpers;
pub mod crud;
pub mod google;
pub mod libs;
pub mod middleware;
pub mod posthog;
pub mod schemas;
pub mod telegram;
pub mod webhooks;

async fn health_check() -> impl IntoResponse {
    OK_RESPONSE
}

fn new_main_app(pool: MySqlPool) -> Router {
    tracing::init_default_subscriber();

    Router::new()
        .route("/", get(health_check))
        .route("/documenso", post(documenso))
        .route(
            "/wordpress-contact-form/{company_id}",
            post(wordpress_contact_form),
        )
        .route(
            "/facebook-contact-form/{company_id}",
            post(facebook_contact_form),
        )
        .route(
            "/v1/webhooks/new-lead-form/{company_id}",
            post(new_lead_form),
        )
        .route("/telegram/webhook", post(webhook_handler))
        .route("/ses/read-receipt", post(read_receipt_handler))
        .layer(axum::middleware::from_fn(print_request_body))
        .with_state(pool)
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    unsafe {
        set_var("AWS_LAMBDA_HTTP_IGNORE_STAGE_IN_PATH", "true");
    }
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let pool = MySqlPool::connect(&database_url).await?;
    let app = new_main_app(pool);
    run(app).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use axum_test::TestServer;
    use serde_json::Value;
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

    fn ses_open_event_json() -> Value {
        serde_json::from_str(r#"{
            "version": "0",
            "id": "df1515d9-b441-32ac-346a-8b4d5dd153c6",
            "detail-type": "Email Opened",
            "source": "aws.ses",
            "account": "741448943665",
            "time": "2025-11-19T00:12:34Z",
            "region": "us-east-2",
            "resources": [
                "arn:aws:ses:us-east-2:741448943665:configuration-set/email-tracking-set"
            ],
            "detail": {
                "eventType": "Open",
                "mail": {
                    "timestamp": "2025-11-19T00:12:33.545Z",
                    "source": "colin99delahunty@gmail.com",
                    "sendingAccountId": "741448943665",
                    "messageId": "010f019a9974b389-60efe038-3845-92e7-45c43cdc6ca2-000000",
                    "destination": ["colin99delahunty@gmail.com"],
                    "headersTruncated": false,
                    "headers": [
                        {"name":"From","value":"colin99delahunty@gmail.com"},
                        {"name":"To","value":"colin99delahunty@gmail.com"},
                        {"name":"Subject","value":"Product Overview Followup"},
                        {"name":"MIME-Version","value":"1.0"},
                        {"name":"Content-Type","value":"text/html; charset=UTF-8"},
                        {"name":"Content-Transfer-Encoding","value":"7bit"}
                    ],
                    "commonHeaders": {
                        "from": ["colin99delahunty@gmail.com"],
                        "to": ["colin99delahunty@gmail.com"],
                        "messageId": "010f019a9974b389-60efe038-3845-92e7-45c43cdc6ca2-000000",
                        "subject": "Product Overview Followup"
                    },
                    "tags": {
                        "ses:source-tls-version": ["TLSv1.3"],
                        "ses:operation": ["SendEmail"],
                        "ses:configuration-set": ["email-tracking-set"],
                        "ses:source-ip": ["68.44.153.241"],
                        "ses:from-domain": ["gmail.com"],
                        "ses:caller-identity": ["dima-ses"]
                    }
                },
                "open": {
                    "timestamp": "2025-11-19T00:12:34.926Z",
                    "userAgent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/42.0.2311.135 Safari/537.36 Edge/12.246 Mozilla/5.0",
                    "ipAddress": "108.177.2.32"
                }
            }
        }"#).unwrap()
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

        println!("Response: {:?}", response);

        assert_eq!(response.status_code(), StatusCode::OK);

        let result = check_db_email_reads(&pool).await.unwrap();

        assert_eq!(result.message_id, expected_message_id);
        assert_eq!(result.user_agent.unwrap(), expected_user_agent);
        assert_eq!(result.ip_address.unwrap(), expected_ip);
    }
}
