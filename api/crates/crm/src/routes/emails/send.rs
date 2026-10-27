//! Sending an employee's email (`POST /v1/emails`).
//!
//! This route checks the request and the caller's access, picks the sender
//! address, signature and thread headers, has the `webhooks` Lambda send each
//! message through SES, then records the messages (participants,
//! attachments, the deal customer's new addresses). Scheduled sends are
//! stored in `delayed_emails` in the shape the delivery cron reads.
//! Documents sent for e-signature: see [`super::signatures`].

use super::attachment_token;
use super::schemas::{SendAttachment, SendEmailRequest, SendEmailResult};
use super::signatures::{self, Created};
use super::sql::{can_read_thread, normalize_address, normalized};
use crate::auth::EmployeeUser;
use crate::auth::session::SessionUser;
use crate::entitlements::AddonKey;
use crate::error::{AddonRequiredBody, ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::native_delivery;
use crate::state::AppState;
use crate::webhooks_client::{OutgoingAttachment, OutgoingEmail, SendFailure};
use axum::Json;
use axum::extract::State;
use chrono::{Duration, Utc};
use common::crud::template::fetch_template_variable_data;
use common::utils::template::replace_template_variables;
use lambda_http::tracing;
use sqlx::{AssertSqlSafe, MySql, MySqlPool, Transaction};
use std::collections::{HashMap, HashSet};

pub const MAX_CC_RECIPIENTS: usize = 25;
const MAX_SUBJECT_CHARS: usize = 300;
const MAX_BODY_CHARS: usize = 100_000;
const MAX_DELAY_DAYS: i64 = 90;

/// Send an email
///
/// Sends the email now, or schedules it with `send_at`. Recipients are
/// lower-cased and deduplicated (`cc` drops `to` addresses, `bcc` drops
/// both). Several `to` recipients without `cc`, `bcc`, `thread_id` or
/// `send_together` each get their own message and conversation. Each message
/// is recorded with its participants and attachments, and the deal's
/// customer gets the recipients' new addresses.
#[crate::openapi::path(
    post,
    path = "/v1/emails",
    operation_id = "send_email",
    tag = "emails",
    request_body = SendEmailRequest,
    responses(
        (status = 200, description = "Sent, or scheduled", body = SendEmailResult),
        (status = 400, description = "Invalid recipients, empty email, unknown deal, bad send time, an attachment the caller may not use, or an address SES refused", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee, or sends documents for signature without the Signatures add-on (`addon`: `signatures`) or eSignature access", body = AddonRequiredBody),
        (status = 500, description = "Database error, or sent but not recorded", body = ErrorBody),
        (status = 502, description = "Sending failed", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn send(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(request): JsonBody<SendEmailRequest>,
) -> ApiResult<Json<SendEmailResult>> {
    let email = Cleaned::from_request(request)?;
    if let Some(send_at) = email.send_at {
        schedule(&state.pool, &user, &email, send_at).await?;
        return Ok(Json(SendEmailResult {
            sent: 0,
            delayed: true,
            thread_ids: email.thread_id.into_iter().collect(),
        }));
    }
    deliver(&state, &user, &email).await.map(Json)
}

/// A validated request.
#[derive(Debug, Clone)]
pub struct Cleaned {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub body: String,
    pub thread_id: Option<String>,
    pub deal_id: Option<i32>,
    pub customer_id: Option<i32>,
    pub send_together: bool,
    pub attachments: Vec<SendAttachment>,
    pub template_id: Option<i32>,
    pub send_at: Option<chrono::DateTime<Utc>>,
    pub cancel_if_customer_messages: bool,
    /// Sent by a flow, not typed by the rep (`emails.is_automated`).
    pub automated: bool,
    /// Attachments sent for e-signature.
    pub signature_requests: Vec<super::schemas::SignatureRequestInput>,
    /// The composer's id of this send (a UUID).
    pub signature_send_key: Option<String>,
    /// A scheduled email being sent: its row (the send's identity).
    pub delayed_email_id: Option<i64>,
}

impl Cleaned {
    pub fn from_request(request: SendEmailRequest) -> Result<Self, ApiError> {
        let bad = |message: &str| ApiError::BadRequest(message.to_string());
        let to = normalize_recipients(&request.to);
        if to.is_empty() {
            return Err(bad("Add at least one recipient"));
        }
        let cc: Vec<String> = normalize_recipients(&request.cc)
            .into_iter()
            .filter(|address| !to.contains(address))
            .collect();
        let bcc: Vec<String> = normalize_recipients(&request.bcc)
            .into_iter()
            .filter(|address| !to.contains(address) && !cc.contains(address))
            .collect();
        if let Some(invalid) = to.iter().chain(&cc).chain(&bcc).find(|a| !valid_address(a)) {
            return Err(ApiError::BadRequest(format!(
                "Invalid email address: {invalid}"
            )));
        }
        if cc.len() > MAX_CC_RECIPIENTS || bcc.len() > MAX_CC_RECIPIENTS {
            return Err(ApiError::BadRequest(format!(
                "At most {MAX_CC_RECIPIENTS} cc and {MAX_CC_RECIPIENTS} bcc recipients"
            )));
        }
        if request.signature_requests.len() > 20 {
            return Err(bad("Invalid signature request"));
        }
        for signature in &request.signature_requests {
            super::signatures::check(signature).map_err(bad)?;
        }
        let (subject, body) = super::signatures::with_defaults(
            request.subject.trim(),
            &request.body,
            &request.signature_requests,
        );
        let subject = subject.trim().to_string();
        if subject.chars().count() > MAX_SUBJECT_CHARS {
            return Err(bad("Subject is too long"));
        }
        if body.chars().count() > MAX_BODY_CHARS {
            return Err(bad("Message is too long"));
        }
        if is_empty_rich_text(&body) && request.attachments.is_empty() {
            return Err(bad("Add a message or an attachment"));
        }
        for attachment in &request.attachments {
            if attachment.url.trim().is_empty() || attachment.filename.trim().is_empty() {
                return Err(bad("Attachments need a url and a file name"));
            }
        }
        if let Some(send_at) = request.send_at {
            let now = Utc::now();
            if send_at <= now {
                return Err(bad("Choose a future time"));
            }
            if send_at - now > Duration::days(MAX_DELAY_DAYS) {
                return Err(bad("Send time must be within 90 days"));
            }
        }
        Ok(Self {
            to,
            cc,
            bcc,
            subject,
            body,
            thread_id: request
                .thread_id
                .map(|id| id.trim().to_string())
                .filter(|id| !id.is_empty()),
            deal_id: request.deal_id.filter(|id| *id > 0),
            customer_id: request.customer_id.filter(|id| *id > 0),
            send_together: request.send_together,
            attachments: request.attachments,
            template_id: request.template_id.filter(|id| *id > 0),
            send_at: request.send_at,
            cancel_if_customer_messages: request.cancel_if_customer_messages,
            automated: false,
            signature_requests: request.signature_requests,
            signature_send_key: request
                .signature_send_key
                .map(|key| key.trim().to_string())
                .filter(|key| !key.is_empty()),
            delayed_email_id: None,
        })
    }

    /// One message to everyone, or one per `to` recipient.
    pub fn batches(&self) -> Vec<Recipients> {
        let together = self.thread_id.is_some()
            || self.send_together
            || !self.cc.is_empty()
            || !self.bcc.is_empty()
            || self.to.len() == 1;
        if together {
            return vec![Recipients {
                to: self.to.clone(),
                cc: self.cc.clone(),
                bcc: self.bcc.clone(),
            }];
        }
        self.to
            .iter()
            .map(|to| Recipients {
                to: vec![to.clone()],
                cc: Vec::new(),
                bcc: Vec::new(),
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recipients {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
}

/// Lower-cased, trimmed, without blanks or repeats, in order.
pub fn normalize_recipients(values: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    values
        .iter()
        .map(|value| value.trim().to_lowercase())
        .filter(|value| !value.is_empty() && seen.insert(value.clone()))
        .collect()
}

/// `local@domain.tld`, without spaces or address punctuation.
pub fn valid_address(address: &str) -> bool {
    let Some((local, domain)) = address.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !domain.contains('@')
        && !address
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '<' | '>' | ',' | ';' | '"' | '(' | ')'))
}

/// No text: empty, or only tags (`<p><br></p>`).
pub fn is_empty_rich_text(html: &str) -> bool {
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag && !c.is_whitespace() => return false,
            _ => {}
        }
    }
    true
}

/// `application/pdf` → (`application`, `pdf`).
pub(super) fn split_content_type(content_type: &str) -> (String, String) {
    let lower = content_type.trim().to_lowercase();
    match lower.split_once('/') {
        Some((kind, sub)) if !kind.is_empty() => (kind.to_string(), sub.to_string()),
        _ => ("application".to_string(), String::new()),
    }
}

/// Stores the email in `delayed_emails`, in the payload shape the delivery
/// cron reads.
async fn schedule(
    pool: &MySqlPool,
    user: &SessionUser,
    email: &Cleaned,
    send_at: chrono::DateTime<Utc>,
) -> ApiResult<()> {
    let uploaded = (!email.attachments.is_empty()).then(|| {
        serde_json::Value::Array(
            email
                .attachments
                .iter()
                .map(|attachment| {
                    let (kind, sub) = split_content_type(&attachment.content_type);
                    let mut value = serde_json::json!({
                        "url": attachment.url,
                        "filename": attachment.filename,
                        "contentType": kind,
                        "contentSubtype": sub,
                    });
                    if let Some(token) = &attachment.token {
                        value["token"] = token.clone().into();
                    }
                    value
                })
                .collect(),
        )
        .to_string()
    });
    let payload = serde_json::json!({
        "to": email.to,
        "cc": email.cc,
        "bcc": email.bcc,
        "subject": email.subject,
        "body": email.body,
        "threadId": email.thread_id,
        "dealId": email.deal_id.map(|id| id.to_string()),
        "customerId": email.customer_id.map(|id| id.to_string()),
        "sendTogether": email.send_together,
        "templateId": email.template_id.map(|id| id.to_string()),
        "uploadedAttachments": uploaded,
        // The CRM form's shape, which both delivery crons read.
        "signatureRequests": (!email.signature_requests.is_empty()).then(|| {
            serde_json::Value::Array(
                email
                    .signature_requests
                    .iter()
                    .map(|request| {
                        let mut value = serde_json::json!({
                            "filename": request.filename,
                            "signerName": request.signer_name,
                            "signerEmail": request.signer_email,
                            "fields": request.fields,
                            "reminders": request.reminders,
                        });
                        if let Some(url) = &request.url {
                            value["url"] = url.clone().into();
                        }
                        value
                    })
                    .collect(),
            )
            .to_string()
        }),
    });
    sqlx::query(
        "INSERT INTO delayed_emails (
           company_id, user_id, customer_id, thread_id, recipient_emails,
           send_at, cancel_if_customer_messages, baseline_at, status, payload_json
         ) VALUES (?, ?, ?, ?, ?, ?, ?, UTC_TIMESTAMP(), 'pending', ?)",
    )
    .bind(user.company_id)
    .bind(user.id)
    .bind(email.customer_id)
    .bind(&email.thread_id)
    .bind(serde_json::Value::from(email.to.clone()).to_string())
    .bind(send_at.naive_utc())
    .bind(email.cancel_if_customer_messages)
    .bind(payload.to_string())
    .execute(pool)
    .await?;
    Ok(())
}

/// The first attachment the caller may not use (see [`may_attach`]).
async fn unauthorized_attachment<'a>(
    pool: &MySqlPool,
    secret: &str,
    user: &SessionUser,
    attachments: &'a [SendAttachment],
) -> ApiResult<Option<&'a str>> {
    for attachment in attachments {
        if !may_attach(
            pool,
            secret,
            user,
            &attachment.url,
            attachment.token.as_deref(),
        )
        .await?
        {
            return Ok(Some(&attachment.filename));
        }
    }
    Ok(None)
}

/// Whether the caller may attach the file at `url`: uploaded by them (a
/// valid `token`) or on a conversation of their company they may read.
pub(super) async fn may_attach(
    pool: &MySqlPool,
    secret: &str,
    user: &SessionUser,
    url: &str,
    token: Option<&str>,
) -> ApiResult<bool> {
    if attachment_token::verify(secret, url, user.id, token) {
        return Ok(true);
    }
    let row: Option<(Option<String>, Option<i32>)> = sqlx::query_as(
        "SELECT e.thread_id, e.company_id FROM email_attachments ea
         JOIN emails e ON e.id = ea.email_id
         WHERE ea.url = ? LIMIT 1",
    )
    .bind(url)
    .fetch_optional(pool)
    .await?;
    Ok(match row {
        Some((_, Some(company_id))) => company_id == user.company_id,
        Some((Some(thread_id), None)) => can_read_thread(&thread_id, user.company_id)
            .query()
            .fetch_optional(pool)
            .await?
            .is_some(),
        _ => false,
    })
}

/// The company's live customer with this address.
pub(super) async fn customer_by_address(
    pool: &MySqlPool,
    address: &str,
    company_id: i32,
) -> Result<Option<i32>, sqlx::Error> {
    let address = normalize_address(Some(address));
    if address.is_empty() {
        return Ok(None);
    }
    sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT c.id FROM customers c
         JOIN customers_emails ce ON ce.customer_id = c.id
         WHERE c.company_id = ? AND c.deleted_at IS NULL AND {} = ?
         LIMIT 1",
        normalized("ce.email")
    )))
    .bind(company_id)
    .bind(address)
    .fetch_optional(pool)
    .await
}

struct Sender {
    name: Option<String>,
    address: String,
    signature: Option<String>,
}

async fn sender(pool: &MySqlPool, user: &SessionUser) -> Result<Sender, sqlx::Error> {
    let domain: Option<Option<String>> =
        sqlx::query_scalar("SELECT domain FROM company WHERE id = ?")
            .bind(user.company_id)
            .fetch_optional(pool)
            .await?;
    let profile: Option<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT email_signature, email_name FROM users WHERE id = ? AND is_deleted = 0",
    )
    .bind(user.id)
    .fetch_optional(pool)
    .await?;
    let (signature, name) = profile.unwrap_or_default();
    Ok(Sender {
        name: name.filter(|name| !name.trim().is_empty()),
        address: super::thread::from_email(domain.flatten().as_deref(), &user.email),
        signature: signature.filter(|_| user.plan.has(AddonKey::Signatures)),
    })
}

/// `Message-ID`s of the conversation, oldest first, in angle brackets.
async fn thread_references(
    pool: &MySqlPool,
    thread_id: Option<&str>,
) -> Result<Vec<String>, sqlx::Error> {
    let Some(thread_id) = thread_id else {
        return Ok(Vec::new());
    };
    let ids: Vec<String> = sqlx::query_scalar(
        "SELECT message_id FROM emails
         WHERE thread_id = ? AND message_id IS NOT NULL AND message_id <> ''
         ORDER BY sent_at ASC",
    )
    .bind(thread_id)
    .fetch_all(pool)
    .await?;
    Ok(ids
        .into_iter()
        .map(|id| {
            if id.starts_with('<') {
                id
            } else {
                format!("<{id}>")
            }
        })
        .collect())
}

/// The customer of the email's deal (`None` without a deal); a deal of
/// another company is refused.
async fn deal_customer(
    pool: &MySqlPool,
    user: &SessionUser,
    email: &Cleaned,
) -> ApiResult<Option<i32>> {
    let Some(deal_id) = email.deal_id else {
        return Ok(None);
    };
    let customer: Option<i32> = sqlx::query_scalar(
        "SELECT d.customer_id FROM deals d
         JOIN customers c ON c.id = d.customer_id
         WHERE d.id = ? AND d.deleted_at IS NULL AND c.company_id = ?
         LIMIT 1",
    )
    .bind(deal_id)
    .bind(user.company_id)
    .fetch_optional(pool)
    .await?;
    let customer = customer.ok_or_else(|| ApiError::BadRequest("Deal not found".to_string()))?;
    Ok(Some(customer).filter(|id| *id > 0))
}

struct SentMessage {
    message_id: String,
    sender_address: String,
    recipients: Recipients,
    thread_id: String,
    body: String,
}

/// What every message of the request shares.
struct Envelope {
    sender: Sender,
    references: Vec<String>,
    in_reply_to: Option<String>,
    attachments: Vec<OutgoingAttachment>,
}

impl Envelope {
    async fn new(pool: &MySqlPool, user: &SessionUser, email: &Cleaned) -> ApiResult<Self> {
        let in_reply_to: Option<String> = match &email.thread_id {
            Some(thread_id) => sqlx::query_scalar(
                "SELECT message_id FROM emails WHERE thread_id = ? ORDER BY sent_at DESC LIMIT 1",
            )
            .bind(thread_id)
            .fetch_optional(pool)
            .await?
            .flatten(),
            None => None,
        };
        Ok(Self {
            sender: sender(pool, user).await?,
            references: thread_references(pool, email.thread_id.as_deref()).await?,
            in_reply_to,
            attachments: email
                .attachments
                .iter()
                .map(|attachment| OutgoingAttachment {
                    filename: attachment.filename.clone(),
                    content_type: attachment.content_type.trim().to_lowercase(),
                    url: attachment.url.clone(),
                })
                .collect(),
        })
    }

    fn message(&self, email: &Cleaned, recipients: &Recipients, body: String) -> OutgoingEmail {
        OutgoingEmail {
            from_name: self.sender.name.clone(),
            from_address: self.sender.address.clone(),
            to: recipients.to.clone(),
            cc: recipients.cc.clone(),
            bcc: recipients.bcc.clone(),
            subject: email.subject.clone(),
            body,
            signature: self.sender.signature.clone(),
            reply_to: Vec::new(),
            in_reply_to: self.in_reply_to.clone(),
            references: self.references.clone(),
            attachments: self.attachments.clone(),
            transactional: false,
        }
    }
}

#[allow(clippy::too_many_lines)]
pub async fn deliver(
    state: &AppState,
    user: &SessionUser,
    email: &Cleaned,
) -> ApiResult<SendEmailResult> {
    let pool = &state.pool;
    let deal_customer = deal_customer(pool, user, email).await?;
    if let Some(name) =
        unauthorized_attachment(pool, &state.session_secret, user, &email.attachments).await?
    {
        return Err(ApiError::BadRequest(format!(
            "Attachment is not available: {name}"
        )));
    }
    let mut created: Option<Created> = None;
    let mut signature_customer = None;
    if !email.signature_requests.is_empty() {
        signatures::check_access(user)?;
        if email.batches().len() > 1 {
            return Err(ApiError::BadRequest(
                "Send signature emails as one message so the signature request is not duplicated."
                    .to_string(),
            ));
        }
        let files = signatures::load_files(state, &email.attachments).await?;
        let matched = signatures::match_attachments(&email.signature_requests, &email.attachments)?;
        signature_customer =
            signatures::associated_customer(state, user, email, deal_customer).await?;
        created = Some(
            signatures::create(state, user, email, &files, &matched, signature_customer).await?,
        );
    }
    let envelope = match Envelope::new(pool, user, email).await {
        Ok(envelope) => envelope,
        Err(err) => {
            if let Some(created) = &created {
                created.rollback(state).await;
            }
            return Err(err);
        }
    };
    let sender = &envelope.sender;
    let branding = match &created {
        Some(_) => Some(signatures::branding(state, user).await?),
        None => None,
    };

    let mut sent = Vec::new();
    for recipients in email.batches() {
        let mut thread_id = email
            .thread_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let mut body = personalized_body(pool, user, email, recipients.to.first()).await?;
        let mut outgoing = envelope.message(email, &recipients, body.clone());
        if let Some(created) = &created {
            body = super::signature_mail::with_signing_links(
                &body,
                &created.links(),
                branding.as_ref(),
            );
            outgoing.body.clone_from(&body);
            outgoing.attachments = email
                .attachments
                .iter()
                .enumerate()
                .filter(|(index, attachment)| {
                    created.keeps_attachment(*index, attachment, &email.signature_requests)
                })
                .map(|(_, attachment)| OutgoingAttachment {
                    filename: attachment.filename.clone(),
                    content_type: attachment.content_type.trim().to_lowercase(),
                    url: attachment.url.clone(),
                })
                .collect();
        }
        let message_id = if let Some(Created::Native { signing, documents }) = &created {
            let snapshot = native_snapshot(&envelope, &outgoing, &body, &thread_id);
            let conflict = |message: String| ApiError::Conflict(message);
            match native_delivery::claim(state, signing, documents, &snapshot).await {
                Err(err) => {
                    if let Some(created) = &created {
                        created.rollback(state).await;
                    }
                    return Err(match err {
                        native_delivery::NativeError::Database(err) => ApiError::Database(err),
                        native_delivery::NativeError::Refused(message) => conflict(message),
                    });
                }
                Ok(native_delivery::Claim::Sent(receipt)) => {
                    let receipt = *receipt;
                    body = receipt.snapshot.personalized_body;
                    thread_id = receipt.snapshot.thread_id;
                    receipt.message_id
                }
                Ok(native_delivery::Claim::Started(claim)) => {
                    match state.webhooks.send_email(&outgoing).await {
                        Ok(id) => {
                            let id = unwrap_brackets(&id);
                            let receipt = crate::native_signing::MailReceipt {
                                snapshot,
                                message_id: id.clone(),
                            };
                            native_delivery::accept(state, signing, documents, &claim, &receipt)
                                .await
                                .map_err(|err| conflict(err.to_string()))?;
                            id
                        }
                        Err(failure) => {
                            tracing::error!(
                                ?failure,
                                user_id = user.id,
                                "signature invitation email failed"
                            );
                            let definite = matches!(failure, SendFailure::Rejected(_));
                            if let Err(err) =
                                native_delivery::fail(state, documents, definite, Some(&claim))
                                    .await
                            {
                                tracing::error!(%err, "native signature delivery not marked failed");
                            }
                            return Err(conflict(if definite {
                                "Email was rejected. Unsent signing links were revoked.".to_string()
                            } else {
                                "Email delivery is uncertain; signing links were retained. Reconciliation is required.".to_string()
                            }));
                        }
                    }
                }
            }
        } else {
            match state.webhooks.send_email(&outgoing).await {
                Ok(id) => unwrap_brackets(&id),
                Err(failure) if sent.is_empty() => {
                    if let Some(created) = &created {
                        created.rollback(state).await;
                    }
                    return Err(match failure {
                        SendFailure::Rejected(reason) => ApiError::BadRequest(reason),
                        SendFailure::Failed(_) => {
                            tracing::error!(?failure, user_id = user.id, "email send failed");
                            ApiError::Upstream("Email failed to send".to_string())
                        }
                    });
                }
                Err(failure) => {
                    tracing::error!(?failure, user_id = user.id, "email send failed");
                    // Earlier messages are out: record them, then report.
                    break;
                }
            }
        };
        sent.push(SentMessage {
            message_id,
            sender_address: sender.address.clone(),
            recipients,
            thread_id,
            body,
        });
    }

    let mut recorded = 0;
    for message in &sent {
        match record(
            pool,
            user,
            email,
            deal_customer,
            sender.name.as_deref(),
            message,
            created
                .as_ref()
                .map(|created| (created, signature_customer)),
        )
        .await
        {
            Ok(()) => recorded += 1,
            Err(err) => tracing::error!(?err, user_id = user.id, "sent email not recorded"),
        }
    }
    record_template_usage(pool, user, email.template_id).await;
    if sent.len() < email.batches().len() {
        return Err(ApiError::Upstream(format!(
            "Email failed to send to some recipients ({} of {} sent)",
            sent.len(),
            email.batches().len()
        )));
    }
    if recorded < sent.len() {
        return Err(ApiError::Internal(
            "Email was sent but could not be saved to the conversation".to_string(),
        ));
    }
    Ok(SendEmailResult {
        sent: sent.len(),
        delayed: false,
        thread_ids: sent.into_iter().map(|message| message.thread_id).collect(),
    })
}

fn unwrap_brackets(id: &str) -> String {
    id.strip_prefix('<')
        .and_then(|rest| rest.strip_suffix('>'))
        .unwrap_or(id)
        .to_string()
}

/// The note with `{{variables}}` filled for the primary recipient.
async fn personalized_body(
    pool: &MySqlPool,
    user: &SessionUser,
    email: &Cleaned,
    primary: Option<&String>,
) -> ApiResult<String> {
    if !email.body.contains("{{") {
        return Ok(email.body.clone());
    }
    let customer_id = match primary {
        Some(address) => customer_by_address(pool, address, user.company_id).await?,
        None => None,
    };
    match fetch_template_variable_data(pool, user.id, email.deal_id, customer_id, user.company_id)
        .await
    {
        Ok(data) => Ok(replace_template_variables(&email.body, &data)),
        Err(err) => {
            tracing::warn!(?err, "template variables unavailable");
            Ok(email.body.clone())
        }
    }
}

/// Writes one sent message: the email, its participants and attachments,
/// and the deal customer's new addresses; all or nothing.
#[allow(clippy::too_many_lines)]
async fn record(
    pool: &MySqlPool,
    user: &SessionUser,
    email: &Cleaned,
    deal_customer: Option<i32>,
    sender_name: Option<&str>,
    message: &SentMessage,
    signed: Option<(&Created, Option<i32>)>,
) -> Result<(), String> {
    let mut tx = pool.begin().await.map_err(|err| err.to_string())?;
    if let Some((created, _)) = signed
        && signatures::already_recorded(&mut tx, created).await?
    {
        return tx.commit().await.map_err(|err| err.to_string());
    }
    let email_id = sqlx::query(
        "INSERT INTO emails (sender_user_id, subject, body, message_id, sender_email, receiver_email, thread_id, deal_id, company_id, is_automated)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(user.id)
    .bind(&email.subject)
    .bind(&message.body)
    .bind(&message.message_id)
    .bind(&message.sender_address)
    .bind(message.recipients.to.first().cloned().unwrap_or_default())
    .bind(&message.thread_id)
    .bind(email.deal_id)
    .bind(user.company_id)
    .bind(email.automated)
    .execute(&mut *tx)
    .await
    .map_err(|err| err.to_string())?
    .last_insert_id();
    let email_id = i32::try_from(email_id).unwrap_or(i32::MAX);
    if let Some(customer_id) = deal_customer {
        add_customer_emails(&mut tx, customer_id, &message.recipients.to, user.id)
            .await
            .map_err(|err| err.to_string())?;
    }
    let from_name = sender_name
        .map(str::to_string)
        .or_else(|| user.name.clone());
    insert_participants(
        &mut tx,
        email_id,
        user.company_id,
        &[
            ("from", vec![(message.sender_address.clone(), from_name)]),
            (
                "to",
                message
                    .recipients
                    .to
                    .iter()
                    .map(|a| (a.clone(), None))
                    .collect(),
            ),
            (
                "cc",
                message
                    .recipients
                    .cc
                    .iter()
                    .map(|a| (a.clone(), None))
                    .collect(),
            ),
            (
                "bcc",
                message
                    .recipients
                    .bcc
                    .iter()
                    .map(|a| (a.clone(), None))
                    .collect(),
            ),
        ],
    )
    .await
    .map_err(|err| err.to_string())?;
    let mut stored = Vec::with_capacity(email.attachments.len());
    for attachment in &email.attachments {
        let (kind, sub) = split_content_type(&attachment.content_type);
        let id = sqlx::query(
            "INSERT INTO email_attachments (email_id, content_type, content_subtype, filename, url)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(email_id)
        .bind(kind)
        .bind(sub)
        .bind(&attachment.filename)
        .bind(&attachment.url)
        .execute(&mut *tx)
        .await
        .map_err(|err| err.to_string())?
        .last_insert_id();
        stored.push((
            i64::try_from(id).unwrap_or(i64::MAX),
            attachment.filename.clone(),
            attachment.url.clone(),
        ));
    }
    if let Some((created, customer_id)) = signed {
        signatures::record(
            &mut tx,
            created,
            user,
            email,
            customer_id,
            i64::from(email_id),
            &stored,
        )
        .await?;
    }
    tx.commit().await.map_err(|err| err.to_string())
}

/// The invitation mail as it goes out (`emailToSend`), sealed with a
/// native send.
fn native_snapshot(
    envelope: &Envelope,
    outgoing: &OutgoingEmail,
    body: &str,
    thread_id: &str,
) -> crate::native_signing::MailSnapshot {
    let bodies =
        common::utils::email_body::outgoing_bodies(body, envelope.sender.signature.as_deref());
    let from = envelope.sender.name.as_deref().map_or_else(
        || envelope.sender.address.clone(),
        |name| format!("\"{name}\" <{}>", envelope.sender.address),
    );
    crate::native_signing::MailSnapshot {
        subject: outgoing.subject.clone(),
        personalized_body: body.to_string(),
        thread_id: thread_id.to_string(),
        from,
        to: outgoing.to.clone(),
        cc: outgoing.cc.clone(),
        bcc: outgoing.bcc.clone(),
        html: bodies.html,
        text: bodies.text,
        in_reply_to: envelope.in_reply_to.clone(),
        references: envelope.references.clone(),
    }
}

pub(super) type ParticipantGroup = (&'static str, Vec<(String, Option<String>)>);

/// `insertEmailParticipants`: one row per distinct address of each group,
/// linked to the company's user or customer with that address.
pub(super) async fn insert_participants(
    tx: &mut Transaction<'_, MySql>,
    email_id: i32,
    company_id: i32,
    groups: &[ParticipantGroup],
) -> Result<(), sqlx::Error> {
    let groups: Vec<ParticipantGroup> = groups
        .iter()
        .map(|(kind, entries)| {
            let mut seen = HashSet::new();
            let items = entries
                .iter()
                .map(|(address, name)| {
                    (
                        normalize_address(Some(address)),
                        name.as_deref()
                            .map(str::trim)
                            .filter(|n| !n.is_empty())
                            .map(str::to_string),
                    )
                })
                .filter(|(address, _)| !address.is_empty() && seen.insert(address.clone()))
                .collect();
            (*kind, items)
        })
        .collect();
    let addresses: Vec<String> = groups
        .iter()
        .flat_map(|(_, items)| items.iter().map(|(address, _)| address.clone()))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    if addresses.is_empty() {
        return Ok(());
    }
    let placeholders = vec!["?"; addresses.len()].join(",");
    let mut users = sqlx::query_as::<_, (String, i32)>(AssertSqlSafe(format!(
        "SELECT LOWER(TRIM(email)), id FROM users
         WHERE is_deleted = 0 AND company_id = ? AND LOWER(TRIM(email)) IN ({placeholders})"
    )))
    .bind(company_id);
    for address in &addresses {
        users = users.bind(address);
    }
    let users: HashMap<String, i32> = users.fetch_all(&mut **tx).await?.into_iter().collect();
    let customer_address = normalized("ce.email");
    let mut customers = sqlx::query_as::<_, (String, i32)>(AssertSqlSafe(format!(
        "SELECT {customer_address}, MIN(c.id) FROM customers c
         JOIN customers_emails ce ON ce.customer_id = c.id
         WHERE c.company_id = ? AND c.deleted_at IS NULL AND {customer_address} IN ({placeholders})
         GROUP BY {customer_address}"
    )))
    .bind(company_id);
    for address in &addresses {
        customers = customers.bind(address);
    }
    let customers: HashMap<String, i32> =
        customers.fetch_all(&mut **tx).await?.into_iter().collect();

    for (kind, items) in &groups {
        for (position, (address, name)) in items.iter().enumerate() {
            sqlx::query(
                "INSERT INTO email_participants
                   (email_id, type, email, display_name, user_id, customer_id, position)
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(email_id)
            .bind(*kind)
            .bind(address)
            .bind(name)
            .bind(users.get(address))
            .bind(customers.get(address))
            .bind(i32::try_from(position).unwrap_or(i32::MAX))
            .execute(&mut **tx)
            .await?;
        }
    }
    Ok(())
}

/// `addCustomerEmails`: the recipients' new addresses go on the customer,
/// unless another customer of the company has them; the first becomes the
/// primary when the customer has none.
async fn add_customer_emails(
    tx: &mut Transaction<'_, MySql>,
    customer_id: i32,
    addresses: &[String],
    created_by: i32,
) -> Result<(), sqlx::Error> {
    let existing: Vec<(i32, String)> = sqlx::query_as(
        "SELECT id, email FROM customers_emails WHERE customer_id = ? ORDER BY created_at ASC, id ASC",
    )
    .bind(customer_id)
    .fetch_all(&mut **tx)
    .await?;
    let mut known: HashSet<String> = existing.iter().map(|(_, e)| e.to_lowercase()).collect();
    let mut first_id = existing.first().map(|(id, _)| *id);
    for address in addresses {
        let address = address.trim();
        if address.is_empty() || known.contains(&address.to_lowercase()) {
            continue;
        }
        let taken: Option<i32> = sqlx::query_scalar(
            "SELECT other.id FROM customers target
             JOIN customers other ON other.company_id = target.company_id
              AND other.id <> target.id AND other.deleted_at IS NULL
             JOIN customers_emails ce ON ce.customer_id = other.id
             WHERE target.id = ? AND LOWER(TRIM(ce.email)) = LOWER(TRIM(?))
             LIMIT 1",
        )
        .bind(customer_id)
        .bind(address)
        .fetch_optional(&mut **tx)
        .await?;
        if taken.is_some() {
            continue;
        }
        let id = sqlx::query(
            "INSERT INTO customers_emails (customer_id, email, created_by) VALUES (?, ?, ?)",
        )
        .bind(customer_id)
        .bind(address)
        .bind(created_by)
        .execute(&mut **tx)
        .await?
        .last_insert_id();
        known.insert(address.to_lowercase());
        first_id = first_id.or_else(|| i32::try_from(id).ok());
    }
    if let Some(first_id) = first_id {
        sqlx::query("UPDATE customers SET email_id = ? WHERE id = ? AND email_id IS NULL")
            .bind(first_id)
            .bind(customer_id)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

/// Counts a use of the caller's email template (best effort).
async fn record_template_usage(pool: &MySqlPool, user: &SessionUser, template_id: Option<i32>) {
    let Some(template_id) = template_id else {
        return;
    };
    let result = sqlx::query(
        "INSERT INTO user_template_usage (user_id, template_type, template_id, usage_count, last_used_at)
         SELECT ?, 'email', t.id, 1, NOW() FROM email_templates t
         WHERE t.id = ? AND t.company_id = ? AND t.deleted_at IS NULL
           AND (t.user_id IS NULL OR t.user_id = ?)
         ON DUPLICATE KEY UPDATE usage_count = usage_count + 1, last_used_at = NOW()",
    )
    .bind(user.id)
    .bind(template_id)
    .bind(user.company_id)
    .bind(user.id)
    .execute(pool)
    .await;
    if let Err(err) = result {
        tracing::warn!(?err, "template usage not recorded");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(to: &[&str]) -> SendEmailRequest {
        SendEmailRequest {
            to: to.iter().map(|s| (*s).to_string()).collect(),
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: "Hi".into(),
            body: "<p>Hello</p>".into(),
            thread_id: None,
            deal_id: None,
            customer_id: None,
            send_together: false,
            attachments: Vec::new(),
            template_id: None,
            send_at: None,
            cancel_if_customer_messages: false,
            signature_requests: Vec::new(),
            signature_send_key: None,
        }
    }

    #[test]
    fn recipients_are_cleaned_and_deduplicated_across_fields() {
        let mut req = request(&[" A@x.com", "a@x.com", "b@x.com"]);
        req.cc = vec!["B@x.com".into(), "c@x.com".into()];
        req.bcc = vec!["c@x.com".into(), "d@x.com".into()];
        let email = Cleaned::from_request(req).unwrap();
        assert_eq!(email.to, ["a@x.com", "b@x.com"]);
        assert_eq!(email.cc, ["c@x.com"]);
        assert_eq!(email.bcc, ["d@x.com"]);
    }

    #[test]
    fn several_recipients_get_their_own_message_unless_together() {
        let email = Cleaned::from_request(request(&["a@x.com", "b@x.com"])).unwrap();
        assert_eq!(email.batches().len(), 2);
        let mut together = request(&["a@x.com", "b@x.com"]);
        together.send_together = true;
        assert_eq!(Cleaned::from_request(together).unwrap().batches().len(), 1);
        let mut threaded = request(&["a@x.com", "b@x.com"]);
        threaded.thread_id = Some("t".into());
        assert_eq!(Cleaned::from_request(threaded).unwrap().batches().len(), 1);
    }

    #[test]
    fn invalid_requests() {
        assert!(Cleaned::from_request(request(&[])).is_err());
        assert!(Cleaned::from_request(request(&["not-an-address"])).is_err());
        let mut empty = request(&["a@x.com"]);
        empty.body = "<p><br></p>".into();
        assert!(Cleaned::from_request(empty).is_err());
        let mut past = request(&["a@x.com"]);
        past.send_at = Some(Utc::now() - Duration::minutes(1));
        assert!(Cleaned::from_request(past).is_err());
        let mut far = request(&["a@x.com"]);
        far.send_at = Some(Utc::now() + Duration::days(91));
        assert!(Cleaned::from_request(far).is_err());
        let mut many = request(&["a@x.com"]);
        many.cc = (0..26).map(|i| format!("c{i}@x.com")).collect();
        assert!(Cleaned::from_request(many).is_err());
    }

    #[test]
    fn addresses() {
        assert!(valid_address("a.b+c@x.co"));
        assert!(!valid_address("a@x"));
        assert!(!valid_address("a b@x.com"));
        assert!(!valid_address("\"A\" <a@x.com>"));
        assert!(!valid_address("@x.com"));
    }

    #[test]
    fn content_types_split() {
        assert_eq!(
            split_content_type("Application/PDF"),
            ("application".into(), "pdf".into())
        );
        assert_eq!(
            split_content_type("weird"),
            ("application".into(), String::new())
        );
    }
}
