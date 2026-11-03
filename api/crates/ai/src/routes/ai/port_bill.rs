//! `POST /v1/ai/port-bill-read`: the AI reads the current carrier's bill and
//! fills a port's Business details (the **Upload your bill** box at the top
//! of the step). The admin checks every field before saving: AI makes
//! mistakes.

use crate::ai::{Caller, Chat, LARGE_MODEL};
use crate::auth::AdminUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::time::Duration;

/// The largest bill read: Telnyx takes documents of at most 4 MB.
pub const MAX_BILL_BYTES: usize = 4 * 1024 * 1024;
/// The request body: the bill in base64 (4/3 of it) and the JSON around it.
pub const MAX_BILL_BODY_BYTES: usize = MAX_BILL_BYTES / 3 * 4 + 64 * 1024;

const PROMPT: &str = r#"This is a bill from a US or Canada phone carrier (RingCentral, AT&T, Verizon, Comcast, a VoIP provider...). The customer is moving phone numbers away from that carrier, and the losing carrier rejects the move unless these details match its records exactly.

Read the bill and copy, exactly as printed (same spelling, abbreviations and capitalisation; do not correct or expand anything):
- entityName: the business name on the account. When the bill shows only a person, use null.
- authPersonName: the person the account is billed to or the account holder, when the bill names one; else null.
- billingPhoneNumber: the account's main / billing telephone number (BTN), 10 digits. On RingCentral the account number is the main company number, so use it here too. Else null.
- accountNumber: the account number as printed.
- streetAddress: the service address's street line (the billed-to / service address, never the carrier's own "pay to" address), without the suite.
- extendedAddress: suite, floor or unit, or null.
- locality: the city.
- administrativeArea: the 2-letter state or province code.
- postalCode: the ZIP or postal code.
- countryCode: "US" or "CA".

Use null for anything the bill does not show; never guess. Ignore card numbers and amounts.

Reply with only a JSON object with exactly these keys: entityName, authPersonName, billingPhoneNumber, accountNumber, streetAddress, extendedAddress, locality, administrativeArea, postalCode, countryCode."#;

/// Body of `POST /v1/ai/port-bill-read`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PortBillInput {
    /// The file's name, e.g. `bill.pdf`.
    pub filename: String,
    /// The bill, base64: a PDF, JPEG or PNG of at most 4 MB.
    pub content_base64: String,
}

/// What the AI read off the bill; null where the bill does not show it.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PortBillDetails {
    /// The business name on the account.
    #[serde(default)]
    pub entity_name: Option<String>,
    /// The person the account is billed to.
    #[serde(default)]
    pub auth_person_name: Option<String>,
    /// The account's main (billing) number, as printed.
    #[serde(default)]
    pub billing_phone_number: Option<String>,
    /// The account number, as printed.
    #[serde(default)]
    pub account_number: Option<String>,
    /// The service address's street line.
    #[serde(default)]
    pub street_address: Option<String>,
    /// Suite, floor or unit.
    #[serde(default)]
    pub extended_address: Option<String>,
    /// The city.
    #[serde(default)]
    pub locality: Option<String>,
    /// The 2-letter state or province.
    #[serde(default)]
    pub administrative_area: Option<String>,
    /// The ZIP or postal code.
    #[serde(default)]
    pub postal_code: Option<String>,
    /// `US` or `CA`.
    #[serde(default)]
    pub country_code: Option<String>,
}

/// The bill's type by its first bytes: its MIME type.
fn bill_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"%PDF-") {
        Some("application/pdf")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some("image/png")
    } else {
        None
    }
}

/// The content part that carries the bill to the model.
fn bill_part(mime: &str, filename: &str, data: &str) -> serde_json::Value {
    let url = format!("data:{mime};base64,{data}");
    if mime == "application/pdf" {
        let name = filename
            .rsplit(['/', '\\'])
            .next()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or("bill.pdf");
        json!({ "type": "file", "file": { "filename": name, "file_data": url } })
    } else {
        json!({ "type": "image_url", "image_url": { "url": url, "detail": "high" } })
    }
}

/// A field as read: trimmed, at most 100 characters, null when empty.
fn clean(value: Option<String>) -> Option<String> {
    let value = value?.trim().to_owned();
    if value.is_empty() || value.eq_ignore_ascii_case("null") {
        return None;
    }
    Some(value.chars().take(100).collect())
}

/// The details out of the AI's reply, each one cleaned; the state upper-case
/// when it is 2 letters (else dropped), the country only `US` or `CA`.
fn parse_details(reply: &str) -> Option<PortBillDetails> {
    let cleaned = reply.replace("```json", "").replace("```", "");
    let parsed: PortBillDetails = serde_json::from_str(cleaned.trim()).ok()?;
    let state = clean(parsed.administrative_area)
        .filter(|state| state.len() == 2 && state.chars().all(|c| c.is_ascii_alphabetic()))
        .map(|state| state.to_ascii_uppercase());
    let country = clean(parsed.country_code)
        .map(|country| country.to_ascii_uppercase())
        .filter(|country| country == "US" || country == "CA");
    Some(PortBillDetails {
        entity_name: clean(parsed.entity_name),
        auth_person_name: clean(parsed.auth_person_name),
        billing_phone_number: clean(parsed.billing_phone_number),
        account_number: clean(parsed.account_number),
        street_address: clean(parsed.street_address),
        extended_address: clean(parsed.extended_address),
        locality: clean(parsed.locality),
        administrative_area: state,
        postal_code: clean(parsed.postal_code),
        country_code: country,
    })
}

/// Read a port's business details from the carrier's bill with AI
///
/// Reads a PDF, JPEG or PNG of the current carrier's bill (at most 4 MB,
/// sent base64) and returns the Business details it shows, copied as
/// printed. Nothing is saved, kept or sent to Telnyx. Company admins (and
/// super admins, superusers) only.
#[crate::openapi::path(
    post,
    path = "/v1/ai/port-bill-read",
    operation_id = "read_port_bill",
    tag = "ai",
    request_body = PortBillInput,
    responses(
        (status = 200, description = "What the bill shows", body = PortBillDetails),
        (status = 400, description = "Not base64, empty, too big, or not a PDF, JPEG or PNG", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 502, description = "OpenAI failed or answered without the details", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn read_port_bill(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<PortBillInput>,
) -> ApiResult<Json<PortBillDetails>> {
    let bytes = STANDARD
        .decode(input.content_base64.trim())
        .map_err(|_| ApiError::BadRequest("The file did not arrive whole. Try again.".into()))?;
    if bytes.is_empty() {
        return Err(ApiError::BadRequest("The file is empty.".into()));
    }
    if bytes.len() > MAX_BILL_BYTES {
        return Err(ApiError::BadRequest(
            "The file is too big: upload a PDF, JPEG or PNG of at most 4 MB.".into(),
        ));
    }
    let mime =
        bill_type(&bytes).ok_or_else(|| ApiError::BadRequest("Upload a PDF, JPEG or PNG.".into()))?;
    let data = STANDARD.encode(&bytes);
    let mut chat = Chat::new(LARGE_MODEL);
    chat.messages.push(json!({
        "role": "user",
        "content": [bill_part(mime, &input.filename, &data), { "type": "text", "text": PROMPT }],
    }));
    chat.json = true;
    chat.reasoning_effort = Some("low");
    chat.timeout = Duration::from_secs(28);
    let caller = Caller {
        feature: "port_bill_read",
        user_id: Some(user.id),
        company_id: Some(user.company_id),
    };
    let reply = state
        .ai
        .chat(&state.pool, caller, &chat)
        .await
        .map_err(|err| ApiError::Upstream(err.to_string()))?;
    let details = parse_details(&reply).ok_or_else(|| {
        ApiError::Upstream("Could not read the bill. Try again or type the details.".into())
    })?;
    Ok(Json(details))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn details_are_copied_as_printed_and_cleaned() {
        let reply = r#"```json
{"entityName":null,"authPersonName":" Georgy Galkin ","billingPhoneNumber":"(463) 329-2269",
 "accountNumber":"(463) 329-2269","streetAddress":"1175 W 16th str","extendedAddress":"",
 "locality":"Indianapolis","administrativeArea":"in","postalCode":"46202","countryCode":"us"}
```"#;
        assert_eq!(
            parse_details(reply).unwrap(),
            PortBillDetails {
                entity_name: None,
                auth_person_name: Some("Georgy Galkin".into()),
                billing_phone_number: Some("(463) 329-2269".into()),
                account_number: Some("(463) 329-2269".into()),
                street_address: Some("1175 W 16th str".into()),
                extended_address: None,
                locality: Some("Indianapolis".into()),
                administrative_area: Some("IN".into()),
                postal_code: Some("46202".into()),
                country_code: Some("US".into()),
            }
        );
        let odd = parse_details(r#"{"administrativeArea":"Indiana","countryCode":"MX"}"#).unwrap();
        assert_eq!(odd.administrative_area, None);
        assert_eq!(odd.country_code, None);
        assert!(parse_details("not json").is_none());
    }

    #[test]
    fn a_pdf_goes_as_a_file_and_a_photo_as_an_image() {
        assert_eq!(bill_type(b"%PDF-1.7"), Some("application/pdf"));
        assert_eq!(bill_type(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(bill_type(b"GIF89a"), None);
        let pdf = bill_part("application/pdf", "C:\\bills\\oct.pdf", "QQ==");
        assert_eq!(pdf["file"]["filename"], "oct.pdf");
        assert_eq!(pdf["file"]["file_data"], "data:application/pdf;base64,QQ==");
        let photo = bill_part("image/png", "bill.png", "QQ==");
        assert_eq!(photo["image_url"]["url"], "data:image/png;base64,QQ==");
    }
}
