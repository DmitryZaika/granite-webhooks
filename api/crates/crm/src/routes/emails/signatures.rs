//! Documents sent for e-signature with an employee's email.
//!
//! Through `SignWell`, or natively when `NATIVE_SIGNATURES_ENABLED`. The email
//! carries the signing links (the company's branded card), the unsigned PDF
//! is not attached, and each request is recorded with the email. Port of the
//! signature half of the CRM's `deliverEmployeeEmail`
//! (`createSignatureDocumentsForSend`, `insertSignatureRequestsForEmail`).

use super::schemas::{SendAttachment, SignatureRequestInput};
use super::send::Cleaned;
use crate::auth::session::SessionUser;
use crate::error::{ApiError, ApiResult};
use crate::native_delivery::{self, NativeError, NativeItem, NativeSend, PreparedNative};
use crate::native_signing::NativeSigning;
use crate::signing_pdf::{has_signer_signature, prepare_for_signwell};
use crate::signwell::{NewDocument, SignWell, api_key};
use crate::state::AppState;
use serde_json::json;
use sqlx::{MySql, Transaction};

/// A request checked like the CRM's schema.
pub fn check(request: &SignatureRequestInput) -> Result<(), &'static str> {
    let filename = request.filename.trim();
    let signer = request.signer_name.trim();
    let email = request.signer_email.trim();
    if filename.is_empty()
        || filename.chars().count() > 255
        || request
            .url
            .as_deref()
            .is_some_and(|url| url.trim().is_empty() || url.trim().chars().count() > 2000)
        || signer.is_empty()
        || signer.chars().count() > 255
        || email.chars().count() > 255
        || !super::send::valid_address(email)
        || request.fields.is_empty()
        || request.fields.len() > 20
    {
        return Err("Invalid signature request");
    }
    for field in &request.fields {
        field.check()?;
    }
    Ok(())
}

/// A file sent for signature gets a standard subject and message when it
/// has none (`withSignatureRequestDefaults`).
pub fn with_defaults(
    subject: &str,
    body: &str,
    requests: &[SignatureRequestInput],
) -> (String, String) {
    if requests.is_empty() {
        return (subject.to_string(), body.to_string());
    }
    let mut names: Vec<String> = Vec::new();
    for request in requests {
        let stem = request
            .filename
            .strip_suffix(".pdf")
            .or_else(|| request.filename.strip_suffix(".PDF"))
            .unwrap_or(&request.filename)
            .trim();
        let name = if stem.is_empty() {
            request.filename.clone()
        } else {
            stem.to_string()
        };
        if !names.contains(&name) {
            names.push(name);
        }
    }
    let subject = if subject.trim().is_empty() {
        let quoted: Vec<String> = names.iter().map(|name| format!("\"{name}\"")).collect();
        format!("Signature requested on {}", quoted.join(", "))
            .chars()
            .take(300)
            .collect()
    } else {
        subject.to_string()
    };
    let body = if super::send::is_empty_rich_text(body) {
        format!(
            "<p>Please review and sign the {}.</p>",
            if names.len() == 1 {
                "document"
            } else {
                "documents"
            }
        )
    } else {
        body.to_string()
    };
    (subject, body)
}

/// Each request with its attachment: by URL, else by file name
/// (`matchSignatureRequestsToAttachments`).
pub fn match_attachments(
    requests: &[SignatureRequestInput],
    attachments: &[SendAttachment],
) -> Result<Vec<usize>, ApiError> {
    let mut used: Vec<usize> = Vec::new();
    let mut matched = Vec::with_capacity(requests.len());
    for request in requests {
        let by_url = request
            .url
            .as_deref()
            .filter(|url| !url.is_empty())
            .and_then(|url| {
                (0..attachments.len())
                    .find(|index| !used.contains(index) && attachments[*index].url == url)
            });
        let index = by_url
            .or_else(|| {
                (0..attachments.len()).find(|index| {
                    !used.contains(index) && attachments[*index].filename == request.filename
                })
            })
            .ok_or_else(|| {
                ApiError::BadRequest(format!(
                    "Could not find the signature document {} among the email attachments.",
                    request.filename
                ))
            })?;
        used.push(index);
        matched.push(index);
    }
    Ok(matched)
}

/// Attachments that are not a signature document: the email carries their
/// link instead (`filesWithoutSignatureDocuments`).
pub fn is_signature_attachment(
    attachment: &SendAttachment,
    requests: &[SignatureRequestInput],
) -> bool {
    requests.iter().any(|request| {
        request
            .url
            .as_deref()
            .is_some_and(|url| !url.is_empty() && url == attachment.url)
            || request.filename == attachment.filename
    })
}

/// A `SignWell` document created for the send.
#[derive(Debug, Clone)]
pub struct SignWellDocument {
    pub filename: String,
    pub url: String,
    pub signer_name: String,
    pub signer_email: String,
    pub fields: Vec<crate::signing_pdf::SignatureField>,
    pub provider_document_id: String,
    pub signing_url: Option<String>,
}

/// The documents of a send.
pub enum Created {
    SignWell {
        key: String,
        documents: Vec<SignWellDocument>,
    },
    Native {
        signing: NativeSigning,
        documents: Vec<PreparedNative>,
    },
}

impl Created {
    /// (file name, signing link) per document.
    pub fn links(&self) -> Vec<(String, Option<String>)> {
        match self {
            Self::SignWell { documents, .. } => documents
                .iter()
                .map(|d| (d.filename.clone(), d.signing_url.clone()))
                .collect(),
            Self::Native { documents, .. } => documents
                .iter()
                .map(|d| (d.filename.clone(), Some(d.signing_url.clone())))
                .collect(),
        }
    }

    /// Attachments sent with the email.
    pub fn keeps_attachment(
        &self,
        index: usize,
        attachment: &SendAttachment,
        requests: &[SignatureRequestInput],
    ) -> bool {
        match self {
            Self::SignWell { .. } => !is_signature_attachment(attachment, requests),
            Self::Native { documents, .. } => {
                !documents.iter().any(|doc| doc.attachment_index == index)
            }
        }
    }

    /// Undoes the documents of a send whose mail did not go out.
    pub async fn rollback(&self, state: &AppState) {
        match self {
            Self::SignWell { key, documents } => {
                let signwell = SignWell::new(&state.signwell_base);
                for document in documents {
                    signwell.cancel(key, &document.provider_document_id).await;
                }
            }
            Self::Native { documents, .. } => {
                if let Err(err) = native_delivery::rollback(state, documents).await {
                    lambda_http::tracing::error!(%err, "native signature rollback failed");
                }
            }
        }
    }
}

/// The caller may send documents for signature: eSignatures are given per
/// user, with no company switch.
pub fn check_access(user: &SessionUser) -> ApiResult<()> {
    if !user.is_signature_available {
        return Err(ApiError::Denied(
            "You do not have access to send documents for signature. Ask a superuser to turn on eSignatures for you in Superuser › Feature access."
                .into(),
        ));
    }
    Ok(())
}

/// The bytes of each attachment.
pub async fn load_files(
    state: &AppState,
    attachments: &[SendAttachment],
) -> ApiResult<Vec<Vec<u8>>> {
    let mut files = Vec::with_capacity(attachments.len());
    for attachment in attachments {
        let object = state
            .storage
            .get_url(&attachment.url)
            .await
            .ok()
            .flatten()
            .ok_or_else(|| {
                ApiError::BadRequest(
                    "Could not load the document that requires a signature.".into(),
                )
            })?;
        files.push(object.bytes);
    }
    Ok(files)
}

/// The customer the requests belong to (`associatedCustomerId`).
pub async fn associated_customer(
    state: &AppState,
    user: &SessionUser,
    email: &Cleaned,
    deal_customer: Option<i32>,
) -> ApiResult<Option<i32>> {
    if deal_customer.is_some() {
        return Ok(deal_customer);
    }
    if let Some(customer_id) = email.customer_id {
        let found: Option<i32> =
            sqlx::query_scalar("SELECT id FROM customers WHERE id = ? AND company_id = ? AND deleted_at IS NULL LIMIT 1")
                .bind(customer_id)
                .bind(user.company_id)
                .fetch_optional(&state.pool)
                .await?;
        if found.is_some() {
            return Ok(found);
        }
    }
    match email.to.first() {
        Some(address) => {
            Ok(super::send::customer_by_address(&state.pool, address, user.company_id).await?)
        }
        None => Ok(None),
    }
}

/// What makes a native send the same send (`sendFingerprint`).
fn fingerprint(email: &Cleaned, files: &[Vec<u8>]) -> String {
    let attachments: Vec<serde_json::Value> = email
        .attachments
        .iter()
        .zip(files)
        .enumerate()
        .map(|(index, (attachment, file))| {
            json!({
                "index": index,
                "sourceIdentity": attachment.url,
                "filename": attachment.filename,
                "sha256": crate::native_signing::sha256_hex(file),
            })
        })
        .collect();
    crate::native_signing::sha256_hex(
        json!({
            "to": email.to,
            "cc": email.cc,
            "bcc": email.bcc,
            "subject": email.subject,
            "body": email.body,
            "threadId": email.thread_id,
            "sendTogether": email.send_together,
            "attachments": attachments,
        })
        .to_string()
        .as_bytes(),
    )
}

/// Creates the signature documents of the send.
#[allow(clippy::too_many_lines)]
pub async fn create(
    state: &AppState,
    user: &SessionUser,
    email: &Cleaned,
    files: &[Vec<u8>],
    matched: &[usize],
    customer_id: Option<i32>,
) -> ApiResult<Created> {
    let failed = |message: String| ApiError::Upstream(message);
    if let Some(signing) = state.native_signing.clone() {
        let key = match email.delayed_email_id {
            Some(id) => format!("delayed:{id}"),
            None => match email
                .signature_send_key
                .as_deref()
                .and_then(|key| uuid::Uuid::parse_str(key).ok())
            {
                Some(key) => format!("composer:{key}"),
                None => return Err(failed("A stable logical send identity is required".into())),
            },
        };
        let items: Vec<NativeItem<'_>> = email
            .signature_requests
            .iter()
            .zip(matched)
            .map(|(request, index)| NativeItem {
                attachment_index: *index,
                source_identity: email.attachments[*index].url.clone(),
                filename: &email.attachments[*index].filename,
                url: &email.attachments[*index].url,
                file: &files[*index],
                signer_name: &request.signer_name,
                signer_email: &request.signer_email,
                fields: &request.fields,
                reminders: request.reminders,
            })
            .collect();
        let send = NativeSend {
            logical_send_key: key,
            send_fingerprint: fingerprint(email, files),
            company_id: user.company_id,
            customer_id,
            deal_id: email.deal_id,
            created_by_user_id: user.id,
            items,
        };
        return match native_delivery::prepare(state, &signing, &send).await {
            Ok(documents) => Ok(Created::Native { signing, documents }),
            Err(NativeError::Database(err)) => Err(ApiError::Database(err)),
            Err(NativeError::Refused(message)) => Err(failed(message)),
        };
    }
    let company_key: Option<Option<String>> =
        sqlx::query_scalar("SELECT signwell_api_key FROM company WHERE id = ?")
            .bind(user.company_id)
            .fetch_optional(&state.pool)
            .await?;
    let key = api_key(company_key.flatten().as_deref()).map_err(|err| failed(err.0))?;
    let signwell = SignWell::new(&state.signwell_base);
    let mut documents: Vec<SignWellDocument> = Vec::new();
    let mut metadata = json!({ "company_id": user.company_id.to_string() });
    if let Some(customer_id) = customer_id {
        metadata["customer_id"] = customer_id.to_string().into();
    }
    if let Some(deal_id) = email.deal_id {
        metadata["deal_id"] = deal_id.to_string().into();
    }
    let requester_name = user.name.clone().unwrap_or_default();
    let result: Result<(), String> = async {
        for (request, index) in email.signature_requests.iter().zip(matched) {
            let attachment = &email.attachments[*index];
            if !attachment.filename.trim().to_lowercase().ends_with(".pdf") {
                return Err(format!("{} must be a PDF to request a signature.", attachment.filename));
            }
            if request.fields.iter().any(|f| f.kind == crate::signing_pdf::FieldKind::Signature) && !has_signer_signature(&request.fields) {
                return Err(format!(
                    "{} needs at least one signature for the customer. You signed them all.",
                    attachment.filename
                ));
            }
            if !has_signer_signature(&request.fields) {
                return Err(format!("{} needs a place for the customer to sign.", attachment.filename));
            }
            let file = files[*index].clone();
            let fields = request.fields.clone();
            let prepared = tokio::task::spawn_blocking(move || prepare_for_signwell(&file, &fields))
                .await
                .map_err(|_| "Could not create the signature request. The email was not sent.".to_string())?
                .map_err(|_| {
                    format!(
                        "Could not fill in the fields on {}. Clear the fields you filled in or try another PDF.",
                        attachment.filename
                    )
                })?;
            // The stored URL is the blank original: never let SignWell fall
            // back to it when we printed details onto this copy.
            let file_url = if prepared.stamped_text {
                None
            } else {
                Some(state.storage.readable_url(&attachment.url, Some("application/pdf")).await)
            };
            let name = attachment
                .filename
                .strip_suffix(".pdf")
                .or_else(|| attachment.filename.strip_suffix(".PDF"))
                .filter(|stem| !stem.is_empty())
                .unwrap_or(&attachment.filename);
            let created = signwell
                .create(
                    &key,
                    &NewDocument {
                        name,
                        filename: &attachment.filename,
                        file: &prepared.bytes,
                        file_url: file_url.as_deref(),
                        signer_name: request.signer_name.trim(),
                        signer_email: request.signer_email.trim(),
                        fields: &request.fields,
                        requester_name: Some(&requester_name),
                        requester_email: Some(&user.email),
                        reminders: request.reminders,
                        metadata: metadata.clone(),
                    },
                )
                .await
                .map_err(|err| err.0)?;
            documents.push(SignWellDocument {
                filename: attachment.filename.clone(),
                url: attachment.url.clone(),
                signer_name: request.signer_name.trim().to_string(),
                signer_email: request.signer_email.trim().to_string(),
                fields: request.fields.clone(),
                provider_document_id: created.id,
                signing_url: created.signing_url,
            });
        }
        Ok(())
    }
    .await;
    match result {
        Ok(()) => Ok(Created::SignWell { key, documents }),
        Err(message) => {
            for document in &documents {
                signwell.cancel(&key, &document.provider_document_id).await;
            }
            Err(failed(message))
        }
    }
}

/// The company's branding for the invitation card.
pub async fn branding(
    state: &AppState,
    user: &SessionUser,
) -> ApiResult<super::signature_mail::Branding> {
    let company: Option<(Option<String>, Option<String>)> =
        sqlx::query_as("SELECT name, logo_url FROM company WHERE id = ? LIMIT 1")
            .bind(user.company_id)
            .fetch_optional(&state.pool)
            .await?;
    let (name, logo_url) = company.unwrap_or_default();
    let sender_name = user
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map_or_else(|| user.email.clone(), str::to_string);
    Ok(super::signature_mail::Branding {
        company_name: name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map_or_else(|| sender_name.clone(), str::to_string),
        logo_url,
        sender_name,
    })
}

/// The requests of a recorded email (`insertSignatureRequestsForEmail`);
/// `attachments` are (id, file name, URL) in the email's order.
pub async fn record(
    tx: &mut Transaction<'_, MySql>,
    created: &Created,
    user: &SessionUser,
    email: &Cleaned,
    customer_id: Option<i32>,
    email_id: i64,
    attachments: &[(i64, String, String)],
) -> Result<(), String> {
    match created {
        Created::Native { documents, .. } => {
            native_delivery::attach_history(tx, documents, email_id, attachments)
                .await
                .map_err(|err| err.to_string())
        }
        Created::SignWell { documents, .. } => {
            for document in documents {
                let attachment = attachments
                    .iter()
                    .find(|(_, filename, url)| {
                        *url == document.url && *filename == document.filename
                    })
                    .or_else(|| {
                        attachments
                            .iter()
                            .find(|(_, filename, _)| *filename == document.filename)
                    })
                    .or_else(|| attachments.iter().find(|(_, _, url)| *url == document.url));
                sqlx::query(
                    "INSERT INTO signature_requests (
                       company_id, customer_id, deal_id, email_id, email_attachment_id,
                       created_by_user_id, filename, file_url, signer_name, signer_email,
                       status, provider, provider_document_id, fields_json
                     ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'sent', 'signwell', ?, ?)",
                )
                .bind(user.company_id)
                .bind(customer_id)
                .bind(email.deal_id)
                .bind(email_id)
                .bind(attachment.map(|(id, _, _)| *id))
                .bind(user.id)
                .bind(&document.filename)
                .bind(&document.url)
                .bind(&document.signer_name)
                .bind(&document.signer_email)
                .bind(&document.provider_document_id)
                .bind(serde_json::to_string(&document.fields).unwrap_or_else(|_| "[]".into()))
                .execute(&mut **tx)
                .await
                .map_err(|err| err.to_string())?;
            }
            Ok(())
        }
    }
}

/// Native sends: the history was already recorded (by a retried send).
pub async fn already_recorded(
    tx: &mut Transaction<'_, MySql>,
    created: &Created,
) -> Result<bool, String> {
    match created {
        Created::Native { documents, .. } => native_delivery::lock_history(tx, documents)
            .await
            .map_err(|err| err.to_string()),
        Created::SignWell { .. } => Ok(false),
    }
}
