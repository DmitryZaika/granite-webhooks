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

pub async fn process_reply_email(pool: &MySqlPool, client: C, message_id: &str) -> BasicResponse {
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

pub async fn process_first_email(pool: &MySqlPool, client: C) -> BasicResponse {
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