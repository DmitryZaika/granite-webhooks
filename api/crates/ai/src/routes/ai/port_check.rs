//! `POST /v1/ai/port-details-check`: the AI reads a port's business details
//! before they are saved and says what looks wrong (the **Check with AI**
//! button on a port's Business details step).

use crate::ai::{Caller, Chat, MINI_MODEL};
use crate::auth::AdminUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use serde_json::json;

/// The form's fields the AI may name, with their labels on the page.
const FIELDS: [(&str, &str); 13] = [
    ("entityName", "Business name on the account"),
    ("authPersonName", "Authorized person"),
    ("billingPhoneNumber", "Billing number (BTN)"),
    ("accountNumber", "Account number"),
    ("pinPasscode", "Port-out PIN"),
    ("streetAddress", "Street address"),
    ("extendedAddress", "Suite or floor"),
    ("locality", "City"),
    ("administrativeArea", "State or province"),
    ("postalCode", "ZIP or postal code"),
    ("countryCode", "Country"),
    ("remainingNumbersAction", "The numbers that stay"),
    ("newBillingPhoneNumber", "New billing number"),
];

const SYSTEM_PROMPT: &str = r#"You check the business details of a US/Canada phone number port order to Telnyx before the user saves them. Telnyx sends them to the losing carrier, which rejects the port when they do not match its records exactly.

Report only real, likely problems, each one short and plain, saying what to fix. Check:
- A required field is empty: business name, authorized person, billing number (BTN), account number, street address, city, state or province, ZIP or postal code. Suite or floor is optional.
- The billing number is not a valid 10-digit US/Canada number.
- The state is not a 2-letter code; the ZIP does not fit the country (5 digits, or ZIP+4, in the US; A1A 1A1 in Canada); the ZIP or city clearly does not belong to the state.
- The street address looks abbreviated or misspelled in a way the carrier's record would not be (e.g. "str" instead of "St").
- The business name looks like a person's name, or the authorized person looks like a company.
- The account number looks like a phone number or a placeholder.
- No port-out PIN: say that many carriers (wireless carriers, RingCentral and many VoIP providers) require one, and to ask the current carrier — only when the PIN is missing.
- Partial port (only some numbers on the account move): "The numbers that stay" must be chosen. When the billing number is one of the numbers being ported, a new billing number is required and must be one of the numbers that stay (not one being ported). When the billing number is not being ported, the new billing number should be empty.
- Full port (every number moves): the numbers being ported should include the billing number.
Do not invent problems, do not repeat a problem, and do not comment on fields that look fine.

Reply with a JSON object: {"issues": [{"field": "<field id or null>", "message": "<what to fix>"}]}. "field" is one of: entityName, authPersonName, billingPhoneNumber, accountNumber, pinPasscode, streetAddress, extendedAddress, locality, administrativeArea, postalCode, countryCode, remainingNumbersAction, newBillingPhoneNumber, or null when the problem is not about one field. An empty "issues" list means everything looks right."#;

/// Body of `POST /v1/ai/port-details-check`: the form as typed.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PortCheckInput {
    /// The numbers being ported.
    #[serde(default)]
    pub phone_numbers: Vec<String>,
    /// The losing carrier, when Telnyx named it.
    #[serde(default)]
    pub old_carrier: Option<String>,
    #[serde(default)]
    pub entity_name: String,
    #[serde(default)]
    pub auth_person_name: String,
    #[serde(default)]
    pub billing_phone_number: String,
    #[serde(default)]
    pub account_number: String,
    /// Whether a PIN is typed or already saved (the PIN itself is not sent).
    #[serde(default)]
    pub pin_set: bool,
    #[serde(default)]
    pub street_address: String,
    #[serde(default)]
    pub extended_address: String,
    #[serde(default)]
    pub locality: String,
    #[serde(default)]
    pub administrative_area: String,
    #[serde(default)]
    pub postal_code: String,
    #[serde(default)]
    pub country_code: String,
    /// `full` or `partial`.
    #[serde(default)]
    pub kind: String,
    /// `keep`, `disconnect` or empty.
    #[serde(default)]
    pub remaining_numbers_action: String,
    #[serde(default)]
    pub new_billing_phone_number: String,
}

/// One problem the AI found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PortCheckIssue {
    /// The form field it is about, or null.
    pub field: Option<String>,
    /// What to fix.
    pub message: String,
}

/// What the AI found; empty when everything looks right.
#[derive(Debug, Serialize, ToSchema)]
pub struct PortCheckResult {
    pub issues: Vec<PortCheckIssue>,
}

pub(crate) fn user_message(input: &PortCheckInput) -> String {
    json!({
        "numbers_being_ported": input.phone_numbers,
        "losing_carrier": input.old_carrier,
        "entityName": input.entity_name.trim(),
        "authPersonName": input.auth_person_name.trim(),
        "billingPhoneNumber": input.billing_phone_number.trim(),
        "accountNumber": input.account_number.trim(),
        "pinPasscode": if input.pin_set { "(entered)" } else { "" },
        "streetAddress": input.street_address.trim(),
        "extendedAddress": input.extended_address.trim(),
        "locality": input.locality.trim(),
        "administrativeArea": input.administrative_area.trim(),
        "postalCode": input.postal_code.trim(),
        "countryCode": input.country_code.trim(),
        "port_kind": if input.kind == "partial" {
            "partial (some numbers on the account stay)"
        } else {
            "full (every number on the account moves)"
        },
        "remainingNumbersAction": input.remaining_numbers_action.trim(),
        "newBillingPhoneNumber": input.new_billing_phone_number.trim(),
    })
    .to_string()
}

/// The issues out of the AI's reply: unknown fields become null, empty
/// messages and repeats are dropped.
fn parse_issues(reply: &str) -> Option<Vec<PortCheckIssue>> {
    #[derive(Deserialize)]
    struct Reply {
        #[serde(default)]
        issues: Vec<PortCheckIssue>,
    }
    let cleaned = reply.replace("```json", "").replace("```", "");
    let parsed: Reply = serde_json::from_str(cleaned.trim()).ok()?;
    let mut issues: Vec<PortCheckIssue> = Vec::new();
    for issue in parsed.issues {
        let message = issue.message.trim().to_owned();
        if message.is_empty() {
            continue;
        }
        let field = issue
            .field
            .filter(|field| FIELDS.iter().any(|(id, _)| id == field));
        let issue = PortCheckIssue { field, message };
        if !issues.contains(&issue) {
            issues.push(issue);
        }
    }
    Some(issues)
}

/// Check a port's business details with AI
///
/// Reads the Business details step as typed and lists what looks wrong
/// before it is saved. Nothing is saved or sent to Telnyx.
#[crate::openapi::path(
    post,
    path = "/v1/ai/port-details-check",
    operation_id = "check_port_details",
    tag = "ai",
    request_body = PortCheckInput,
    responses(
        (status = 200, description = "What looks wrong", body = PortCheckResult),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 502, description = "OpenAI failed or answered without a list", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn port_details_check(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<PortCheckInput>,
) -> ApiResult<Json<PortCheckResult>> {
    let mut chat = Chat::new(MINI_MODEL)
        .message("system", SYSTEM_PROMPT)
        .message("user", &user_message(&input));
    chat.json = true;
    let caller = Caller {
        feature: "port_details_check",
        user_id: Some(user.id),
        company_id: Some(user.company_id),
    };
    let reply = state
        .ai
        .chat(&state.pool, caller, &chat)
        .await
        .map_err(|err| ApiError::Upstream(err.to_string()))?;
    let issues = parse_issues(&reply)
        .ok_or_else(|| ApiError::Upstream("Could not read the AI's answer. Try again.".into()))?;
    Ok(Json(PortCheckResult { issues }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issues_keep_known_fields_and_drop_blanks_and_repeats() {
        let reply = r#"```json
{"issues":[
 {"field":"streetAddress","message":"Write \"St\", not \"str\"."},
 {"field":"streetAddress","message":"Write \"St\", not \"str\"."},
 {"field":"made_up","message":"Something else"},
 {"field":null,"message":"  "}
]}
```"#;
        assert_eq!(
            parse_issues(reply).unwrap(),
            [
                PortCheckIssue {
                    field: Some("streetAddress".into()),
                    message: "Write \"St\", not \"str\".".into(),
                },
                PortCheckIssue {
                    field: None,
                    message: "Something else".into(),
                },
            ]
        );
        assert_eq!(parse_issues(r#"{"issues":[]}"#).unwrap(), []);
        assert!(parse_issues("not json").is_none());
    }

    #[test]
    fn the_pin_itself_never_reaches_the_ai() {
        let input: PortCheckInput =
            serde_json::from_value(json!({ "pinSet": true, "kind": "partial" })).unwrap();
        let message = user_message(&input);
        assert!(message.contains("\"pinPasscode\":\"(entered)\""));
        assert!(message.contains("partial (some numbers"));
    }
}
