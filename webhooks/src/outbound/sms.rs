//! Sends a text an employee (or an automation) wrote.
//!
//! The `api` Lambda checks the request; this route stores each message as
//! `pending`, calls the provider and records the outcome, so the provider's
//! echo webhook can be matched while the call is in flight. `CloudTalk` gets
//! one plain text with links to the files; `RingCentral` gets real MMS,
//! split to what carriers deliver, and a file too big for MMS as a link.
//!
//! - `POST /sms/send` (Remix key) → `{ "ids": [<row id>, ...] }`
//!
//! Port of the CRM's `deliverCloudTalkSms` / `deliverRingCentralSms`.

use crate::axum_helpers::guards::RemixBackend;
use crate::cloudtalk::utils::normalize_to_e164;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use common::sms::derived::{
    PendingOutbound, SmsProvider, finalize_outbound, insert_pending_outbound,
};
use common::telephony::cloudtalk;
use common::telephony::ringcentral::{self, MmsFile};
use lambda_http::tracing;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::MySqlPool;
use std::time::Duration;

/// Body of `POST /sms/send`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OutgoingSms {
    /// `cloudtalk` or `ringcentral`.
    pub provider: String,
    pub company_id: i32,
    pub user_id: i32,
    /// The sender's agent (`CloudTalk`) or extension (`RingCentral`) id.
    pub agent_id: String,
    pub phone_digits: String,
    #[serde(default)]
    pub text: String,
    /// Files already uploaded to the company's `sms-attachments/` folder.
    #[serde(default)]
    pub attachments: Vec<SmsAttachment>,
    #[serde(default)]
    pub is_automated: bool,
    /// Reused when the same message is retried, so the provider drops a
    /// repeat (follow-up steps); a fresh one per send otherwise.
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SmsAttachment {
    pub s3_key: String,
    pub url: String,
    pub content_type: String,
    pub filename: String,
    pub width: Option<i32>,
    pub height: Option<i32>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SentSms {
    /// The rows sent, in order; the first carries the text.
    pub ids: Vec<i64>,
}

/// A send that did not (fully) happen.
#[derive(Debug)]
pub struct SendFailure {
    status: StatusCode,
    /// A token the CRM translates (`agent_no_number`, `cloudtalk_…`).
    error: String,
    /// Rows already sent when a later message failed.
    sent_ids: Vec<i64>,
    /// Files of the messages that did not go out.
    unsent_s3_keys: Vec<String>,
}

impl SendFailure {
    fn new(status: StatusCode, error: impl Into<String>) -> Self {
        Self {
            status,
            error: error.into(),
            sent_ids: Vec::new(),
            unsent_s3_keys: Vec::new(),
        }
    }
}

impl From<sqlx::Error> for SendFailure {
    fn from(error: sqlx::Error) -> Self {
        tracing::error!(error = %error, "SMS send database error");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "send_failed")
    }
}

impl IntoResponse for SendFailure {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({
                "error": self.error,
                "ids": self.sent_ids,
                "unsent_s3_keys": self.unsent_s3_keys,
            })),
        )
            .into_response()
    }
}

/// "Photo 1: <url>", "File 1: name\n<url>" under the caption.
pub fn link_body(caption: &str, files: &[&SmsAttachment]) -> String {
    let (mut photos, mut others) = (0, 0);
    let mut lines = Vec::new();
    for file in files {
        if file.content_type.starts_with("image/") {
            photos += 1;
            lines.push(format!("Photo {photos}: {}", file.url));
        } else {
            others += 1;
            let name = file.filename.trim();
            lines.push(format!(
                "File {others}: {}",
                if name.is_empty() { "file" } else { name }
            ));
            lines.push(file.url.clone());
        }
    }
    let joined = lines.join("\n");
    if caption.is_empty() {
        joined
    } else {
        format!("{caption}\n{joined}")
    }
}

/// One message of the send.
struct Planned<'a> {
    text: String,
    files: Vec<&'a SmsAttachment>,
    /// Downloaded files for a `RingCentral` MMS; empty for plain SMS.
    mms: Vec<MmsFile>,
}

async fn download(client: &Client, file: &SmsAttachment) -> Option<MmsFile> {
    let response = client
        .get(&file.url)
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    Some(MmsFile {
        bytes: response.bytes().await.ok()?.to_vec(),
        content_type: file.content_type.clone(),
        filename: ringcentral::mms_filename(&file.filename, &file.content_type),
    })
}

async fn plan_ringcentral<'a>(
    client: &Client,
    text: &str,
    files: &'a [SmsAttachment],
) -> Result<Vec<Planned<'a>>, SendFailure> {
    if files.is_empty() {
        return Ok(vec![Planned {
            text: text.to_string(),
            files: Vec::new(),
            mms: Vec::new(),
        }]);
    }
    let mut downloaded = Vec::with_capacity(files.len());
    for file in files {
        let Some(mms) = download(client, file).await else {
            return Err(SendFailure::new(
                StatusCode::BAD_REQUEST,
                "ringcentral_mms_attachment_unavailable",
            ));
        };
        let size = mms.bytes.len();
        downloaded.push(((file, mms), size));
    }
    let (batches, too_large) = ringcentral::plan_batches(downloaded);
    let mut planned: Vec<Planned<'a>> = batches
        .into_iter()
        .enumerate()
        .map(|(index, batch)| {
            let (files, mms) = batch.into_iter().unzip();
            Planned {
                text: if index == 0 {
                    text.to_string()
                } else {
                    String::new()
                },
                files,
                mms,
            }
        })
        .collect();
    // Over the MMS cap: sent as a link, in its own text, with the caption
    // when nothing went as MMS.
    if !too_large.is_empty() {
        let links: Vec<&SmsAttachment> = too_large.into_iter().map(|(file, _)| file).collect();
        planned.push(Planned {
            text: link_body(if planned.is_empty() { text } else { "" }, &links),
            files: links,
            mms: Vec::new(),
        });
    }
    Ok(planned)
}

async fn insert_attachments(
    pool: &MySqlPool,
    provider: SmsProvider,
    sms_id: i64,
    files: &[&SmsAttachment],
) -> Result<(), sqlx::Error> {
    let (table, column) = match provider {
        SmsProvider::Cloudtalk => ("cloudtalk_sms_attachments", "cloudtalk_sms_id"),
        SmsProvider::Ringcentral => ("ringcentral_sms_attachments", "ringcentral_sms_id"),
    };
    for (position, file) in files.iter().enumerate() {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT INTO {table} ({column}, content_type, filename, s3_key, s3_url, width, height, position)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)"
        )))
        .bind(sms_id)
        .bind(&file.content_type)
        .bind(&file.filename)
        .bind(&file.s3_key)
        .bind(&file.url)
        .bind(file.width)
        .bind(file.height)
        .bind(i32::try_from(position).unwrap_or(i32::MAX))
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// Removes a never-sent pending row and its attachment rows.
async fn discard(pool: &MySqlPool, provider: SmsProvider, id: i64) -> Result<(), sqlx::Error> {
    let (attachments, column) = match provider {
        SmsProvider::Cloudtalk => ("cloudtalk_sms_attachments", "cloudtalk_sms_id"),
        SmsProvider::Ringcentral => ("ringcentral_sms_attachments", "ringcentral_sms_id"),
    };
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DELETE FROM {attachments} WHERE {column} = ?"
    )))
    .bind(id)
    .execute(pool)
    .await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DELETE FROM {} WHERE id = ? AND status = 'pending' AND {} IS NULL",
        provider.table(),
        provider.id_column()
    )))
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Who sends one request's messages, and from which number to which.
struct Delivery<'a> {
    pool: &'a MySqlPool,
    client: &'a Client,
    sms: &'a OutgoingSms,
    provider: SmsProvider,
    from: String,
    to: String,
    /// The `CloudTalk` API (a stub in tests).
    cloudtalk_base: String,
}

impl Delivery<'_> {
    fn cloudtalk(&self) -> cloudtalk::Api<'_> {
        cloudtalk::Api {
            pool: self.pool,
            client: self.client,
            base_url: &self.cloudtalk_base,
            company_id: self.sms.company_id,
        }
    }

    /// Calls the provider for one planned message; the error token on failure.
    async fn send(
        &self,
        message: &Planned<'_>,
        idempotency_key: &str,
    ) -> Result<Option<i64>, String> {
        let company_id = self.sms.company_id;
        match self.provider {
            SmsProvider::Cloudtalk => cloudtalk::send(
                self.cloudtalk(),
                &self.from,
                &self.to,
                &message.text,
                idempotency_key,
            )
            .await
            .map_err(|error| cloudtalk::error_token(&error)),
            SmsProvider::Ringcentral => {
                let outgoing = ringcentral::Outgoing {
                    extension_id: &self.sms.agent_id,
                    from: &self.from,
                    to: &self.to,
                    text: &message.text,
                    files: &message.mms,
                    idempotency_key,
                };
                ringcentral::send(self.pool, self.client, company_id, &outgoing)
                    .await
                    .map_err(|error| {
                        if message.mms.is_empty() {
                            ringcentral::sms_error_token(&error)
                        } else {
                            ringcentral::mms_error_token(&error)
                        }
                    })
            }
        }
    }

    /// Stores, sends and records one planned message.
    async fn send_planned(
        &self,
        message: &Planned<'_>,
        caption: &str,
        index: usize,
    ) -> Result<(i64, Result<(), String>), sqlx::Error> {
        // Only the first message of a split send reuses the caller's key.
        let idempotency_key = match &self.sms.idempotency_key {
            Some(key) if index == 0 => key.clone(),
            _ => uuid::Uuid::new_v4().to_string(),
        };
        let id = insert_pending_outbound(
            self.pool,
            self.provider,
            &PendingOutbound {
                company_id: self.sms.company_id,
                user_id: self.sms.user_id,
                agent: Some(&self.sms.agent_id),
                phone_digits: &self.sms.phone_digits,
                // CloudTalk rows show the caption; the files show as attachments.
                text: if self.provider == SmsProvider::Cloudtalk {
                    caption
                } else {
                    &message.text
                },
                idempotency_key: &idempotency_key,
                is_automated: self.sms.is_automated,
                created_date: chrono::Utc::now(),
            },
        )
        .await?;
        insert_attachments(self.pool, self.provider, id, &message.files).await?;
        match self.send(message, &idempotency_key).await {
            Ok(provider_id) => {
                // Sent: a database error from here on must not fail the row.
                if let Err(error) =
                    finalize_outbound(self.pool, self.provider, id, true, provider_id, None).await
                {
                    tracing::error!(error = %error, id, "could not record a sent SMS");
                }
                Ok((id, Ok(())))
            }
            Err(token) => Ok((id, Err(token))),
        }
    }
}

/// The sender's own number.
async fn sender_number(
    pool: &MySqlPool,
    client: &Client,
    provider: SmsProvider,
    sms: &OutgoingSms,
) -> Result<String, SendFailure> {
    let sender = match provider {
        SmsProvider::Cloudtalk => {
            let api = cloudtalk::Api {
                pool,
                client,
                base_url: &crate::cloudtalk::api::base_url(),
                company_id: sms.company_id,
            };
            cloudtalk::agent_sender(api, &sms.agent_id)
                .await
                .map_err(|error| cloudtalk::error_token(&error))
        }
        SmsProvider::Ringcentral => {
            ringcentral::extension_sender(pool, client, sms.company_id, &sms.agent_id)
                .await
                .map_err(|error| ringcentral::sms_error_token(&error))
        }
    };
    match sender {
        Ok(Some(from)) => Ok(from),
        Ok(None) => Err(SendFailure::new(StatusCode::BAD_REQUEST, "agent_no_number")),
        Err(token) => Err(SendFailure::new(StatusCode::BAD_GATEWAY, token)),
    }
}

/// Plans, stores and sends every message of `sms`.
pub async fn deliver(
    pool: &MySqlPool,
    client: &Client,
    sms: &OutgoingSms,
) -> Result<SentSms, SendFailure> {
    let provider = match sms.provider.as_str() {
        "cloudtalk" => SmsProvider::Cloudtalk,
        "ringcentral" => SmsProvider::Ringcentral,
        _ => {
            return Err(SendFailure::new(
                StatusCode::BAD_REQUEST,
                "unknown_provider",
            ));
        }
    };
    let Some(to) = normalize_to_e164(Some(&sms.phone_digits)) else {
        return Err(SendFailure::new(StatusCode::BAD_REQUEST, "invalid_phone"));
    };
    let from = sender_number(pool, client, provider, sms).await?;
    let text = sms.text.trim();
    let planned = match provider {
        SmsProvider::Cloudtalk => {
            let files: Vec<&SmsAttachment> = sms.attachments.iter().collect();
            vec![Planned {
                text: if files.is_empty() {
                    text.to_string()
                } else {
                    link_body(text, &files)
                },
                files,
                mms: Vec::new(),
            }]
        }
        SmsProvider::Ringcentral => plan_ringcentral(client, text, &sms.attachments).await?,
    };
    let delivery = Delivery {
        pool,
        client,
        sms,
        provider,
        from,
        to,
        cloudtalk_base: crate::cloudtalk::api::base_url(),
    };
    let mut ids = Vec::with_capacity(planned.len());
    for (index, message) in planned.iter().enumerate() {
        let (id, outcome) = delivery.send_planned(message, text, index).await?;
        let Err(token) = outcome else {
            ids.push(id);
            continue;
        };
        if ids.is_empty() {
            finalize_outbound(pool, provider, id, false, None, Some(&token)).await?;
            return Err(SendFailure::new(StatusCode::BAD_GATEWAY, token));
        }
        // Partly sent: the page keeps the files that did not go out.
        discard(pool, provider, id).await?;
        return Err(SendFailure {
            status: StatusCode::BAD_GATEWAY,
            error: token,
            sent_ids: ids,
            unsent_s3_keys: planned[index..]
                .iter()
                .flat_map(|m| m.files.iter().map(|f| f.s3_key.clone()))
                .collect(),
        });
    }
    Ok(SentSms { ids })
}

pub async fn send_handler(
    _: RemixBackend,
    State(pool): State<MySqlPool>,
    Json(sms): Json<OutgoingSms>,
) -> Result<Json<SentSms>, SendFailure> {
    deliver(&pool, &common::http::client(), &sms)
        .await
        .map(Json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::stub_http::{StubServer, use_cloudtalk};
    use serde_json::Value;

    fn photo(name: &str, url: &str) -> SmsAttachment {
        SmsAttachment {
            s3_key: format!("sms-attachments/90/m/{name}"),
            url: url.to_string(),
            content_type: "image/jpeg".into(),
            filename: name.into(),
            width: Some(10),
            height: Some(10),
        }
    }

    fn sms(provider: &str, company_id: i32, attachments: Vec<SmsAttachment>) -> OutgoingSms {
        OutgoingSms {
            provider: provider.into(),
            company_id,
            user_id: 7,
            agent_id: "5001".into(),
            phone_digits: "3175550100".into(),
            text: " Hello ".into(),
            attachments,
            is_automated: false,
            idempotency_key: None,
        }
    }

    #[derive(sqlx::FromRow, Debug)]
    struct Row {
        text: String,
        status: String,
        error_message: Option<String>,
        phone_digits: Option<String>,
        agent: Option<String>,
        sender_user_id: Option<i32>,
        provider_id: Option<i64>,
    }

    async fn rows(pool: &MySqlPool, table: &str, id_column: &str) -> Vec<Row> {
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT text, CAST(status AS CHAR) AS status, error_message, phone_digits, agent,
                    sender_user_id, {id_column} AS provider_id
               FROM {table} ORDER BY id"
        )))
        .fetch_all(pool)
        .await
        .unwrap()
    }

    async fn cloudtalk_company(pool: &MySqlPool, id: i32) {
        sqlx::query(
            "INSERT INTO company (id, name, cloudtalk_access_key, cloudtalk_access_secret)
             VALUES (?, 'Ct Co', 'key', 'secret')",
        )
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    }

    fn cloudtalk_stub(
        send_reply: Value,
    ) -> impl Fn(&crate::tests::stub_http::Recorded) -> Option<Value> {
        move |request| {
            if request.path.starts_with("/agents/index.json") {
                return Some(json!({ "responseData": { "data": [
                    { "Agent": { "id": 9999, "default_number": "+13175550000" } },
                    { "Agent": { "id": 5001, "default_number": "", "associated_numbers": ["+13175550999"] } }
                ] } }));
            }
            (request.path == "/sms/send.json").then(|| send_reply.clone())
        }
    }

    #[test_db::test]
    async fn cloudtalk_sends_one_text_with_links(pool: MySqlPool) {
        cloudtalk_company(&pool, 90).await;
        let stub = StubServer::start(cloudtalk_stub(
            json!({ "responseData": { "success": true } }),
        ))
        .await;
        use_cloudtalk(&stub.base);
        let sent = deliver(
            &pool,
            &common::http::client(),
            &sms(
                "cloudtalk",
                90,
                vec![photo("a.jpg", "https://bucket/a.jpg")],
            ),
        )
        .await
        .unwrap();
        assert_eq!(sent.ids.len(), 1);
        let stored = rows(&pool, "cloudtalk_sms", "cloudtalk_id").await;
        assert_eq!(stored[0].text, "Hello");
        assert_eq!(stored[0].status, "sent");
        assert_eq!(stored[0].phone_digits.as_deref(), Some("3175550100"));
        assert_eq!(stored[0].agent.as_deref(), Some("5001"));
        assert_eq!(stored[0].sender_user_id, Some(7));
        let body: Value = serde_json::from_str(&stub.requests().last().unwrap().body).unwrap();
        assert_eq!(
            body,
            json!({ "sender": "+13175550999", "recipient": "+13175550100",
                    "message": "Hello\nPhoto 1: https://bucket/a.jpg" })
        );
        let files: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM cloudtalk_sms_attachments WHERE cloudtalk_sms_id = ?",
        )
        .bind(sent.ids[0])
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(files, 1);
    }

    #[test_db::test]
    async fn a_follow_up_step_keeps_its_idempotency_key(pool: MySqlPool) {
        cloudtalk_company(&pool, 93).await;
        let stub = StubServer::start(cloudtalk_stub(
            json!({ "responseData": { "success": true } }),
        ))
        .await;
        use_cloudtalk(&stub.base);
        let mut step = sms("cloudtalk", 93, Vec::new());
        step.is_automated = true;
        step.idempotency_key = Some("sms-flow-12-3".into());
        deliver(&pool, &common::http::client(), &step)
            .await
            .unwrap();
        let stored: (Option<String>, bool) =
            sqlx::query_as("SELECT idempotency_key, is_automated <> 0 FROM cloudtalk_sms")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(stored, (Some("sms-flow-12-3".into()), true));
    }

    #[test_db::test]
    async fn a_refused_cloudtalk_send_is_stored_failed(pool: MySqlPool) {
        cloudtalk_company(&pool, 91).await;
        let stub = StubServer::start(cloudtalk_stub(json!({
            "responseData": { "success": false, "data": "Insufficient funds" }
        })))
        .await;
        use_cloudtalk(&stub.base);
        let failure = deliver(
            &pool,
            &common::http::client(),
            &sms("cloudtalk", 91, Vec::new()),
        )
        .await
        .unwrap_err();
        assert_eq!(failure.status, StatusCode::BAD_GATEWAY);
        assert_eq!(failure.error, "cloudtalk_insufficient_funds");
        let stored = rows(&pool, "cloudtalk_sms", "cloudtalk_id").await;
        assert_eq!(stored[0].status, "failed");
        assert_eq!(
            stored[0].error_message.as_deref(),
            Some("cloudtalk_insufficient_funds")
        );
    }

    #[test_db::test]
    async fn an_agent_without_a_number_cannot_send(pool: MySqlPool) {
        cloudtalk_company(&pool, 92).await;
        let stub = StubServer::start(|request| {
            request
                .path
                .starts_with("/agents/index.json")
                .then(|| json!({ "responseData": { "data": [] } }))
        })
        .await;
        use_cloudtalk(&stub.base);
        let failure = deliver(
            &pool,
            &common::http::client(),
            &sms("cloudtalk", 92, Vec::new()),
        )
        .await
        .unwrap_err();
        assert_eq!(failure.error, "agent_no_number");
        assert!(
            rows(&pool, "cloudtalk_sms", "cloudtalk_id")
                .await
                .is_empty()
        );
    }

    #[test_db::test]
    async fn ringcentral_sends_pictures_as_mms(pool: MySqlPool) {
        let stub = StubServer::start(|request| match request.path.as_str() {
            "/restapi/oauth/token" => {
                Some(json!({ "access_token": "jwt-token", "expires_in": 3600 }))
            }
            "/restapi/v1.0/account/~/extension/5001/phone-number?perPage=100" => {
                Some(json!({ "records": [{ "phoneNumber": "+13175550999" }] }))
            }
            "/files/a.jpg" => Some(json!({ "jpeg": true })),
            "/restapi/v1.0/account/~/extension/5001/mms" => {
                Some(json!({ "id": "123", "messageStatus": "Queued" }))
            }
            _ => None,
        })
        .await;
        sqlx::query(
            "INSERT INTO company (id, name, ringcentral_client_id, ringcentral_client_secret,
                                  ringcentral_jwt, ringcentral_server_url)
             VALUES (93, 'Rc Co', 'id', 'secret', 'jwt', ?)",
        )
        .bind(&stub.base)
        .execute(&pool)
        .await
        .unwrap();
        let url = format!("{}/files/a.jpg", stub.base);
        let sent = deliver(
            &pool,
            &common::http::client(),
            &sms("ringcentral", 93, vec![photo("a.png", &url)]),
        )
        .await
        .unwrap();
        assert_eq!(sent.ids.len(), 1);
        let stored = rows(&pool, "ringcentral_sms", "ringcentral_id").await;
        assert_eq!(stored[0].status, "sent");
        assert_eq!(stored[0].provider_id, Some(123));
        let mms = stub
            .requests()
            .into_iter()
            .find(|request| request.path.ends_with("/mms"))
            .unwrap();
        assert!(mms.body.contains("\"text\":\"Hello\""));
        assert!(mms.body.contains("filename=\"a.jpg\""));
    }

    #[test]
    fn link_bodies_list_photos_and_files() {
        let mut pdf = photo("quote.pdf", "https://b/q.pdf");
        pdf.content_type = "application/pdf".into();
        let image = photo("a.jpg", "https://b/a.jpg");
        assert_eq!(
            link_body("Hi", &[&image, &pdf]),
            "Hi\nPhoto 1: https://b/a.jpg\nFile 1: quote.pdf\nhttps://b/q.pdf"
        );
        assert_eq!(link_body("", &[&image]), "Photo 1: https://b/a.jpg");
    }
}
