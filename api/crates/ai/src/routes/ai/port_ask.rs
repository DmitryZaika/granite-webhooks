//! `POST /v1/ai/port-ask`: the AI answers a question about a port's steps,
//! such as what goes in a field or what to upload (the **Ask AI** box under
//! a port's steps).

use super::port_check::{PortCheckInput, user_message};
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

/// The longest question read.
const MAX_QUESTION_CHARS: usize = 1000;

const SYSTEM_PROMPT: &str = r#"You help a business owner port (move) their US/Canada phone numbers from their current carrier to Telnyx in Granite Manager. Answer their question about the port's steps: what to type in a field, where to find it, or what to upload. Use the step they are on and the details they typed (given as JSON). Answer in plain words, short (1-4 sentences, or a few short bullets), with no Markdown headings. When the answer is on their bill, say where on a bill it usually is. Never ask them to tell you the PIN itself. If you do not know, say so and suggest asking the current carrier.

The steps:
1. Business details. Everything must match the current (losing) carrier's records exactly, or the carrier rejects the port: copy it from the bill as printed.
   - "Your current bill": upload a recent bill (PDF, JPEG or PNG, up to 4 MB); AI fills the fields from it and attaches it as the recent bill. Check every field afterwards.
   - Business name on the account: the account holder's business name exactly as on the bill.
   - Authorized person: the person allowed to make changes on the account, who signs the Letter of Authorization (an owner or manager).
   - Billing number (BTN): the account's main/billing telephone number shown on the bill.
   - Account number: the account number printed on the bill. On RingCentral the account number is the main company number, so it can look like a phone number. Wireless carriers sometimes use a different number than the one on the bill: ask them.
   - Port-out PIN: a passcode for moving numbers away, set or given by the current carrier (from its app, website, or support; for wireless carriers often by dialing a code or in the account settings; RingCentral: Admin Portal or support). It is not on the bill. Many carriers require it.
   - Service address: the address on the carrier's account (the service/billed-to address on the bill, not the carrier's own address), spelled as on the bill. Suite or floor is optional.
   - New billing number: shown only when the billing number itself is being ported in a partial port; a number that stays with the old carrier becomes the account's billing number.
2. Documents.
   - Letter of Authorization (LOA): Telnyx's form, filled from the business details; the authorized person signs it on screen ("Sign now"), or gets an emailed link ("Send to sign"). Paper: download, sign, upload.
   - Recent bill: a bill from the current carrier from the last 30 days that shows the account holder, account number, service address and the numbers. PDF, JPEG or PNG, up to 4 MB. A screenshot of the account page or a Customer Service Record (CSR) can work when there is no bill; ask the carrier for a CSR.
3. Port date: pick when the numbers move; the current carrier confirms the final date (FOC date). The numbers keep working with the old carrier until then.
4. Review: check everything and submit to Telnyx. After submitting, Telnyx may ask for more documents under Requirements on the Status tab."#;

/// Body of `POST /v1/ai/port-ask`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PortAskInput {
    /// What the user asks.
    pub question: String,
    /// The step they are on: `business`, `documents`, `date` or `review`.
    #[serde(default)]
    pub step: String,
    /// Whether the signed LOA and the recent bill are attached.
    #[serde(default)]
    pub loa_attached: bool,
    #[serde(default)]
    pub bill_attached: bool,
    /// The Business details as typed (the PIN itself is not sent).
    pub details: PortCheckInput,
}

/// The AI's answer.
#[derive(Debug, Serialize, ToSchema)]
pub struct PortAskResult {
    pub answer: String,
}

fn context(input: &PortAskInput) -> String {
    json!({
        "step": match input.step.as_str() {
            "documents" => "2. Documents",
            "date" => "3. Port date",
            "review" => "4. Review",
            _ => "1. Business details",
        },
        "loa_attached": input.loa_attached,
        "bill_attached": input.bill_attached,
        "details": serde_json::from_str::<serde_json::Value>(&user_message(&input.details))
            .unwrap_or_default(),
    })
    .to_string()
}

/// Ask AI about a port's steps
///
/// Answers a question such as what goes in a field or what to upload, from
/// the step the user is on and the details as typed. Nothing is saved.
#[crate::openapi::path(
    post,
    path = "/v1/ai/port-ask",
    operation_id = "ask_port",
    tag = "ai",
    request_body = PortAskInput,
    responses(
        (status = 200, description = "The answer", body = PortAskResult),
        (status = 400, description = "No question, or a question too long", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 502, description = "OpenAI failed or answered nothing", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn port_ask(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<PortAskInput>,
) -> ApiResult<Json<PortAskResult>> {
    let question = input.question.trim();
    if question.is_empty() {
        return Err(ApiError::BadRequest("Type a question.".into()));
    }
    if question.chars().count() > MAX_QUESTION_CHARS {
        return Err(ApiError::BadRequest(format!(
            "Keep the question under {MAX_QUESTION_CHARS} characters."
        )));
    }
    let chat = Chat::new(MINI_MODEL)
        .message("system", SYSTEM_PROMPT)
        .message(
            "user",
            &format!(
                "Where I am:\n{}\n\nMy question: {question}",
                context(&input)
            ),
        );
    let caller = Caller {
        feature: "port_ask",
        user_id: Some(user.id),
        company_id: Some(user.company_id),
    };
    let reply = state
        .ai
        .chat(&state.pool, caller, &chat)
        .await
        .map_err(|err| ApiError::Upstream(err.to_string()))?;
    let answer = reply.trim().to_owned();
    if answer.is_empty() {
        return Err(ApiError::Upstream(
            "The AI answered nothing. Try again.".into(),
        ));
    }
    Ok(Json(PortAskResult { answer }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_names_the_step_and_never_the_pin() {
        let input: PortAskInput = serde_json::from_value(json!({
            "question": "What do I upload?",
            "step": "documents",
            "billAttached": true,
            "details": { "pinSet": true, "accountNumber": "123" },
        }))
        .unwrap();
        let text = context(&input);
        assert!(text.contains("2. Documents"));
        assert!(text.contains("\"bill_attached\":true"));
        assert!(text.contains("\"accountNumber\":\"123\""));
        assert!(text.contains("\"pinPasscode\":\"(entered)\""));
    }
}
