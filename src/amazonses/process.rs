use axum::http::StatusCode;
use lambda_http::tracing;
use sqlx::MySqlPool;
use uuid::Uuid;

use crate::amazon::bucket::S3Bucket;
use crate::amazonses::parse_email::{Attachment, ParsedEmail};
use crate::amazonses::upload::upload_attachments;
use crate::crud::email::{PriorEmail, create_email_with_attachments, get_prior_email};
use crate::crud::users::get_id_by_email;
use crate::libs::constants::{OK_RESPONSE, internal_error};
use crate::libs::types::BasicResponse;

pub struct EmailInfo<'a> {
    pub bucket: &'a str,
    pub key: &'a str,
    pub parsed: &'a ParsedEmail,
    pub attachments: Vec<Attachment>,
}

pub async fn process_reply_email<C: S3Bucket + Send + Sync + 'static>(
    pool: &MySqlPool,
    client: C,
    message_id: &str,
    email_info: EmailInfo<'_>,
) -> BasicResponse {
    let prior = match get_prior_email(pool, message_id).await {
        Ok(email) => email,
        Err(error) => {
            tracing::error!(
                ?error,
                bucket = email_info.bucket,
                key = email_info.key,
                "Failed to retrieve prior email"
            );
            return internal_error("Unable to retrieve prior email");
        }
    };
    let Some(clean_prior) = prior else {
        tracing::error!(
            bucket = email_info.bucket,
            key = email_info.key,
            "No prior email found"
        );
        return (StatusCode::BAD_REQUEST, "No prior email found");
    };

    let uploaded_attachments = match upload_attachments(client, email_info.attachments).await {
        Ok(attachments) => attachments,
        Err(error) => {
            tracing::error!(
                ?error,
                bucket = email_info.bucket,
                key = email_info.key,
                "Failed to upload attachments"
            );
            return internal_error("Failed to upload attachments");
        }
    };
    let result =
        create_email_with_attachments(pool, email_info.parsed, &clean_prior, &uploaded_attachments)
            .await;
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

pub async fn process_first_email<C: S3Bucket + Send + Sync + 'static>(
    pool: &MySqlPool,
    client: C,
    email_info: EmailInfo<'_>,
) -> BasicResponse {
    let uploaded_attachments = match upload_attachments(client, email_info.attachments).await {
        Ok(attachments) => attachments,
        Err(error) => {
            tracing::error!(
                ?error,
                bucket = email_info.bucket,
                key = email_info.key,
                "Failed to upload attachments"
            );
            return internal_error("Failed to upload attachments");
        }
    };
    let Some(receiver_user_id) = get_id_by_email(pool, &email_info.parsed.receiver_email)
        .await
        .unwrap()
    else {
        tracing::error!(
            bucket = email_info.bucket,
            to_email = email_info.parsed.receiver_email,
            "Reciever email not found"
        );
        return (StatusCode::NOT_FOUND, "receiver email not found");
    };
    let prior_email = PriorEmail {
        thread_id: Some(Uuid::new_v4().to_string()),
        receiver_user_id: Some(receiver_user_id),
    };
    let result =
        create_email_with_attachments(pool, email_info.parsed, &prior_email, &uploaded_attachments)
            .await;
    if let Err(error) = result {
        tracing::error!(
            "Error inserting email: {} into the db: {}",
            email_info.parsed.message_id,
            error
        );
        return internal_error("Failed to insert email into the database");
    }
    OK_RESPONSE
}
