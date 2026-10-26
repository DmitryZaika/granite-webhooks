//! Telnyx number porting (port-in orders, API v2): moving a company's
//! existing numbers to Telnyx from Admin → Phone.
//!
//! - `POST /portability_checks`: can these numbers move to Telnyx;
//! - `POST /porting_orders` (draft orders; Telnyx may split them),
//!   `GET|PATCH|DELETE /porting_orders/{id}`, `POST .../actions/confirm`
//!   (submit) and `.../actions/cancel`;
//! - `GET .../requirements`, `GET .../allowed_foc_windows`,
//!   `GET .../loa_template` (a PDF), `GET|POST .../comments`;
//! - `POST /documents` (the signed LOA, the bill), linked to the order by
//!   the next `PATCH` (Telnyx deletes documents not linked within 30
//!   minutes);
//! - once a port completes: `GET /phone_numbers`, `PATCH
//!   /phone_numbers/{id}` (its connection) and `PATCH
//!   /phone_numbers/{id}/messaging` (its messaging profile).
//!
//! A refused request keeps Telnyx's reason ([`PortingError::Status`]'s
//! `detail`, with phone numbers masked) for the admin to read. It is never
//! part of the error's `Display`, so logs do not carry it.

use super::{Telnyx, segment, telnyx_time};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chrono::{DateTime, Utc};
use regex::Regex;
use reqwest::Method;
use serde_json::{Value, json};
use std::sync::LazyLock;
use std::time::Duration;

/// Document uploads and the LOA template may take longer than other calls.
const DOCUMENT_TIMEOUT: Duration = Duration::from_secs(30);
/// The largest LOA template read.
const MAX_TEMPLATE_BYTES: usize = 10 * 1024 * 1024;
/// Rows per page, and pages at most, of the lists read.
const PAGE_SIZE: u32 = 100;
const MAX_PAGES: u32 = 5;
/// Telnyx's reason is cut to this many characters.
const DETAIL_CHARS: usize = 300;
/// Telnyx's reasons kept from one answer.
const MAX_REASONS: usize = 3;

/// Why a porting request failed.
#[derive(Debug, thiserror::Error)]
pub enum PortingError {
    /// `TELNYX_API_KEY` is not set: nothing was sent.
    #[error("TELNYX_API_KEY is not set")]
    NotConfigured,
    /// The request did not complete (connection refused, timeout, ...).
    #[error("Telnyx request failed: {0}")]
    Request(String),
    /// Telnyx answered with an error status. `code` is its first
    /// `errors[].code`; `detail` its reasons (`detail` or `title`, and the
    /// field they are about), phone numbers masked, for the admin.
    #[error("Telnyx answered {status}")]
    Status {
        status: u16,
        code: Option<String>,
        detail: Option<String>,
    },
    /// A success answer without what we asked for.
    #[error("unexpected Telnyx answer: {0}")]
    Unexpected(&'static str),
}

impl PortingError {
    /// The HTTP status Telnyx answered, when it answered.
    pub const fn status(&self) -> Option<u16> {
        match self {
            Self::Status { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// Telnyx's error code, when it gave one.
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Status { code, .. } => code.as_deref(),
            _ => None,
        }
    }

    /// Telnyx's reason, phone numbers masked, when it gave one.
    pub fn detail(&self) -> Option<&str> {
        match self {
            Self::Status { detail, .. } => detail.as_deref(),
            _ => None,
        }
    }
}

/// Telnyx's verdict on one number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Portability {
    /// The number as Telnyx wrote it.
    pub phone_number: String,
    pub portable: bool,
    /// FastPort: it can port within the day.
    pub fast_portable: bool,
    /// Telnyx can take its texting too; `None` when Telnyx did not say.
    pub messaging_capable: Option<bool>,
    /// Why it cannot port (Telnyx's words); `None` when it can.
    pub not_portable_reason: Option<String>,
}

/// One of Telnyx's reasons for an order's status (an exception's).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusDetail {
    /// E.g. `ACCOUNT_NUMBER_MISMATCH`.
    pub code: String,
    /// E.g. "Account number does not match that on the CSR".
    pub description: Option<String>,
}

/// A number of a porting order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PortingNumber {
    /// As Telnyx wrote it (E.164, sometimes without the `+`).
    pub phone_number: String,
    /// E.g. `New`, `Pending`, `Active`, `Conflict`.
    pub activation_status: Option<String>,
    /// `pending`, `confirmed` or `provisional`.
    pub portability_status: Option<String>,
    /// `requirement-info-pending`, `...-under-review`, `...-exception` or `approved`.
    pub requirements_status: Option<String>,
}

/// The current account holder at the losing carrier (`end_user.admin`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EndUser {
    /// The business (or person) on the account.
    pub entity_name: Option<String>,
    /// Who authorizes the port (signs the LOA).
    pub auth_person_name: Option<String>,
    /// The account's billing telephone number (BTN).
    pub billing_phone_number: Option<String>,
    /// The account number with the losing carrier.
    pub account_number: Option<String>,
    /// A PIN or passcode is saved (it is never read back).
    pub has_pin: bool,
}

/// The service address on the account (`end_user.location`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Location {
    pub street_address: Option<String>,
    pub extended_address: Option<String>,
    pub locality: Option<String>,
    pub administrative_area: Option<String>,
    pub postal_code: Option<String>,
    pub country_code: Option<String>,
}

/// Whether every number of the account ports (`misc`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Misc {
    /// `full` or `partial`.
    pub port_type: Option<String>,
    /// `keep` or `disconnect`: the numbers left at the losing carrier.
    pub remaining_numbers_action: Option<String>,
    /// The account's new billing number when the current one ports.
    pub new_billing_phone_number: Option<String>,
}

/// A porting order, as Telnyx describes it (the fields we use).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PortingOrder {
    pub id: String,
    /// `draft`, `in-process`, `submitted`, `exception`,
    /// `foc-date-confirmed`, `ported`, `cancel-pending` or `cancelled`.
    pub status: String,
    pub status_details: Vec<StatusDetail>,
    /// What to quote to Telnyx support (none while a draft).
    pub support_key: Option<String>,
    pub customer_reference: Option<String>,
    /// The losing carrier.
    pub old_service_provider: Option<String>,
    /// `local`, `toll_free`, `mobile`, ...
    pub phone_number_type: Option<String>,
    /// Its numbers (the first 50).
    pub phone_numbers: Vec<PortingNumber>,
    pub fast_port_eligible: bool,
    pub foc_requested: Option<DateTime<Utc>>,
    /// The port date the losing carrier confirmed.
    pub foc_actual: Option<DateTime<Utc>>,
    pub requirements_met: bool,
    /// The signed LOA's document id.
    pub loa_document: Option<String>,
    /// The bill's document id.
    pub invoice_document: Option<String>,
    pub end_user: EndUser,
    pub location: Location,
    pub misc: Option<Misc>,
    pub messaging_capable: Option<bool>,
    pub messaging_enabled: Option<bool>,
    /// `not_applicable`, `pending`, `activating`, `exception`, `ported`, ...
    pub messaging_status: Option<String>,
    /// The connection the numbers get once ported.
    pub connection_id: Option<String>,
    pub messaging_profile_id: Option<String>,
    pub webhook_url: Option<String>,
    /// E.g. `associated_phone_numbers` (UK partial ports).
    pub additional_steps: Vec<String>,
    pub updated_at: Option<DateTime<Utc>>,
}

/// A requirement of an order (`GET /porting_orders/{id}/requirements`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PortingRequirement {
    pub requirement_type_id: String,
    /// E.g. "Latest Invoice".
    pub name: Option<String>,
    pub description: Option<String>,
    pub example: Option<String>,
    /// `document` or `textual`.
    pub field_type: Option<String>,
    /// What fulfils it so far (a document id, or the text).
    pub field_value: Option<String>,
    /// `requirement-info-pending`, `...-under-review`, `...-exception` or `approved`.
    pub status: Option<String>,
    /// The values Telnyx accepts, when it lists them.
    pub acceptable_values: Vec<String>,
}

/// A comment on an order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortingComment {
    pub id: String,
    pub body: String,
    /// `admin` (Telnyx's porting team), `user` (us) or `system`.
    pub user_type: String,
    pub created_at: Option<DateTime<Utc>>,
}

/// A range of times the port date may be requested in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FocWindow {
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
}

/// A number on our Telnyx account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountNumber {
    /// Telnyx's id of the number.
    pub id: String,
    /// E.164.
    pub phone_number: String,
    pub connection_id: Option<String>,
    pub messaging_profile_id: Option<String>,
}

/// A JSON string (or number) as text; `None` when missing or blank.
fn text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.trim())
            .filter(|text| !text.is_empty())
            .map(str::to_string),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn time(value: &Value) -> Option<DateTime<Utc>> {
    value
        .as_str()
        .and_then(|text| DateTime::parse_from_rfc3339(text.trim()).ok())
        .map(|at| at.with_timezone(&Utc))
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(text)
        .collect()
}

/// Runs of 10 or more digits (with `+`, spaces, dots, dashes, brackets): phone
/// numbers, not dates or codes.
static NUMBER_RUN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\+?\(?\d[\d\s().\-]{5,}\d").expect("valid pattern"));

/// `text` with every phone-number-like run of 10 or more digits replaced by
/// `[number]`, so it can be shown and reported without the numbers.
pub fn mask_numbers(text: &str) -> String {
    NUMBER_RUN
        .replace_all(text, |found: &regex::Captures<'_>| {
            let run = &found[0];
            if run.chars().filter(char::is_ascii_digit).count() >= 10 {
                "[number]".to_string()
            } else {
                run.to_string()
            }
        })
        .into_owned()
}

/// Telnyx's reasons in an error answer: each error's `detail` (else its
/// `title`) and the field it names, masked and cut short.
fn reasons(answer: &Value) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for error in answer["errors"].as_array().into_iter().flatten() {
        let Some(reason) = text(&error["detail"]).or_else(|| text(&error["title"])) else {
            continue;
        };
        let field = text(&error["source"]["pointer"])
            .map(|pointer| pointer.trim_matches('/').replace('/', "."))
            .filter(|field| !field.is_empty());
        let part = match field {
            Some(field) => format!("{reason} ({field})"),
            None => reason,
        };
        if !parts.contains(&part) {
            parts.push(part);
        }
        if parts.len() == MAX_REASONS {
            break;
        }
    }
    if parts.is_empty() {
        return None;
    }
    let joined = mask_numbers(&parts.join("; "));
    Some(joined.chars().take(DETAIL_CHARS).collect())
}

fn first_code(answer: &Value) -> Option<String> {
    text(&answer["errors"][0]["code"])
}

impl PortingOrder {
    /// The order in a Telnyx answer's `data`; `None` without an id or status.
    pub fn parse(data: &Value) -> Option<Self> {
        let id = text(&data["id"])?;
        let status = text(&data["status"]["value"]).or_else(|| text(&data["status"]))?;
        let admin = &data["end_user"]["admin"];
        let location = &data["end_user"]["location"];
        let activation = &data["activation_settings"];
        let configuration = &data["phone_number_configuration"];
        let messaging = &data["messaging"];
        let misc = &data["misc"];
        Some(Self {
            id,
            status,
            status_details: data["status"]["details"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|detail| {
                    Some(StatusDetail {
                        code: text(&detail["code"])?,
                        description: text(&detail["description"]),
                    })
                })
                .collect(),
            support_key: text(&data["support_key"]),
            customer_reference: text(&data["customer_reference"]),
            old_service_provider: text(&data["old_service_provider_ocn"]),
            phone_number_type: text(&data["phone_number_type"]),
            phone_numbers: data["phone_numbers"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|number| {
                    Some(PortingNumber {
                        phone_number: text(&number["phone_number"])?,
                        activation_status: text(&number["activation_status"]),
                        portability_status: text(&number["portability_status"]),
                        requirements_status: text(&number["requirements_status"]),
                    })
                })
                .collect(),
            fast_port_eligible: activation["fast_port_eligible"].as_bool().unwrap_or(false),
            foc_requested: time(&activation["foc_datetime_requested"]),
            foc_actual: time(&activation["foc_datetime_actual"]),
            requirements_met: data["requirements_met"].as_bool().unwrap_or(false),
            loa_document: text(&data["documents"]["loa"]),
            invoice_document: text(&data["documents"]["invoice"]),
            end_user: EndUser {
                entity_name: text(&admin["entity_name"]),
                auth_person_name: text(&admin["auth_person_name"]),
                billing_phone_number: text(&admin["billing_phone_number"]),
                account_number: text(&admin["account_number"]),
                has_pin: text(&admin["pin_passcode"]).is_some(),
            },
            location: Location {
                street_address: text(&location["street_address"]),
                extended_address: text(&location["extended_address"]),
                locality: text(&location["locality"]),
                administrative_area: text(&location["administrative_area"]),
                postal_code: text(&location["postal_code"]),
                country_code: text(&location["country_code"]),
            },
            misc: misc.is_object().then(|| Misc {
                port_type: text(&misc["type"]),
                remaining_numbers_action: text(&misc["remaining_numbers_action"]),
                new_billing_phone_number: text(&misc["new_billing_phone_number"]),
            }),
            messaging_capable: messaging["messaging_capable"].as_bool(),
            messaging_enabled: messaging["enable_messaging"].as_bool(),
            messaging_status: text(&messaging["messaging_port_status"]),
            connection_id: text(&configuration["connection_id"]),
            messaging_profile_id: text(&configuration["messaging_profile_id"]),
            webhook_url: text(&data["webhook_url"]),
            additional_steps: strings(&data["additional_steps"]),
            updated_at: time(&data["updated_at"]),
        })
    }
}

impl PortingRequirement {
    fn parse(row: &Value) -> Option<Self> {
        let kind = &row["requirement_type"];
        Some(Self {
            requirement_type_id: text(&kind["id"]).or_else(|| text(&row["requirement_type_id"]))?,
            name: text(&kind["name"]),
            description: text(&kind["description"]),
            example: text(&kind["example"]),
            field_type: text(&row["field_type"]).or_else(|| text(&kind["type"])),
            field_value: text(&row["field_value"]),
            status: text(&row["requirement_status"]),
            acceptable_values: strings(&kind["acceptance_criteria"]["acceptable_values"]),
        })
    }
}

impl PortingComment {
    fn parse(row: &Value) -> Option<Self> {
        Some(Self {
            id: text(&row["id"])?,
            body: row["body"].as_str().unwrap_or_default().to_string(),
            user_type: text(&row["user_type"]).unwrap_or_else(|| "system".into()),
            created_at: time(&row["created_at"]).or_else(|| time(&row["inserted_at"])),
        })
    }
}

/// `path?query` with `pairs` form-encoded (`[` and `]` as `%5B`, `%5D`).
fn with_query(path: &str, pairs: &[(&str, &str)]) -> String {
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in pairs {
        query.append_pair(key, value);
    }
    format!("{path}?{}", query.finish())
}

impl Telnyx {
    async fn porting_send(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        timeout: Option<Duration>,
    ) -> Result<reqwest::Response, PortingError> {
        let key = self.key.as_deref().ok_or(PortingError::NotConfigured)?;
        let mut request = self
            .http
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(key);
        if let Some(timeout) = timeout {
            request = request.timeout(timeout);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request
            .send()
            .await
            .map_err(|err| PortingError::Request(err.without_url().to_string()))?;
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status().as_u16();
        let answer: Value = response.json().await.unwrap_or(Value::Null);
        Err(PortingError::Status {
            status,
            code: first_code(&answer),
            detail: reasons(&answer),
        })
    }

    async fn porting_json(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, PortingError> {
        self.porting_send(method, path, body, None)
            .await?
            .json()
            .await
            .map_err(|err| PortingError::Request(err.without_url().to_string()))
    }

    /// The order in an answer's `data`.
    async fn porting_order(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<PortingOrder, PortingError> {
        let answer = self.porting_json(method, path, body).await?;
        PortingOrder::parse(&answer["data"]).ok_or(PortingError::Unexpected("porting order"))
    }

    /// Every row of a paged list (at most [`MAX_PAGES`] pages).
    async fn porting_pages(
        &self,
        path: &str,
        filters: &[(&str, &str)],
    ) -> Result<Vec<Value>, PortingError> {
        let mut rows = Vec::new();
        for page in 1..=MAX_PAGES {
            let number = page.to_string();
            let size = PAGE_SIZE.to_string();
            let mut pairs = filters.to_vec();
            pairs.push(("page[number]", &number));
            pairs.push(("page[size]", &size));
            let answer = self
                .porting_json(Method::GET, &with_query(path, &pairs), None)
                .await?;
            let page_rows = answer["data"].as_array().cloned().unwrap_or_default();
            let empty = page_rows.is_empty();
            rows.extend(page_rows);
            let total_pages = answer["meta"]["total_pages"].as_u64().unwrap_or(1);
            if empty || u64::from(page) >= total_pages {
                break;
            }
        }
        Ok(rows)
    }

    /// Whether each number can move to Telnyx (`POST /portability_checks`),
    /// in Telnyx's order.
    pub async fn portability_check(
        &self,
        phone_numbers: &[String],
    ) -> Result<Vec<Portability>, PortingError> {
        let body = json!({ "phone_numbers": phone_numbers });
        let answer = self
            .porting_json(Method::POST, "/portability_checks", Some(&body))
            .await?;
        Ok(answer["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|row| {
                Some(Portability {
                    phone_number: text(&row["phone_number"])?,
                    portable: row["portable"].as_bool().unwrap_or(false),
                    fast_portable: row["fast_portable"].as_bool().unwrap_or(false),
                    messaging_capable: row["messaging_capable"].as_bool(),
                    not_portable_reason: text(&row["not_portable_reason"]),
                })
            })
            .collect())
    }

    /// Creates draft orders for the numbers (`POST /porting_orders`): one,
    /// or several when Telnyx splits them (by carrier, number type or
    /// FastPort).
    pub async fn create_porting_orders(
        &self,
        phone_numbers: &[String],
        customer_reference: &str,
        customer_group_reference: &str,
    ) -> Result<Vec<PortingOrder>, PortingError> {
        let body = json!({
            "phone_numbers": phone_numbers,
            "customer_reference": customer_reference,
            "customer_group_reference": customer_group_reference,
        });
        let answer = self
            .porting_json(Method::POST, "/porting_orders", Some(&body))
            .await?;
        let orders: Vec<PortingOrder> = match &answer["data"] {
            Value::Array(rows) => rows.iter().filter_map(PortingOrder::parse).collect(),
            data => PortingOrder::parse(data).into_iter().collect(),
        };
        if orders.is_empty() {
            return Err(PortingError::Unexpected("no porting order"));
        }
        Ok(orders)
    }

    /// An order with its numbers (`GET /porting_orders/{id}`).
    pub async fn get_porting_order(&self, id: &str) -> Result<PortingOrder, PortingError> {
        let path = with_query(
            &format!("/porting_orders/{}", segment(id)),
            &[("include_phone_numbers", "true")],
        );
        self.porting_order(Method::GET, &path, None).await
    }

    /// Changes an order (`PATCH /porting_orders/{id}`); fields left out
    /// keep their values.
    pub async fn update_porting_order(
        &self,
        id: &str,
        changes: &Value,
    ) -> Result<PortingOrder, PortingError> {
        self.porting_order(
            Method::PATCH,
            &format!("/porting_orders/{}", segment(id)),
            Some(changes),
        )
        .await
    }

    /// Submits an order to the losing carrier (`.../actions/confirm`).
    pub async fn confirm_porting_order(&self, id: &str) -> Result<PortingOrder, PortingError> {
        self.porting_order(
            Method::POST,
            &format!("/porting_orders/{}/actions/confirm", segment(id)),
            None,
        )
        .await
    }

    /// Asks Telnyx to cancel a submitted order (`.../actions/cancel`): it
    /// goes `cancel-pending`, then `cancelled`.
    pub async fn cancel_porting_order(&self, id: &str) -> Result<PortingOrder, PortingError> {
        self.porting_order(
            Method::POST,
            &format!("/porting_orders/{}/actions/cancel", segment(id)),
            None,
        )
        .await
    }

    /// Deletes a draft order (`DELETE /porting_orders/{id}`); one that is
    /// gone already is fine.
    pub async fn delete_porting_order(&self, id: &str) -> Result<(), PortingError> {
        match self
            .porting_send(
                Method::DELETE,
                &format!("/porting_orders/{}", segment(id)),
                None,
                None,
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(err) if err.status() == Some(404) => Ok(()),
            Err(err) => Err(err),
        }
    }

    /// What the order still needs (`GET .../requirements`).
    pub async fn porting_requirements(
        &self,
        id: &str,
    ) -> Result<Vec<PortingRequirement>, PortingError> {
        let rows = self
            .porting_pages(
                &format!("/porting_orders/{}/requirements", segment(id)),
                &[],
            )
            .await?;
        Ok(rows.iter().filter_map(PortingRequirement::parse).collect())
    }

    /// The order's comments, oldest first (`GET .../comments`).
    pub async fn porting_comments(&self, id: &str) -> Result<Vec<PortingComment>, PortingError> {
        let rows = self
            .porting_pages(&format!("/porting_orders/{}/comments", segment(id)), &[])
            .await?;
        let mut comments: Vec<PortingComment> =
            rows.iter().filter_map(PortingComment::parse).collect();
        comments.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        Ok(comments)
    }

    /// Writes to Telnyx's porting team on the order (`POST .../comments`).
    pub async fn create_porting_comment(
        &self,
        id: &str,
        body: &str,
    ) -> Result<PortingComment, PortingError> {
        let answer = self
            .porting_json(
                Method::POST,
                &format!("/porting_orders/{}/comments", segment(id)),
                Some(&json!({ "body": body })),
            )
            .await?;
        PortingComment::parse(&answer["data"]).ok_or(PortingError::Unexpected("comment"))
    }

    /// When the port date may be requested (`GET .../allowed_foc_windows`).
    pub async fn allowed_foc_windows(&self, id: &str) -> Result<Vec<FocWindow>, PortingError> {
        let answer = self
            .porting_json(
                Method::GET,
                &format!("/porting_orders/{}/allowed_foc_windows", segment(id)),
                None,
            )
            .await?;
        let mut windows: Vec<FocWindow> = answer["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|row| {
                let started_at = time(&row["started_at"])?;
                let ended_at = time(&row["ended_at"])?;
                (ended_at >= started_at).then_some(FocWindow {
                    started_at,
                    ended_at,
                })
            })
            .collect();
        windows.sort_by_key(|window| window.started_at);
        Ok(windows)
    }

    /// The order's Letter of Authorization to sign, a PDF
    /// (`GET .../loa_template`).
    pub async fn porting_loa_template(&self, id: &str) -> Result<Vec<u8>, PortingError> {
        let response = self
            .porting_send(
                Method::GET,
                &format!("/porting_orders/{}/loa_template", segment(id)),
                None,
                Some(DOCUMENT_TIMEOUT),
            )
            .await?;
        if response
            .content_length()
            .is_some_and(|length| length > MAX_TEMPLATE_BYTES as u64)
        {
            return Err(PortingError::Unexpected("LOA template too large"));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|err| PortingError::Request(err.without_url().to_string()))?;
        if bytes.is_empty() {
            return Err(PortingError::Unexpected("empty LOA template"));
        }
        if bytes.len() > MAX_TEMPLATE_BYTES {
            return Err(PortingError::Unexpected("LOA template too large"));
        }
        Ok(bytes.to_vec())
    }

    /// Uploads a document (`POST /documents`, the file base64): its id.
    /// It must be linked to an order within 30 minutes, or Telnyx deletes it.
    pub async fn upload_document(
        &self,
        filename: &str,
        bytes: &[u8],
        customer_reference: &str,
    ) -> Result<String, PortingError> {
        let body = json!({
            "file": STANDARD.encode(bytes),
            "filename": filename,
            "customer_reference": customer_reference,
        });
        let answer: Value = self
            .porting_send(
                Method::POST,
                "/documents",
                Some(&body),
                Some(DOCUMENT_TIMEOUT),
            )
            .await?
            .json()
            .await
            .map_err(|err| PortingError::Request(err.without_url().to_string()))?;
        text(&answer["data"]["id"]).ok_or(PortingError::Unexpected("document without id"))
    }

    /// The number on our account (`GET /phone_numbers?filter[phone_number]=`,
    /// matched exactly); `None` while Telnyx does not list it.
    pub async fn find_account_number(
        &self,
        e164: &str,
    ) -> Result<Option<AccountNumber>, PortingError> {
        let digits: String = e164.chars().filter(char::is_ascii_digit).collect();
        let path = with_query(
            "/phone_numbers",
            &[("filter[phone_number]", &digits), ("page[size]", "20")],
        );
        let answer = self.porting_json(Method::GET, &path, None).await?;
        Ok(answer["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(account_number)
            .find(|number| {
                number
                    .phone_number
                    .chars()
                    .filter(char::is_ascii_digit)
                    .eq(digits.chars())
            }))
    }

    /// The numbers on a connection (the company's TeXML app), every page,
    /// with their messaging profile.
    pub async fn account_numbers_on(
        &self,
        connection_id: &str,
    ) -> Result<Vec<AccountNumber>, PortingError> {
        let rows = self
            .porting_pages(
                "/phone_numbers",
                &[("filter[connection_id]", connection_id)],
            )
            .await?;
        Ok(rows.iter().filter_map(account_number).collect())
    }

    /// Puts a number on a connection (and billing group)
    /// (`PATCH /phone_numbers/{id}`).
    pub async fn set_number_connection(
        &self,
        id: &str,
        connection_id: &str,
        billing_group_id: Option<&str>,
    ) -> Result<(), PortingError> {
        let mut body = json!({ "connection_id": connection_id });
        if let Some(billing_group_id) = billing_group_id {
            body["billing_group_id"] = json!(billing_group_id);
        }
        self.porting_send(
            Method::PATCH,
            &format!("/phone_numbers/{}", segment(id)),
            Some(&body),
            None,
        )
        .await
        .map(drop)
    }

    /// Gives a number a messaging profile (`PATCH /phone_numbers/{id}/messaging`).
    pub async fn set_number_messaging_profile(
        &self,
        id: &str,
        messaging_profile_id: &str,
    ) -> Result<(), PortingError> {
        self.porting_send(
            Method::PATCH,
            &format!("/phone_numbers/{}/messaging", segment(id)),
            Some(&json!({ "messaging_profile_id": messaging_profile_id })),
            None,
        )
        .await
        .map(drop)
    }
}

fn account_number(row: &Value) -> Option<AccountNumber> {
    Some(AccountNumber {
        id: text(&row["id"])?,
        phone_number: text(&row["phone_number"])?,
        connection_id: text(&row["connection_id"]),
        messaging_profile_id: text(&row["messaging_profile_id"]),
    })
}

/// A requested port date as Telnyx takes it (UTC, to the second).
pub fn foc_time(at: DateTime<Utc>) -> String {
    telnyx_time(at)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_are_masked_in_reasons() {
        assert_eq!(
            mask_numbers("+13035550000 is not portable; 303-555-0001 neither"),
            "[number] is not portable; [number] neither"
        );
        assert_eq!(
            mask_numbers("Phone number (303) 555 0002 has a conflict"),
            "Phone number [number] has a conflict"
        );
        // Dates, codes and short numbers stay.
        assert_eq!(
            mask_numbers("FOC 2026-10-30 is outside the window; code 10015, 48 hours"),
            "FOC 2026-10-30 is outside the window; code 10015, 48 hours"
        );
        let answer = json!({ "errors": [
            { "code": "10015", "title": "Bad Request",
              "detail": "+13035550000 is not portable", "source": { "pointer": "/phone_numbers/0" } },
            { "code": "10015", "title": "Invalid value" },
            { "code": "10015", "title": "Invalid value" },
        ] });
        assert_eq!(
            reasons(&answer).as_deref(),
            Some("[number] is not portable (phone_numbers.0); Invalid value")
        );
        assert_eq!(first_code(&answer).as_deref(), Some("10015"));
        assert_eq!(reasons(&json!({ "errors": [] })), None);
        assert_eq!(reasons(&Value::Null), None);
        let long = json!({ "errors": [{ "detail": "x".repeat(500) }] });
        assert_eq!(reasons(&long).unwrap().chars().count(), DETAIL_CHARS);
    }

    #[test]
    fn orders_are_read_from_telnyx_answers() {
        let data = json!({
            "id": "ord-1", "record_type": "porting_order",
            "status": { "value": "exception", "details": [
                { "code": "ACCOUNT_NUMBER_MISMATCH", "description": "Account number does not match" },
                { "description": "no code" }] },
            "support_key": "sr_1a2b3c", "customer_reference": "granite-c100-abcd",
            "old_service_provider_ocn": "Unreal Communications", "phone_number_type": "local",
            "phone_numbers": [
                { "phone_number": "13035550000", "activation_status": "New",
                  "portability_status": "confirmed", "requirements_status": "approved" },
                { "activation_status": "New" }],
            "activation_settings": { "fast_port_eligible": true,
                "foc_datetime_requested": "2026-10-30T15:00:00Z", "foc_datetime_actual": null },
            "requirements_met": false,
            "documents": { "loa": "doc-loa", "invoice": null },
            "end_user": {
                "admin": { "entity_name": "Acme Granite", "auth_person_name": "Ada Admin",
                           "billing_phone_number": "+13035550000", "account_number": "123abc",
                           "pin_passcode": "1234" },
                "location": { "street_address": "600 Congress Avenue", "locality": "Austin",
                              "administrative_area": "TX", "postal_code": "78701", "country_code": "US" } },
            "misc": { "type": "partial", "remaining_numbers_action": "keep",
                      "new_billing_phone_number": "+13035550009" },
            "messaging": { "messaging_capable": true, "enable_messaging": false,
                           "messaging_port_status": "not_applicable" },
            "phone_number_configuration": { "connection_id": "app-100", "messaging_profile_id": null },
            "webhook_url": "https://api.test/v1/webhooks/telnyx/tok/porting",
            "additional_steps": [],
            "updated_at": "2026-10-23T10:00:00.5Z"
        });
        let order = PortingOrder::parse(&data).unwrap();
        assert_eq!(order.id, "ord-1");
        assert_eq!(order.status, "exception");
        assert_eq!(
            order.status_details,
            [StatusDetail {
                code: "ACCOUNT_NUMBER_MISMATCH".into(),
                description: Some("Account number does not match".into())
            }]
        );
        assert_eq!(order.phone_numbers.len(), 1);
        assert_eq!(order.phone_numbers[0].phone_number, "13035550000");
        assert!(order.fast_port_eligible);
        assert_eq!(
            order.foc_requested.map(|at| at.to_rfc3339()),
            Some("2026-10-30T15:00:00+00:00".into())
        );
        assert_eq!(order.foc_actual, None);
        assert_eq!(order.loa_document.as_deref(), Some("doc-loa"));
        assert_eq!(order.invoice_document, None);
        assert!(order.end_user.has_pin);
        assert_eq!(order.end_user.account_number.as_deref(), Some("123abc"));
        assert_eq!(order.location.extended_address, None);
        assert_eq!(
            order
                .misc
                .as_ref()
                .and_then(|misc| misc.port_type.as_deref()),
            Some("partial")
        );
        assert_eq!(order.messaging_capable, Some(true));
        assert_eq!(order.connection_id.as_deref(), Some("app-100"));
        assert_eq!(order.messaging_profile_id, None);
        // A null `misc` is none; without an id or a status there is no order.
        assert_eq!(
            PortingOrder::parse(
                &json!({ "id": "o", "status": { "value": "draft" }, "misc": null })
            )
            .unwrap()
            .misc,
            None
        );
        assert!(PortingOrder::parse(&json!({ "status": { "value": "draft" } })).is_none());
        assert!(PortingOrder::parse(&json!({ "id": "o" })).is_none());
    }

    #[test]
    fn requirements_and_comments_are_read() {
        let requirement = PortingRequirement::parse(&json!({
            "field_type": "document", "field_value": "doc-9", "requirement_status": "approved",
            "requirement_type": { "id": "req-1", "name": "Latest Invoice",
                "description": "A copy of the latest phone bill", "example": "Most recent phone bill",
                "type": "document", "acceptance_criteria": { "acceptable_values": ["a", "b"] } } }))
        .unwrap();
        assert_eq!(requirement.requirement_type_id, "req-1");
        assert_eq!(requirement.name.as_deref(), Some("Latest Invoice"));
        assert_eq!(requirement.status.as_deref(), Some("approved"));
        assert_eq!(requirement.acceptable_values, ["a", "b"]);
        let comment = PortingComment::parse(&json!({
            "id": "c-1", "body": "Please send a newer bill", "user_type": "admin",
            "created_at": "2026-10-23T10:00:00Z" }))
        .unwrap();
        assert_eq!(comment.user_type, "admin");
        assert_eq!(comment.body, "Please send a newer bill");
        assert!(PortingComment::parse(&json!({ "body": "no id" })).is_none());
    }
}
