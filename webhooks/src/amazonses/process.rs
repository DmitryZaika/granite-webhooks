use axum::http::StatusCode;
use lambda_http::tracing;
use sqlx::MySqlPool;

use crate::amazon::bucket::S3Bucket;
use crate::amazonses::parse_email::{Attachment, ParsedEmail};
use crate::amazonses::upload::upload_attachments;
use crate::axum_helpers::guards::NotificationsTelegramBot;
use crate::crud::deals::{maybe_cancel_flow_on_inbound_email, maybe_move_deal_on_inbound_email};
use crate::crud::email::{
    PriorEmail, SendEmail, create_email_with_attachments, get_inbound_email_notify_context,
    get_prior_email, resolve_inbound_customer_name,
};
use crate::crud::users::{
    ReceivingEmail, get_company_id_by_user_id, get_id_by_email, get_id_by_email_with_forward,
};
use crate::libs::constants::{OK_RESPONSE, internal_error};
use crate::libs::types::BasicResponse;
use crate::telegram::crm::{InboundEmailTelegramNotify, send_inbound_email_telegram_notification};

pub struct EmailInfo<'a> {
    pub bucket: &'a str,
    pub key: &'a str,
    pub parsed: &'a ParsedEmail,
    pub attachments: Vec<Attachment>,
}

impl EmailInfo<'_> {
    pub fn s3_url(&self) -> String {
        format!("s3://{}/{}", self.bucket, self.key)
    }
}

/// Owning company for an inbound message, taken from the resolved receiver.
/// A lookup failure is not fatal — the message is still stored, just without
/// company attribution, which leaves it legacy-visible rather than lost.
async fn resolve_company_id(pool: &MySqlPool, user_id: Option<i32>) -> Option<i32> {
    let user_id = user_id?;
    match get_company_id_by_user_id(pool, user_id).await {
        Ok(company_id) => company_id,
        Err(error) => {
            tracing::error!(
                ?error,
                user_id,
                "Failed to resolve company for inbound email"
            );
            None
        }
    }
}

pub async fn get_prior_email_backwards_compatible(
    pool: &MySqlPool,
    message_id: &str,
) -> Result<Option<PriorEmail>, sqlx::Error> {
    if let Some(prior) = get_prior_email(pool, message_id).await? {
        return Ok(Some(prior));
    }
    let clean = match message_id.find('@') {
        Some(idx) => &message_id[..idx],
        None => message_id,
    };
    get_prior_email(pool, clean).await
}

/// Find the thread an inbound message belongs to.
///
/// `In-Reply-To` is tried first, then each `References` entry from nearest
/// ancestor backwards, since some clients drop `In-Reply-To` on a forwarded
/// message but keep the chain.
pub async fn find_prior_email(
    pool: &MySqlPool,
    parsed: &ParsedEmail,
) -> Result<Option<PriorEmail>, sqlx::Error> {
    if let Some(in_reply_to) = parsed.in_reply_to.as_deref()
        && let Some(prior) = get_prior_email_backwards_compatible(pool, in_reply_to).await?
    {
        return Ok(Some(prior));
    }
    for reference in parsed.references.iter().rev() {
        if let Some(prior) = get_prior_email_backwards_compatible(pool, reference).await? {
            return Ok(Some(prior));
        }
    }
    Ok(None)
}

pub async fn process_reply_email<C: S3Bucket + Send + Sync + 'static>(
    pool: &MySqlPool,
    client: C,
    email_info: EmailInfo<'_>,
) -> BasicResponse {
    let s3_url = email_info.s3_url();
    let prior_raw = match find_prior_email(pool, email_info.parsed).await {
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
    let Some(prior) = prior_raw else {
        tracing::error!(
            bucket = email_info.bucket,
            key = email_info.key,
            "No prior email found. Processed as first email"
        );
        return process_first_email(pool, client, email_info).await;
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
    let received_id = match prior.receiver_user_id {
        Some(user_id) => Some(ReceivingEmail::To(user_id)),
        None => get_id_by_email(pool, &email_info.parsed.receiver_email)
            .await
            .unwrap()
            .map(ReceivingEmail::To),
    };
    let company_id = resolve_company_id(pool, received_id.map(ReceivingEmail::inner)).await;
    let send_email =
        SendEmail::new(email_info.parsed, prior.thread_id, received_id).with_company_id(company_id);
    let result =
        create_email_with_attachments(pool, &send_email, &s3_url, &uploaded_attachments).await;
    if let Err(error) = result {
        tracing::error!(
            "Error inserting email: {} into the db: {}",
            email_info.parsed.message_id,
            error
        );
        return internal_error("Failed to insert email into the database");
    }
    maybe_move_deal_on_inbound_email(pool, &send_email).await;
    maybe_cancel_flow_on_inbound_email(pool, &send_email).await;
    maybe_send_inbound_email_telegram(pool, &send_email).await;
    OK_RESPONSE
}

pub async fn process_first_email<C: S3Bucket + Send + Sync + 'static>(
    pool: &MySqlPool,
    client: C,
    email_info: EmailInfo<'_>,
) -> BasicResponse {
    let s3_url = email_info.s3_url();
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
    let Some(receiver) = get_id_by_email_with_forward(
        pool,
        &email_info.parsed.receiver_email,
        email_info.parsed.forward_to_email.as_deref(),
    )
    .await
    .unwrap() else {
        tracing::error!(
            bucket = email_info.bucket,
            to_email = email_info.parsed.receiver_email,
            "Reciever email not found"
        );
        return (StatusCode::NOT_FOUND, "receiver email not found");
    };
    let company_id = resolve_company_id(pool, Some(receiver.inner())).await;
    let send_email =
        SendEmail::new(email_info.parsed, None, Some(receiver)).with_company_id(company_id);
    let result =
        create_email_with_attachments(pool, &send_email, &s3_url, &uploaded_attachments).await;
    if let Err(error) = result {
        tracing::error!(
            "Error inserting email: {} into the db: {}",
            email_info.parsed.message_id,
            error
        );
        return internal_error("Failed to insert email into the database");
    }
    maybe_move_deal_on_inbound_email(pool, &send_email).await;
    maybe_cancel_flow_on_inbound_email(pool, &send_email).await;
    maybe_send_inbound_email_telegram(pool, &send_email).await;
    OK_RESPONSE
}

async fn maybe_send_inbound_email_telegram(pool: &MySqlPool, send: &SendEmail) {
    let Some(receiver_user_id) = send.receiver_user_id() else {
        return;
    };

    let context =
        match get_inbound_email_notify_context(pool, send.thread_id(), receiver_user_id).await {
            Ok(value) => value,
            Err(error) => {
                tracing::error!(
                    ?error,
                    thread_id = send.thread_id(),
                    "Failed to load inbound email notify context"
                );
                return;
            }
        };
    let (deal_id, customer_name) = match context {
        Some(value) => (
            value.deal_id,
            resolve_inbound_customer_name(value.customer_name, value.sender_email.as_deref()),
        ),
        None => (None, None),
    };

    let payload = InboundEmailTelegramNotify {
        receiver_user_id,
        thread_id: send.thread_id().to_string(),
        subject: send.subject().map(str::to_string),
        deal_id,
        customer_name,
    };
    let bot = NotificationsTelegramBot::default();
    if let Err(error) = send_inbound_email_telegram_notification(&pool, &bot, &payload).await {
        tracing::error!(
            ?error,
            receiver_user_id = receiver_user_id,
            thread_id = send.thread_id(),
            "Failed to send inbound email telegram notification"
        );
    }
}
