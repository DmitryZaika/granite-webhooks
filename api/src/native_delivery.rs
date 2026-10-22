//! Sending documents for native e-signature with the CRM's email.
//!
//! Each PDF is prepared and stored privately, with a request and invitation
//! per document, and the invitation mail is claimed, sent and recorded
//! exactly once. Port of the CRM's `NativeSignatureDeliveryService` (prepare, send, fail,
//! rollback, lockHistory, attachHistory) and `insertNativeRequest`. A send
//! is identified by its logical key (`composer:<uuid>` from the composer,
//! `delayed:<id>` for a scheduled email) and the hash of what it carries,
//! so a retried request reuses the same documents and links. The signing
//! pages (`/sign/...`) stay in the CRM and read what is written here.

use crate::native_signing::{MailReceipt, MailSnapshot, NativeSigning, sha256_hex};
use crate::signing_pdf::{FieldKind, SignatureField, prepare_native};
use crate::state::AppState;
use chrono::{Duration, Utc};
use serde_json::{Value, json};
use sqlx::{MySql, Transaction};

/// Why a native send did not go out; worded for the sender.
#[derive(Debug, thiserror::Error)]
pub enum NativeError {
    #[error("{0}")]
    Refused(String),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

fn refused(message: &str) -> NativeError {
    NativeError::Refused(message.to_string())
}

/// One document of a send.
pub struct NativeItem<'a> {
    pub attachment_index: usize,
    /// The uploaded file's URL, or `raw:<index>`.
    pub source_identity: String,
    pub filename: &'a str,
    pub url: &'a str,
    pub file: &'a [u8],
    pub signer_name: &'a str,
    pub signer_email: &'a str,
    pub fields: &'a [SignatureField],
    pub reminders: bool,
}

/// A send to prepare.
pub struct NativeSend<'a> {
    pub logical_send_key: String,
    /// sha256 of the mail's recipients, text and attachments.
    pub send_fingerprint: String,
    pub company_id: i32,
    pub customer_id: Option<i32>,
    pub deal_id: Option<i32>,
    pub created_by_user_id: i32,
    pub items: Vec<NativeItem<'a>>,
}

/// A prepared document: its request, invitation link and delivery.
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedNative {
    pub company_id: i32,
    pub created_by_user_id: i32,
    pub delivery_id: i64,
    pub request_id: i64,
    pub attachment_index: usize,
    pub filename: String,
    pub url: String,
    pub signer_name: String,
    pub signer_email: String,
    pub fields: Vec<SignatureField>,
    pub provider_document_id: String,
    pub signing_url: String,
}

/// The per-company signing settings verified for native signing
/// (`SIGNATURE_VERIFIED_COMPANY_SETTINGS`: `{"<company>": {verifiedAt,
/// evidenceReference, settings}}`).
fn verified_settings(company_id: i32) -> Result<Value, NativeError> {
    let pending = || refused("Verified signature account settings are pending");
    let configs: Value = serde_json::from_str(
        &std::env::var("SIGNATURE_VERIFIED_COMPANY_SETTINGS").unwrap_or_else(|_| "{}".into()),
    )
    .map_err(|_| pending())?;
    let entry = configs.get(company_id.to_string()).ok_or_else(pending)?;
    let verified_at = entry["verifiedAt"].as_str().unwrap_or("");
    let evidence = entry["evidenceReference"].as_str().map_or("", str::trim);
    if chrono::DateTime::parse_from_rfc3339(verified_at).is_err() || evidence.is_empty() {
        return Err(pending());
    }
    let settings = entry["settings"].clone();
    if !frozen_settings_valid(&settings) {
        return Err(pending());
    }
    Ok(settings)
}

/// `frozenSigningSettingsSchema`.
pub fn frozen_settings_valid(settings: &Value) -> bool {
    let methods = settings["signatureMethods"].as_array();
    let bools = ["allowDecline", "allowReassign", "reminders", "auditPage"]
        .iter()
        .all(|key| settings[*key].is_boolean());
    let timezone = settings["timezone"]
        .as_str()
        .is_some_and(|tz| tz.parse::<chrono_tz::Tz>().is_ok());
    let expires = settings["expiresInDays"].is_null()
        || settings["expiresInDays"]
            .as_i64()
            .is_some_and(|days| days > 0);
    let notifications = ["viewed", "completed", "declined"]
        .iter()
        .all(|key| settings["notifications"][*key].is_boolean());
    settings["parityVerified"] == Value::Bool(true)
        && timezone
        && methods.is_some_and(|m| {
            !m.is_empty()
                && m.iter()
                    .all(|v| matches!(v.as_str(), Some("type" | "draw" | "upload")))
        })
        && matches!(
            settings["authentication"].as_str(),
            Some("invitation" | "email_code")
        )
        && bools
        && expires
        && notifications
        && crate::native_signing::FrozenSettings::parse(settings).is_some()
}

/// The company may sign and the user may send (`authorize`).
async fn authorize<'e, E>(executor: E, company_id: i32, user_id: i32) -> Result<(), NativeError>
where
    E: sqlx::Executor<'e, Database = MySql>,
{
    let owner: Option<(Option<i8>, Option<String>, i8)> = sqlx::query_as(
        "SELECT c.signature_enabled, c.signwell_api_key, u.is_signature_available
           FROM company c INNER JOIN users u ON u.company_id = c.id
          WHERE c.id = ? AND c.deleted_at IS NULL AND u.id = ? AND u.is_deleted = 0",
    )
    .bind(company_id)
    .bind(user_id)
    .fetch_optional(executor)
    .await?;
    let allowed = owner.is_some_and(|(enabled, key, available)| {
        signature_enabled(enabled, key.as_deref()) && available != 0
    });
    if allowed {
        Ok(())
    } else {
        Err(refused("Signature access denied"))
    }
}

/// `companySignatureEnabled`: switched on, or (never set) with a `SignWell` key.
pub fn signature_enabled(enabled: Option<i8>, signwell_key: Option<&str>) -> bool {
    enabled.map_or_else(
        || signwell_key.is_some_and(|key| !key.trim().is_empty()),
        |value| value != 0,
    )
}

fn key_valid(key: &str) -> bool {
    let lower = key.to_lowercase();
    if let Some(id) = lower.strip_prefix("composer:") {
        return id.len() == 36 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    }
    lower.strip_prefix("delayed:").is_some_and(|id| {
        !id.is_empty() && !id.starts_with('0') && id.chars().all(|c| c.is_ascii_digit())
    })
}

/// The payload hash of a send (`JSON.stringify` of its identity).
fn payload_hash(input: &NativeSend<'_>) -> String {
    let items: Vec<Value> = input
        .items
        .iter()
        .map(|item| {
            json!([
                item.attachment_index,
                item.source_identity,
                item.filename,
                item.signer_name,
                item.signer_email,
                item.fields,
                item.reminders,
                sha256_hex(item.file),
            ])
        })
        .collect();
    sha256_hex(
        json!([
            input.customer_id,
            input.deal_id,
            input.send_fingerprint,
            items
        ])
        .to_string()
        .as_bytes(),
    )
}

#[derive(sqlx::FromRow)]
struct DeliveryRow {
    id: i64,
    company_id: i32,
    created_by_user_id: i32,
    payload_sha256: String,
    state: String,
    encrypted_mail: Option<String>,
    lease_owner: Option<String>,
}

const DELIVERY_COLUMNS: &str =
    "id, company_id, created_by_user_id, payload_sha256, CAST(state AS CHAR) AS state,
    encrypted_mail, lease_owner";

/// The documents of an earlier preparation of the same send.
async fn existing(
    state: &AppState,
    signing: &NativeSigning,
    input: &NativeSend<'_>,
    key_hash: &str,
    payload: &str,
) -> Result<Option<Vec<PreparedNative>>, NativeError> {
    #[derive(sqlx::FromRow)]
    struct Item {
        attachment_index: i64,
        id: i64,
        filename: String,
        file_url: String,
        signer_name: String,
        signer_email: String,
        provider_document_id: String,
        fields_json: String,
        token_hash: String,
    }
    let row: Option<DeliveryRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {DELIVERY_COLUMNS} FROM native_signature_deliveries
          WHERE company_id = ? AND created_by_user_id = ? AND logical_send_hash = ?"
    )))
    .bind(input.company_id)
    .bind(input.created_by_user_id)
    .bind(key_hash)
    .fetch_optional(&state.pool)
    .await?;
    let Some(row) = row else { return Ok(None) };
    if row.payload_sha256 != payload {
        return Err(refused("Signature send contents changed; start a new send"));
    }
    let items: Vec<Item> = sqlx::query_as(
        "SELECT CAST(di.attachment_index AS SIGNED) AS attachment_index, CAST(sr.id AS SIGNED) AS id, sr.filename,
                sr.file_url, sr.signer_name, sr.signer_email, sr.provider_document_id,
                CAST(sr.fields_json AS CHAR) AS fields_json, nd.token_hash
           FROM native_signature_delivery_items di
           INNER JOIN signature_requests sr ON sr.id = di.request_id AND sr.company_id = di.company_id
           INNER JOIN native_signature_documents nd ON nd.request_id = sr.id AND nd.company_id = sr.company_id
          WHERE di.delivery_id = ? AND di.company_id = ? AND sr.created_by_user_id = ? AND sr.provider = 'native'
          ORDER BY di.attachment_index",
    )
    .bind(row.id)
    .bind(input.company_id)
    .bind(input.created_by_user_id)
    .fetch_all(&state.pool)
    .await?;
    if items.len() != input.items.len() {
        return Err(refused("Signature delivery snapshot is incomplete"));
    }
    items
        .into_iter()
        .map(|item| {
            let token = signing.token(
                i64::from(input.company_id),
                i64::from(input.created_by_user_id),
                key_hash,
                &item.provider_document_id,
            );
            if sha256_hex(token.as_bytes()) != item.token_hash {
                return Err(refused(
                    "Signature invitation key does not match retained snapshot",
                ));
            }
            Ok(PreparedNative {
                company_id: input.company_id,
                created_by_user_id: input.created_by_user_id,
                delivery_id: row.id,
                request_id: item.id,
                attachment_index: usize::try_from(item.attachment_index).unwrap_or(usize::MAX),
                filename: item.filename,
                url: item.file_url,
                signer_name: item.signer_name,
                signer_email: item.signer_email,
                fields: serde_json::from_str(&item.fields_json)
                    .map_err(|_| refused("Stored signing fields are invalid"))?,
                signing_url: signing.signing_url(&item.provider_document_id, &token),
                provider_document_id: item.provider_document_id,
            })
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

/// Prepares the documents of a send (or returns the earlier preparation of
/// the same send).
#[allow(clippy::too_many_lines)]
pub async fn prepare(
    state: &AppState,
    signing: &NativeSigning,
    input: &NativeSend<'_>,
) -> Result<Vec<PreparedNative>, NativeError> {
    if input.company_id <= 0 || input.created_by_user_id <= 0 || !key_valid(&input.logical_send_key)
    {
        return Err(refused("A stable logical send identity is required"));
    }
    if input.send_fingerprint.len() != 64
        || !input
            .send_fingerprint
            .chars()
            .all(|c| matches!(c, '0'..='9' | 'a'..='f'))
    {
        return Err(refused("Invalid send fingerprint"));
    }
    if input.items.is_empty() || input.items.len() > 20 {
        return Err(refused("Invalid signature batch"));
    }
    let mut settings = verified_settings(input.company_id)?;
    let methods = settings["signatureMethods"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if settings["authentication"] != "invitation"
        || settings["allowReassign"] == Value::Bool(true)
        || methods.iter().any(|m| m == "upload")
    {
        return Err(refused("Account signing parity is pending"));
    }
    let mut indexes: Vec<usize> = input
        .items
        .iter()
        .map(|item| item.attachment_index)
        .collect();
    indexes.sort_unstable();
    indexes.dedup();
    if indexes.len() != input.items.len() {
        return Err(refused("Duplicate attachment identity"));
    }
    let payload = payload_hash(input);
    let key_hash = sha256_hex(input.logical_send_key.as_bytes());
    authorize(&state.pool, input.company_id, input.created_by_user_id).await?;
    if let Some(prior) = existing(state, signing, input, &key_hash, &payload).await? {
        return Ok(prior);
    }
    // Every document is checked before anything is stored.
    let mut prepared = Vec::with_capacity(input.items.len());
    for item in &input.items {
        if !item.filename.to_lowercase().ends_with(".pdf") {
            return Err(refused("Signature documents must be PDFs"));
        }
        let file = item.file.to_vec();
        let fields = item.fields.to_vec();
        let pdf = tokio::task::spawn_blocking(move || prepare_native(&file, &fields))
            .await
            .map_err(|_| refused("Signature preparation failed"))?
            .map_err(|err| NativeError::Refused(err.message))?;
        prepared.push((uuid::Uuid::new_v4().to_string(), pdf));
    }
    let bucket = state.signature_bucket.as_deref().ok_or_else(|| {
        refused("Native signatures require a configured private SIGNATURE_STORAGE_BUCKET")
    })?;
    state
        .storage
        .verify_private(bucket)
        .await
        .map_err(|_| refused("Signature bucket must be private and block all public access"))?;
    let mut stored = Vec::with_capacity(prepared.len());
    for (document_id, pdf) in &prepared {
        let key = format!(
            "{}/signatures/{document_id}/prepared-{}.pdf",
            input.company_id, pdf.sha256
        );
        state
            .storage
            .put_private(bucket, &key, pdf.bytes.clone(), "application/pdf")
            .await
            .map_err(|_| refused("Could not store the signature document"))?;
        stored.push(key);
    }
    let expires_in_days = settings["expiresInDays"].as_i64();
    let result: Result<(), NativeError> = async {
        let mut tx = state.pool.begin().await?;
        let delivery = sqlx::query(
            "INSERT INTO native_signature_deliveries (company_id, created_by_user_id, logical_send_hash, payload_sha256, state)
             VALUES (?, ?, ?, ?, 'prepared')",
        )
        .bind(input.company_id)
        .bind(input.created_by_user_id)
        .bind(&key_hash)
        .bind(&payload)
        .execute(&mut *tx)
        .await?
        .last_insert_id();
        for ((item, (document_id, pdf)), key) in input.items.iter().zip(&prepared).zip(&stored) {
            settings["reminders"] = Value::Bool(item.reminders);
            let token = signing.token(
                i64::from(input.company_id),
                i64::from(input.created_by_user_id),
                &key_hash,
                document_id,
            );
            let request_id = insert_request(
                &mut tx,
                &NewRequest {
                    company_id: input.company_id,
                    user_id: input.created_by_user_id,
                    customer_id: input.customer_id,
                    deal_id: input.deal_id,
                    filename: item.filename,
                    file_url: item.url,
                    signer_name: item.signer_name,
                    signer_email: item.signer_email,
                    document_id,
                    prepared_object_key: key,
                    prepared_sha256: &pdf.sha256,
                    fields: &pdf.fields,
                    settings: &settings,
                    token_hash: &sha256_hex(token.as_bytes()),
                    expires_at: expires_in_days.map(|days| Utc::now() + Duration::days(days)),
                },
            )
            .await?;
            sqlx::query(
                "INSERT INTO native_signature_delivery_items (delivery_id, company_id, request_id, attachment_index) VALUES (?, ?, ?, ?)",
            )
            .bind(delivery)
            .bind(input.company_id)
            .bind(request_id)
            .bind(i64::try_from(item.attachment_index).unwrap_or(i64::MAX))
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }
    .await;
    if let Err(err) = result {
        // A competing preparation won the send key: use its documents.
        let duplicate = matches!(&err, NativeError::Database(sqlx::Error::Database(db)) if db.is_unique_violation());
        if !duplicate {
            return Err(err);
        }
        return existing(state, signing, input, &key_hash, &payload)
            .await?
            .ok_or(err);
    }
    existing(state, signing, input, &key_hash, &payload)
        .await?
        .ok_or_else(|| refused("Signature delivery snapshot is missing"))
}

struct NewRequest<'a> {
    company_id: i32,
    user_id: i32,
    customer_id: Option<i32>,
    deal_id: Option<i32>,
    filename: &'a str,
    file_url: &'a str,
    signer_name: &'a str,
    signer_email: &'a str,
    document_id: &'a str,
    prepared_object_key: &'a str,
    prepared_sha256: &'a str,
    fields: &'a [SignatureField],
    settings: &'a Value,
    token_hash: &'a str,
    expires_at: Option<chrono::DateTime<Utc>>,
}

/// A draft request and its native document (`insertNativeRequest`).
async fn insert_request(
    tx: &mut Transaction<'_, MySql>,
    value: &NewRequest<'_>,
) -> Result<i64, NativeError> {
    if !value
        .prepared_object_key
        .starts_with(&format!("{}/signatures/", value.company_id))
    {
        return Err(refused("Invalid company storage key"));
    }
    if value
        .fields
        .iter()
        .any(|field| field.kind == FieldKind::AutofillDateSigned && field.value.is_some())
    {
        return Err(refused("Signed date must be supplied by the server"));
    }
    let owner: Option<(Option<i8>, Option<String>, i8)> = sqlx::query_as(
        "SELECT c.signature_enabled, c.signwell_api_key, u.is_signature_available
           FROM company c INNER JOIN users u ON u.company_id = c.id
          WHERE c.id = ? AND c.deleted_at IS NULL AND u.id = ? AND u.is_deleted = 0
          FOR SHARE",
    )
    .bind(value.company_id)
    .bind(value.user_id)
    .fetch_optional(&mut **tx)
    .await?;
    if !owner.is_some_and(|(enabled, key, available)| {
        signature_enabled(enabled, key.as_deref()) && available != 0
    }) {
        return Err(refused("Signature access denied"));
    }
    if let Some(customer_id) = value.customer_id {
        let found: Option<i64> =
            sqlx::query_scalar("SELECT CAST(id AS SIGNED) FROM customers WHERE id = ? AND company_id = ? AND deleted_at IS NULL FOR SHARE")
                .bind(customer_id)
                .bind(value.company_id)
                .fetch_optional(&mut **tx)
                .await?;
        if found.is_none() {
            return Err(refused("Customer access denied"));
        }
    }
    if let Some(deal_id) = value.deal_id {
        let found: Option<i64> = sqlx::query_scalar(
            "SELECT CAST(d.id AS SIGNED) FROM deals d INNER JOIN customers c ON c.id = d.customer_id
              WHERE d.id = ? AND c.company_id = ? AND d.deleted_at IS NULL AND c.deleted_at IS NULL
                AND (? IS NULL OR c.id = ?) FOR SHARE",
        )
        .bind(deal_id)
        .bind(value.company_id)
        .bind(value.customer_id)
        .bind(value.customer_id)
        .fetch_optional(&mut **tx)
        .await?;
        if found.is_none() {
            return Err(refused("Deal access denied"));
        }
    }
    let fields = serde_json::to_string(value.fields).unwrap_or_else(|_| "[]".into());
    let request_id = sqlx::query(
        "INSERT INTO signature_requests
           (company_id, customer_id, deal_id, created_by_user_id, filename, file_url,
            signer_name, signer_email, status, provider, provider_document_id, fields_json)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'draft', 'native', ?, ?)",
    )
    .bind(value.company_id)
    .bind(value.customer_id)
    .bind(value.deal_id)
    .bind(value.user_id)
    .bind(value.filename)
    .bind(value.file_url)
    .bind(value.signer_name)
    .bind(value.signer_email)
    .bind(value.document_id)
    .bind(&fields)
    .execute(&mut **tx)
    .await?
    .last_insert_id();
    let request_id = i64::try_from(request_id).unwrap_or(i64::MAX);
    sqlx::query(
        "INSERT INTO native_signature_documents
           (request_id, company_id, document_id, prepared_object_key, prepared_sha256,
            document_version, token_hash, fields_json, settings_json, expires_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(request_id)
    .bind(value.company_id)
    .bind(value.document_id)
    .bind(value.prepared_object_key)
    .bind(value.prepared_sha256)
    .bind(value.prepared_sha256)
    .bind(value.token_hash)
    .bind(&fields)
    .bind(value.settings.to_string())
    .bind(value.expires_at.map(|at| at.naive_utc()))
    .execute(&mut **tx)
    .await?;
    Ok(request_id)
}

/// The delivery row of a set of documents, locked.
async fn lock(
    tx: &mut Transaction<'_, MySql>,
    docs: &[PreparedNative],
) -> Result<DeliveryRow, NativeError> {
    let first = docs
        .first()
        .ok_or_else(|| refused("Invalid signature delivery scope"))?;
    if docs.iter().any(|doc| {
        doc.delivery_id != first.delivery_id
            || doc.company_id != first.company_id
            || doc.created_by_user_id != first.created_by_user_id
    }) {
        return Err(refused("Invalid signature delivery scope"));
    }
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {DELIVERY_COLUMNS} FROM native_signature_deliveries
          WHERE id = ? AND company_id = ? AND created_by_user_id = ? FOR UPDATE"
    )))
    .bind(first.delivery_id)
    .bind(first.company_id)
    .bind(first.created_by_user_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| refused("Signature delivery access denied"))
}

/// What sending found.
pub enum Claim {
    /// The mail already went out (a retried send): its receipt.
    Sent(Box<MailReceipt>),
    /// The caller must send the mail now, then [`accept`] or [`fail`].
    Started(String),
}

/// Claims the send of the invitation mail (`send`, up to the transport).
pub async fn claim(
    state: &AppState,
    signing: &NativeSigning,
    docs: &[PreparedNative],
    snapshot: &MailSnapshot,
) -> Result<Claim, NativeError> {
    let claim_id = uuid::Uuid::new_v4().to_string();
    let mut tx = state.pool.begin().await?;
    let row = lock(&mut tx, docs).await?;
    authorize(&mut *tx, row.company_id, row.created_by_user_id).await?;
    if row.state == "accepted" || row.state == "recorded" {
        let receipt = row
            .encrypted_mail
            .as_deref()
            .and_then(|sealed| signing.open::<MailReceipt>(sealed))
            .ok_or_else(|| refused("Accepted mail snapshot is missing"))?;
        tx.commit().await?;
        return Ok(Claim::Sent(Box::new(receipt)));
    }
    if row.state == "sending" || row.state == "uncertain" {
        return Err(refused(
            "Email delivery is uncertain or in progress; reconciliation is required before another send",
        ));
    }
    if row.state != "prepared" {
        return Err(refused("Email was rejected; start a new send"));
    }
    // Every frozen request is still open: never mail a revoked link.
    let ready: Vec<(i64, String, Option<chrono::NaiveDateTime>)> = sqlx::query_as(
        "SELECT CAST(sr.id AS SIGNED), CAST(sr.status AS CHAR), CAST(nd.revoked_at AS DATETIME)
           FROM signature_requests sr
           INNER JOIN native_signature_documents nd ON nd.request_id = sr.id AND nd.company_id = sr.company_id
           INNER JOIN native_signature_delivery_items di ON di.request_id = sr.id AND di.company_id = sr.company_id
          WHERE di.delivery_id = ? AND di.company_id = ? AND sr.provider = 'native' ORDER BY sr.id FOR UPDATE",
    )
    .bind(row.id)
    .bind(row.company_id)
    .fetch_all(&mut *tx)
    .await?;
    if ready.len() != docs.len()
        || ready.iter().any(|(id, status, revoked)| {
            status != "draft" || revoked.is_some() || !docs.iter().any(|doc| doc.request_id == *id)
        })
    {
        return Err(refused(
            "Invitation is no longer available; no email was sent",
        ));
    }
    let started_at = Utc::now().naive_utc();
    sqlx::query(
        "UPDATE native_signature_documents nd INNER JOIN native_signature_delivery_items di ON di.request_id = nd.request_id AND di.company_id = nd.company_id
            SET nd.expires_at = CASE WHEN JSON_EXTRACT(nd.settings_json, '$.expiresInDays') = CAST('null' AS JSON) THEN NULL
                ELSE TIMESTAMPADD(DAY, CAST(JSON_UNQUOTE(JSON_EXTRACT(nd.settings_json, '$.expiresInDays')) AS UNSIGNED), ?) END
          WHERE di.delivery_id = ? AND di.company_id = ?",
    )
    .bind(started_at)
    .bind(row.id)
    .bind(row.company_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE native_signature_deliveries SET state = 'sending', lease_owner = ?, lease_until = ?, encrypted_mail = ?
          WHERE id = ? AND company_id = ?",
    )
    .bind(&claim_id)
    .bind((Utc::now() + Duration::seconds(120)).naive_utc())
    .bind(signing.seal(snapshot))
    .bind(row.id)
    .bind(row.company_id)
    .execute(&mut *tx)
    .await?;
    let started = sqlx::query(
        "UPDATE signature_requests sr INNER JOIN native_signature_delivery_items di ON di.request_id = sr.id AND di.company_id = sr.company_id
            SET sr.status = 'sent'
          WHERE di.delivery_id = ? AND di.company_id = ? AND sr.status = 'draft' AND sr.provider = 'native'",
    )
    .bind(row.id)
    .bind(row.company_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if usize::try_from(started).ok() != Some(docs.len()) {
        return Err(refused("Invitation changed before transport"));
    }
    tx.commit().await?;
    Ok(Claim::Started(claim_id))
}

/// The mail was accepted: the delivery and its documents are sent.
pub async fn accept(
    state: &AppState,
    signing: &NativeSigning,
    docs: &[PreparedNative],
    claim_id: &str,
    receipt: &MailReceipt,
) -> Result<(), NativeError> {
    let mut tx = state.pool.begin().await?;
    let row = lock(&mut tx, docs).await?;
    if row.state != "sending" || row.lease_owner.as_deref() != Some(claim_id) {
        return Err(refused("Mail acceptance reconciliation is required"));
    }
    let accepted_at = Utc::now().naive_utc();
    sqlx::query(
        "UPDATE native_signature_deliveries SET state = 'accepted', accepted_at = ?, encrypted_mail = ?, lease_owner = NULL, lease_until = NULL
          WHERE id = ? AND company_id = ?",
    )
    .bind(accepted_at)
    .bind(signing.seal(receipt))
    .bind(row.id)
    .bind(row.company_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE native_signature_documents nd INNER JOIN native_signature_delivery_items di ON di.request_id = nd.request_id AND di.company_id = nd.company_id
            SET nd.sent_at = COALESCE(nd.sent_at, ?), nd.expires_at = CASE
              WHEN JSON_EXTRACT(nd.settings_json, '$.expiresInDays') = CAST('null' AS JSON) THEN NULL
              ELSE TIMESTAMPADD(DAY, CAST(JSON_UNQUOTE(JSON_EXTRACT(nd.settings_json, '$.expiresInDays')) AS UNSIGNED), ?) END
          WHERE di.delivery_id = ? AND di.company_id = ?",
    )
    .bind(accepted_at)
    .bind(accepted_at)
    .bind(row.id)
    .bind(row.company_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// The mail did not go out. `definite` (refused): unsent links are revoked;
/// otherwise the delivery is uncertain and kept for reconciliation.
pub async fn fail(
    state: &AppState,
    docs: &[PreparedNative],
    definite: bool,
    claim_id: Option<&str>,
) -> Result<(), NativeError> {
    let mut tx = state.pool.begin().await?;
    let row = lock(&mut tx, docs).await?;
    if ["accepted", "recorded", "rejected"].contains(&row.state.as_str()) {
        return Ok(());
    }
    let owns = match claim_id {
        Some(claim) => row.lease_owner.as_deref() == Some(claim),
        None => row.state == "prepared",
    };
    if !owns {
        return Ok(());
    }
    sqlx::query("UPDATE native_signature_deliveries SET state = ?, lease_owner = NULL, lease_until = NULL WHERE id = ? AND company_id = ?")
        .bind(if definite { "rejected" } else { "uncertain" })
        .bind(row.id)
        .bind(row.company_id)
        .execute(&mut *tx)
        .await?;
    if definite {
        sqlx::query(
            "UPDATE native_signature_documents nd INNER JOIN signature_requests sr ON sr.id = nd.request_id AND sr.company_id = nd.company_id
             INNER JOIN native_signature_delivery_items di ON di.request_id = nd.request_id AND di.company_id = nd.company_id
                SET nd.revoked_at = ?, sr.status = 'expired'
              WHERE di.delivery_id = ? AND di.company_id = ? AND nd.sent_at IS NULL AND sr.status IN ('draft', 'sent', 'viewed')
                AND sr.provider = 'native'",
        )
        .bind(Utc::now().naive_utc())
        .bind(row.id)
        .bind(row.company_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// A preparation whose mail never started is revoked.
pub async fn rollback(state: &AppState, docs: &[PreparedNative]) -> Result<(), NativeError> {
    fail(state, docs, true, None).await
}

/// Inside the email's record transaction: `true` when the history is
/// already recorded (skip the record), an error unless the mail was accepted.
pub async fn lock_history(
    tx: &mut Transaction<'_, MySql>,
    docs: &[PreparedNative],
) -> Result<bool, NativeError> {
    let row = lock(tx, docs).await?;
    if row.state == "recorded" {
        return Ok(true);
    }
    if row.state != "accepted" {
        return Err(refused("Mail acceptance reconciliation is required"));
    }
    Ok(false)
}

/// Links each request to the recorded email and its attachment, and marks
/// the delivery recorded.
pub async fn attach_history(
    tx: &mut Transaction<'_, MySql>,
    docs: &[PreparedNative],
    email_id: i64,
    attachments: &[(i64, String, String)],
) -> Result<(), NativeError> {
    if lock_history(tx, docs).await? {
        return Err(refused("Email history is already recorded"));
    }
    for doc in docs {
        let Some((attachment_id, filename, url)) = attachments.get(doc.attachment_index) else {
            return Err(refused("Signature attachment identity mismatch"));
        };
        if *url != doc.url || *filename != doc.filename {
            return Err(refused("Signature attachment identity mismatch"));
        }
        let changed = sqlx::query(
            "UPDATE signature_requests sr INNER JOIN native_signature_delivery_items di ON di.request_id = sr.id AND di.company_id = sr.company_id
                SET sr.email_id = ?, sr.email_attachment_id = ?
              WHERE sr.id = ? AND sr.company_id = ? AND sr.created_by_user_id = ? AND sr.provider = 'native'
                AND di.delivery_id = ? AND di.attachment_index = ? AND sr.email_id IS NULL",
        )
        .bind(email_id)
        .bind(attachment_id)
        .bind(doc.request_id)
        .bind(doc.company_id)
        .bind(doc.created_by_user_id)
        .bind(doc.delivery_id)
        .bind(i64::try_from(doc.attachment_index).unwrap_or(i64::MAX))
        .execute(&mut **tx)
        .await?
        .rows_affected();
        if changed != 1 {
            return Err(refused("Signature history association failed"));
        }
    }
    let first = &docs[0];
    sqlx::query(
        "UPDATE native_signature_deliveries SET state = 'recorded', email_id = ?
          WHERE id = ? AND company_id = ? AND created_by_user_id = ? AND state = 'accepted'",
    )
    .bind(email_id)
    .bind(first.delivery_id)
    .bind(first.company_id)
    .bind(first.created_by_user_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_keys_are_composer_or_delayed() {
        assert!(key_valid("composer:6f1d2c3b-1a2b-4c5d-8e9f-0a1b2c3d4e5f"));
        assert!(key_valid("delayed:12"));
        assert!(!key_valid("delayed:012"));
        assert!(!key_valid("composer:nope"));
    }

    #[test]
    fn signing_is_on_when_switched_on_or_with_a_key() {
        assert!(signature_enabled(Some(1), None));
        assert!(!signature_enabled(Some(0), Some("key")));
        assert!(signature_enabled(None, Some("key")));
        assert!(!signature_enabled(None, Some(" ")));
    }
}
