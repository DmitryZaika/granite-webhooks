//! Telnyx 10DLC: registering a company's business texting with The Campaign
//! Registry (TCR) through Telnyx, so US carriers carry its texts.
//!
//! - brands: `POST /10dlc/brand`, `GET|PUT /10dlc/brand/{id}`,
//!   `GET /10dlc/brand?displayName=` (a brand whose create answer never
//!   came), `GET /10dlc/brand_feedback/{id}` (why it failed);
//! - campaigns: `POST /10dlc/campaignBuilder`, `GET|PUT /10dlc/campaign/{id}`
//!   (only the samples, message flow and help message change),
//!   `GET /10dlc/campaign?brandId=` (ours by `referenceId`),
//!   `POST /10dlc/campaign/{id}/appeal`, `GET /10dlc/campaign/usecase_cost`;
//! - numbers: `POST /10dlc/phone_number_campaigns`,
//!   `GET /10dlc/phone_number_campaigns/{phoneNumber}`.
//!
//! Telnyx answers the record itself (its guides show some wrapped in
//! `data`; both are read). A refused request keeps Telnyx's reasons
//! (`errors[].detail` or `.title`, or a validation error's `detail[].msg`)
//! with phone numbers masked, for the superuser; they are never part of the
//! error's `Display`, so logs do not carry them.

use super::porting::mask_numbers;
use super::{Telnyx, segment};
use reqwest::Method;
use serde_json::{Value, json};
use std::time::Duration;

/// Creating or changing a brand or campaign waits on TCR.
const REGISTRY_TIMEOUT: Duration = Duration::from_secs(15);
/// Telnyx's reasons are cut to this many characters.
const DETAIL_CHARS: usize = 300;
/// Telnyx's reasons kept from one answer.
const MAX_REASONS: usize = 4;
/// Records per page of the lists read (one page is plenty: a company has one
/// brand and a few campaigns).
const LIST_SIZE: &str = "100";

/// Why a 10DLC request failed.
#[derive(Debug, thiserror::Error)]
pub enum TenDlcError {
    /// `TELNYX_API_KEY` is not set: nothing was sent.
    #[error("TELNYX_API_KEY is not set")]
    NotConfigured,
    /// The request did not complete (connection refused, timeout, ...): it
    /// may or may not have been done.
    #[error("Telnyx request failed: {0}")]
    Request(String),
    /// Telnyx answered with an error status. `code` is its first
    /// `errors[].code`; `detail` its reasons, phone numbers masked, for the
    /// superuser.
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

impl TenDlcError {
    /// The HTTP status Telnyx answered, when it answered.
    pub const fn status(&self) -> Option<u16> {
        match self {
            Self::Status { status, .. } => Some(*status),
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

    /// Telnyx looked at the request and refused it (4xx): nothing was done,
    /// and sending the same again is refused again.
    pub const fn refused(&self) -> bool {
        matches!(self, Self::Status { status, .. } if *status >= 400 && *status < 500)
    }
}

/// A brand (the business), as Telnyx describes it (the fields we use).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Brand {
    /// Telnyx's id.
    pub brand_id: String,
    /// TCR's id (`BXXXXXX`), once registered.
    pub tcr_brand_id: Option<String>,
    /// `OK`, `REGISTRATION_PENDING` or `REGISTRATION_FAILED`.
    pub status: Option<String>,
    /// `SELF_DECLARED`, `VERIFIED`, `VETTED_VERIFIED` or `UNVERIFIED`.
    pub identity_status: Option<String>,
    /// Why registering it failed (Telnyx's words, numbers masked).
    pub failure_reasons: Option<String>,
    pub display_name: Option<String>,
    pub company_name: Option<String>,
    pub email: Option<String>,
}

/// A campaign (how the business texts), as Telnyx describes it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Campaign {
    /// Telnyx's id.
    pub campaign_id: String,
    /// TCR's id (`CXXXXXX`), once registered.
    pub tcr_campaign_id: Option<String>,
    /// `TCR_PENDING`, `TCR_ACCEPTED`, `TCR_FAILED`, `TELNYX_ACCEPTED`,
    /// `TELNYX_FAILED`, `MNO_PENDING`, `MNO_ACCEPTED`, `MNO_REJECTED`,
    /// `MNO_PROVISIONED`, `MNO_PROVISIONING_FAILED`, `TCR_SUSPENDED` or
    /// `TCR_EXPIRED`.
    pub campaign_status: Option<String>,
    /// `ACTIVE` or `EXPIRED`.
    pub status: Option<String>,
    /// Why it failed (Telnyx's words, numbers masked).
    pub failure_reasons: Option<String>,
    /// The `referenceId` we gave it.
    pub reference_id: Option<String>,
}

/// One of Telnyx's reasons a brand failed (`GET /10dlc/brand_feedback/{id}`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BrandFeedback {
    /// `TAX_ID`, `STOCK_SYMBOL`, `GOVERNMENT_ENTITY`, `NONPROFIT` or `OTHERS`.
    pub id: String,
    /// E.g. "Tax Id".
    pub display_name: Option<String>,
    /// What is wrong (numbers masked).
    pub description: Option<String>,
    /// The brand fields it is about, e.g. `ein`, `companyName`.
    pub fields: Vec<String>,
}

/// What a campaign costs (`GET /10dlc/campaign/usecase_cost`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CampaignCost {
    pub usecase: String,
    /// Charged when the campaign is created (Telnyx's text, e.g. `30.00`).
    pub up_front_cost: Option<String>,
    /// Charged every month after the first three.
    pub monthly_cost: Option<String>,
    pub description: Option<String>,
}

/// A number's campaign assignment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NumberAssignment {
    /// E.164, as Telnyx wrote it.
    pub phone_number: String,
    pub campaign_id: Option<String>,
    /// `PENDING_ASSIGNMENT`, `ASSIGNED`, `FAILED_ASSIGNMENT`,
    /// `PENDING_UNASSIGNMENT` or `FAILED_UNASSIGNMENT`.
    pub assignment_status: Option<String>,
    /// Why it failed (numbers masked).
    pub failure_reasons: Option<String>,
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

/// Telnyx's words about a failure, which it gives as a string, a list of
/// strings, or a list of `{description}`: one text, masked and cut short.
pub fn reason_text(value: &Value) -> Option<String> {
    let joined = match value {
        Value::Array(items) => {
            let parts: Vec<String> = items
                .iter()
                .filter_map(|item| {
                    text(item)
                        .or_else(|| text(&item["description"]))
                        .or_else(|| text(&item["reason"]))
                })
                .collect();
            (!parts.is_empty()).then(|| parts.join("; "))
        }
        other => text(other),
    }?;
    Some(mask_numbers(&joined).chars().take(DETAIL_CHARS).collect())
}

/// The record in an answer: the answer itself, or its `data`.
fn record(answer: &Value) -> &Value {
    if answer["data"].is_object() {
        &answer["data"]
    } else {
        answer
    }
}

/// The rows of a list answer (`records`, or `data`).
fn rows(answer: &Value) -> Vec<Value> {
    [&answer["records"], &answer["data"]]
        .into_iter()
        .find_map(|rows| rows.as_array().cloned())
        .unwrap_or_default()
}

/// Telnyx's reasons in an error answer: each error's `detail` (else its
/// `title`) and the field it names; or a validation error's `msg` and
/// `loc`; masked and cut short.
fn reasons(answer: &Value) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for error in answer["errors"].as_array().into_iter().flatten() {
        let Some(reason) = text(&error["detail"]).or_else(|| text(&error["title"])) else {
            continue;
        };
        let field = text(&error["source"]["pointer"])
            .map(|pointer| pointer.trim_matches('/').replace('/', "."))
            .filter(|field| !field.is_empty());
        parts.push(match field {
            Some(field) => format!("{reason} ({field})"),
            None => reason,
        });
    }
    match &answer["detail"] {
        Value::Array(items) => {
            for item in items {
                let Some(message) = text(&item["msg"]) else {
                    continue;
                };
                let field: Vec<String> = item["loc"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(text)
                    .filter(|part| part != "body")
                    .collect();
                parts.push(if field.is_empty() {
                    message
                } else {
                    format!("{message} ({})", field.join("."))
                });
            }
        }
        other => parts.extend(text(other)),
    }
    parts.extend(text(&answer["message"]));
    parts.dedup();
    parts.truncate(MAX_REASONS);
    if parts.is_empty() {
        return None;
    }
    Some(
        mask_numbers(&parts.join("; "))
            .chars()
            .take(DETAIL_CHARS)
            .collect(),
    )
}

impl Brand {
    /// The brand in an answer; `None` without an id.
    pub fn parse(answer: &Value) -> Option<Self> {
        let data = record(answer);
        Some(Self {
            brand_id: text(&data["brandId"])?,
            tcr_brand_id: text(&data["tcrBrandId"]),
            status: text(&data["status"]),
            identity_status: text(&data["identityStatus"]),
            failure_reasons: reason_text(&data["failureReasons"]),
            display_name: text(&data["displayName"]),
            company_name: text(&data["companyName"]),
            email: text(&data["email"]),
        })
    }
}

impl Campaign {
    /// The campaign in an answer; `None` without an id.
    pub fn parse(answer: &Value) -> Option<Self> {
        let data = record(answer);
        Some(Self {
            campaign_id: text(&data["campaignId"])?,
            tcr_campaign_id: text(&data["tcrCampaignId"]),
            campaign_status: text(&data["campaignStatus"]),
            status: text(&data["status"]),
            failure_reasons: reason_text(&data["failureReasons"]),
            reference_id: text(&data["referenceId"]),
        })
    }
}

impl NumberAssignment {
    fn parse(answer: &Value) -> Option<Self> {
        let data = record(answer);
        Some(Self {
            phone_number: text(&data["phoneNumber"])?,
            campaign_id: text(&data["telnyxCampaignId"]).or_else(|| text(&data["campaignId"])),
            assignment_status: text(&data["assignmentStatus"]),
            failure_reasons: reason_text(&data["failureReasons"]),
        })
    }
}

/// `path?query` with `pairs` form-encoded.
fn with_query(path: &str, pairs: &[(&str, &str)]) -> String {
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in pairs {
        query.append_pair(key, value);
    }
    format!("{path}?{}", query.finish())
}

impl Telnyx {
    async fn tendlc_send(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        timeout: Option<Duration>,
    ) -> Result<Value, TenDlcError> {
        let key = self.key.as_deref().ok_or(TenDlcError::NotConfigured)?;
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
            .map_err(|err| TenDlcError::Request(err.without_url().to_string()))?;
        let status = response.status();
        if status.is_success() {
            // Some answers have no body (an appeal may answer 204).
            let bytes = response
                .bytes()
                .await
                .map_err(|err| TenDlcError::Request(err.without_url().to_string()))?;
            return Ok(serde_json::from_slice(&bytes).unwrap_or(Value::Null));
        }
        let answer: Value = response.json().await.unwrap_or(Value::Null);
        Err(TenDlcError::Status {
            status: status.as_u16(),
            code: text(&answer["errors"][0]["code"]),
            detail: reasons(&answer),
        })
    }

    /// Registers a brand with TCR (`POST /10dlc/brand`; Telnyx charges for
    /// it).
    pub async fn create_brand(&self, body: &Value) -> Result<Brand, TenDlcError> {
        let answer = self
            .tendlc_send(
                Method::POST,
                "/10dlc/brand",
                Some(body),
                Some(REGISTRY_TIMEOUT),
            )
            .await?;
        Brand::parse(&answer).ok_or(TenDlcError::Unexpected("brand without brandId"))
    }

    /// A brand (`GET /10dlc/brand/{id}`).
    pub async fn get_brand(&self, id: &str) -> Result<Brand, TenDlcError> {
        let answer = self
            .tendlc_send(
                Method::GET,
                &format!("/10dlc/brand/{}", segment(id)),
                None,
                None,
            )
            .await?;
        Brand::parse(&answer).ok_or(TenDlcError::Unexpected("brand without brandId"))
    }

    /// Changes a brand; the whole brand is sent (`PUT /10dlc/brand/{id}`).
    pub async fn update_brand(&self, id: &str, body: &Value) -> Result<Brand, TenDlcError> {
        let answer = self
            .tendlc_send(
                Method::PUT,
                &format!("/10dlc/brand/{}", segment(id)),
                Some(body),
                Some(REGISTRY_TIMEOUT),
            )
            .await?;
        Brand::parse(&answer).ok_or(TenDlcError::Unexpected("brand without brandId"))
    }

    /// Our brands with this display name, newest first
    /// (`GET /10dlc/brand?displayName=`).
    pub async fn find_brands(&self, display_name: &str) -> Result<Vec<Brand>, TenDlcError> {
        let path = with_query(
            "/10dlc/brand",
            &[
                ("displayName", display_name),
                ("recordsPerPage", LIST_SIZE),
                ("sort", "-createdAt"),
            ],
        );
        let answer = self.tendlc_send(Method::GET, &path, None, None).await?;
        Ok(rows(&answer).iter().filter_map(Brand::parse).collect())
    }

    /// Why a brand failed (`GET /10dlc/brand_feedback/{id}`); none for a
    /// brand without problems.
    pub async fn brand_feedback(&self, id: &str) -> Result<Vec<BrandFeedback>, TenDlcError> {
        let answer = match self
            .tendlc_send(
                Method::GET,
                &format!("/10dlc/brand_feedback/{}", segment(id)),
                None,
                None,
            )
            .await
        {
            Ok(answer) => answer,
            Err(err) if err.status() == Some(404) => return Ok(Vec::new()),
            Err(err) => return Err(err),
        };
        Ok(record(&answer)["category"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| {
                Some(BrandFeedback {
                    id: text(&item["id"])?,
                    display_name: text(&item["displayName"]),
                    description: reason_text(&item["description"]),
                    fields: item["fields"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(text)
                        .collect(),
                })
            })
            .collect())
    }

    /// Registers a campaign of a brand (`POST /10dlc/campaignBuilder`;
    /// Telnyx charges three months up front).
    pub async fn create_campaign(&self, body: &Value) -> Result<Campaign, TenDlcError> {
        let answer = self
            .tendlc_send(
                Method::POST,
                "/10dlc/campaignBuilder",
                Some(body),
                Some(REGISTRY_TIMEOUT),
            )
            .await?;
        Campaign::parse(&answer).ok_or(TenDlcError::Unexpected("campaign without campaignId"))
    }

    /// A campaign (`GET /10dlc/campaign/{id}`).
    pub async fn get_campaign(&self, id: &str) -> Result<Campaign, TenDlcError> {
        let answer = self
            .tendlc_send(
                Method::GET,
                &format!("/10dlc/campaign/{}", segment(id)),
                None,
                None,
            )
            .await?;
        Campaign::parse(&answer).ok_or(TenDlcError::Unexpected("campaign without campaignId"))
    }

    /// Changes a campaign's samples, message flow and help message
    /// (`PUT /10dlc/campaign/{id}`); a rejected one goes back to review.
    pub async fn update_campaign(&self, id: &str, body: &Value) -> Result<Campaign, TenDlcError> {
        let answer = self
            .tendlc_send(
                Method::PUT,
                &format!("/10dlc/campaign/{}", segment(id)),
                Some(body),
                Some(REGISTRY_TIMEOUT),
            )
            .await?;
        Campaign::parse(&answer).ok_or(TenDlcError::Unexpected("campaign without campaignId"))
    }

    /// The brand's campaigns (`GET /10dlc/campaign?brandId=`).
    pub async fn brand_campaigns(&self, brand_id: &str) -> Result<Vec<Campaign>, TenDlcError> {
        let path = with_query(
            "/10dlc/campaign",
            &[("brandId", brand_id), ("recordsPerPage", LIST_SIZE)],
        );
        let answer = self.tendlc_send(Method::GET, &path, None, None).await?;
        Ok(rows(&answer).iter().filter_map(Campaign::parse).collect())
    }

    /// Asks Telnyx's compliance team to look at a rejected campaign again
    /// (`POST /10dlc/campaign/{id}/appeal`; `TELNYX_FAILED` or `MNO_REJECTED`
    /// only).
    pub async fn appeal_campaign(&self, id: &str, reason: &str) -> Result<(), TenDlcError> {
        self.tendlc_send(
            Method::POST,
            &format!("/10dlc/campaign/{}/appeal", segment(id)),
            Some(&json!({ "appeal_reason": reason })),
            Some(REGISTRY_TIMEOUT),
        )
        .await
        .map(drop)
    }

    /// What a campaign of `usecase` costs (`GET /10dlc/campaign/usecase_cost`).
    pub async fn campaign_cost(&self, usecase: &str) -> Result<CampaignCost, TenDlcError> {
        let path = with_query("/10dlc/campaign/usecase_cost", &[("usecase", usecase)]);
        let answer = self.tendlc_send(Method::GET, &path, None, None).await?;
        let data = record(&answer);
        Ok(CampaignCost {
            usecase: text(&data["campaignUsecase"]).unwrap_or_else(|| usecase.to_string()),
            up_front_cost: text(&data["upFrontCost"]),
            monthly_cost: text(&data["monthlyCost"]),
            description: text(&data["description"]),
        })
    }

    /// Assigns a number to a campaign (`POST /10dlc/phone_number_campaigns`;
    /// Telnyx charges for each).
    pub async fn assign_number_campaign(
        &self,
        e164: &str,
        campaign_id: &str,
    ) -> Result<NumberAssignment, TenDlcError> {
        let answer = self
            .tendlc_send(
                Method::POST,
                "/10dlc/phone_number_campaigns",
                Some(&json!({ "phoneNumber": e164, "campaignId": campaign_id })),
                None,
            )
            .await?;
        Ok(
            NumberAssignment::parse(&answer).unwrap_or_else(|| NumberAssignment {
                phone_number: e164.to_string(),
                campaign_id: Some(campaign_id.to_string()),
                ..NumberAssignment::default()
            }),
        )
    }

    /// A number's assignment (`GET /10dlc/phone_number_campaigns/{phoneNumber}`);
    /// `None` when it has none.
    pub async fn number_campaign(
        &self,
        e164: &str,
    ) -> Result<Option<NumberAssignment>, TenDlcError> {
        match self
            .tendlc_send(
                Method::GET,
                &format!("/10dlc/phone_number_campaigns/{}", segment(e164)),
                None,
                None,
            )
            .await
        {
            Ok(answer) => Ok(NumberAssignment::parse(&answer)),
            Err(err) if err.status() == Some(404) => Ok(None),
            Err(err) => Err(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_errors_and_telnyx_errors_become_reasons() {
        let validation = json!({ "detail": [
            { "loc": ["body", "ein"], "msg": "Invalid EIN", "type": "value_error" },
            { "loc": ["body", "phone"], "msg": "+13175550100 is not valid", "type": "value_error" } ] });
        assert_eq!(
            reasons(&validation).as_deref(),
            Some("Invalid EIN (ein); [number] is not valid (phone)")
        );
        let errors = json!({ "errors": [
            { "code": "10015", "title": "Bad Request", "detail": "Brand already exists",
              "source": { "pointer": "/displayName" } } ] });
        assert_eq!(
            reasons(&errors).as_deref(),
            Some("Brand already exists (displayName)")
        );
        assert_eq!(
            reasons(&json!({ "detail": "Insufficient funds" })).as_deref(),
            Some("Insufficient funds")
        );
        assert_eq!(reasons(&Value::Null), None);
        let long = json!({ "errors": [{ "detail": "x".repeat(500) }] });
        assert_eq!(reasons(&long).unwrap().chars().count(), DETAIL_CHARS);
    }

    #[test]
    fn records_are_read_bare_or_in_data() {
        let bare = json!({ "brandId": "b-1", "tcrBrandId": "BABC123", "status": "OK",
                           "identityStatus": "VERIFIED", "failureReasons": null });
        let wrapped = json!({ "data": bare.clone() });
        for answer in [&bare, &wrapped] {
            let brand = Brand::parse(answer).unwrap();
            assert_eq!(brand.brand_id, "b-1");
            assert_eq!(brand.status.as_deref(), Some("OK"));
            assert_eq!(brand.failure_reasons, None);
        }
        let campaign = Campaign::parse(&json!({
            "campaignId": "c-1", "campaignStatus": "TELNYX_FAILED", "referenceId": "granite-c1-r1-1",
            "failureReasons": [{ "description": "Opt-out missing; call 317-555-0100" }] }))
        .unwrap();
        assert_eq!(campaign.campaign_status.as_deref(), Some("TELNYX_FAILED"));
        assert_eq!(
            campaign.failure_reasons.as_deref(),
            Some("Opt-out missing; call [number]")
        );
        assert!(Brand::parse(&json!({ "status": "OK" })).is_none());
        assert_eq!(
            reason_text(&json!(["a", { "reason": "b" }])).as_deref(),
            Some("a; b")
        );
    }
}
