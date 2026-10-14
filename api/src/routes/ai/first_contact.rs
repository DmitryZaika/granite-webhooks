//! `POST /v1/ai/first-contact-email`: the first email to a new lead.
//!
//! Written from what the lead told us. Port of the CRM's `api.aiRecommend.email` as its
//! only caller (the email composer's "Generate from customer info") uses it:
//! a first-contact email, no thread history. The CRM streamed the draft;
//! this answers the subject and body once written.

use crate::ai::{AiError, Caller, Chat, MINI_MODEL};
use crate::auth::EmployeeUser;
use crate::auth::session::SessionUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;
use utoipa::ToSchema;

const SYSTEM_PROMPT: &str = "
  You are an expert sales email assistant. Your purpose is to generate professional, persuasive, and realistically usable emails.

  *** STRICT NEGATIVE CONSTRAINTS ***
  1. NO SIGNATURES: Do NOT include \"Best\", \"Sincerely\", \"Regards\", \"Cheers\".
  2. NO NAME AT THE END: Do NOT put the sender's name (e.g. \"Dema\") or company at the end of the email.
  3. NO PLACEHOLDERS: Do not use [brackets].
  4. NO META-TAGS: Do not print \"[End of text]\".

  *** ENDING RULES (CRITICAL) ***
  • The application adds the signature automatically.
  • If you write the name at the end, it will appear TWICE.
  • STOP writing immediately after the last punctuation mark (usually a question mark).
  • WRONG: \"...let me know. Thanks, Dema.\"
  • CORRECT: \"...let me know?\"

  *** HUMAN-LIKE & NATURAL FLOW ***
  • Be conversational. Write like a human responding to a friend or colleague.
  • INTELLIGENT RESPONSIVENESS: If the customer asked for a specific action (e.g., \"come give a quote\"), ADDRESS THAT REQUEST DIRECTLY.
  • CONVERSATION AWARENESS: When previous email messages are provided, treat them as the live thread. Match tone and topic, answer open questions, and continue naturally from the latest message.
  • If the source is \"Facebook\" or \"Instagram\", phrase it casually.

  SUBJECT LINE RULES:
  • Style: Casual, short (2-5 words), sentence case. No \"salesy\" words.

  EMAIL CATEGORY DEFINITIONS
  (Keep your existing category definitions here...)

  FIRST-CONTACT
  Tone: warm and helpful.
  Structure:
  1. Casual opening (\"Hi [Name], this is [Sender] with [Company]\").
  2. Acknowledge intent / Analyze request.
  3. End with a specific question.

  GENERAL RULES
  • PHONE/EMAIL RULES:
    - FIRST-CONTACT: NEVER include phone/email in body.
    - REPLY: Include phone ONLY if asked.

  *** OUTPUT STRUCTURE ***
  1. Provide ONLY the subject line (no \"Subject:\" prefix).
  2. Next line: ---BODY---
  3. Then the email body.
  4. Stop immediately after the last sentence.
";

/// What the lead told us.
#[derive(Debug, Default, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Lead {
    pub customer_name: Option<String>,
    pub customer_company: Option<String>,
    pub lead_message: Option<String>,
    pub remodel_type: Option<String>,
    pub referral_source: Option<String>,
}

/// Who writes.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Sender {
    pub name: Option<String>,
    pub company: Option<String>,
    pub phone: Option<String>,
    pub email: Option<String>,
}

fn filled(value: Option<&String>) -> Option<&str> {
    value.map(String::as_str).filter(|v| !v.is_empty())
}

/// The rep's contact details and the opening rules (`formatSenderInfo`).
fn sender_info(sender: &Sender) -> String {
    let parts: Vec<String> = [
        ("Name", filled(sender.name.as_ref())),
        ("Company", filled(sender.company.as_ref())),
        ("Phone", filled(sender.phone.as_ref())),
        ("Email", filled(sender.email.as_ref())),
    ]
    .into_iter()
    .filter_map(|(label, value)| value.map(|v| format!("{label}: {v}")))
    .collect();
    if parts.is_empty() {
        return String::new();
    }
    format!(
        "Here is my (the sales rep) contact information: {}. INTRODUCTION RULE: Introduce yourself in the FIRST SENTENCE ONLY (e.g., \"Hi [Name], this is Dema with Granite Depot...\"). NEVER mention my name or company at the end of the email. PHONE/EMAIL RULES: For FIRST-CONTACT emails - NEVER write phone or email in the body. Always end FIRST-CONTACT emails with a direct question. For REPLY emails - include phone ONLY if customer asks. ",
        parts.join(", ")
    )
}

/// What the model is asked (`buildUserPrompt`, first contact, no history).
pub fn build_user_prompt(lead: &Lead, sender: &Sender, variation: &str) -> String {
    let mut prompt = String::from(
        "Write a neutral, friendly sales email. Email type: first-contact. Verboseness: concise. Keep the entire email very brief. Focus only on essential information and keep the email short. Urgency level: medium. ",
    );
    prompt.push_str("\nCONTEXT DETAILS:");
    if let Some(name) = filled(lead.customer_name.as_ref()) {
        let _ = write!(prompt, "\n- Recipient Name: {name}");
    }
    if let Some(company) = filled(lead.customer_company.as_ref()) {
        let _ = write!(prompt, "\n- Recipient Company: {company}");
    }
    if let Some(message) = filled(lead.lead_message.as_ref()) {
        let _ = write!(prompt, "\n- Customer's Original Request: \"{message}\"");
        prompt.push_str("\n INSTRUCTION: Analyze the customer's request above. If they asked for a specific action (like a visit, a call, or a quote), respond to that specifically. Do not just say \"tell me more\" if they already asked you to come over.");
    }
    if let Some(source) = filled(lead.referral_source.as_ref()) {
        let _ = write!(prompt, "\n- Source: {source}");
    }
    if let Some(project) = filled(lead.remodel_type.as_ref()) {
        let _ = write!(prompt, "\n- Project Type: {project}");
    }
    if !variation.is_empty() {
        let _ = write!(prompt, "\nVariation hint: {variation}. ");
    }
    let _ = write!(prompt, "\n\n{}", sender_info(sender));
    prompt.push_str("\n  STRICT INSTRUCTION FOR SUBJECT LINE:\n  1. FORMAT: Sentence case. First letter capitalized.\n  2. STYLE: Casual, short (2-5 words).\n  3. CONTENT: Relevant to the project.\n");
    prompt
}

/// "subject\n---BODY---\nbody" → (subject, body).
pub fn split_reply(text: &str) -> (String, String) {
    let mut parts = text.split("---BODY---");
    let subject = parts.next().unwrap_or("").trim().to_string();
    let body = parts.next().unwrap_or("").trim().to_string();
    (subject, body)
}

const LEAD_COLUMNS: &str =
    "NULLIF(c.name, '') AS customer_name, NULLIF(c.company_name, '') AS customer_company,
    COALESCE(NULLIF(c.details, ''), NULLIF(c.your_message, '')) AS lead_message,
    NULLIF(c.remodal_type, '') AS remodel_type, NULLIF(c.referral_source, '') AS referral_source";

/// The deal's customer (else the customer), only the caller's company's.
async fn lead(
    state: &AppState,
    user: &SessionUser,
    deal_id: Option<i32>,
    customer_id: Option<i32>,
) -> Result<Lead, sqlx::Error> {
    if let Some(deal_id) = deal_id {
        let found: Option<Lead> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {LEAD_COLUMNS} FROM deals d JOIN customers c ON c.id = d.customer_id
              WHERE d.id = ? AND d.deleted_at IS NULL AND c.company_id = ? LIMIT 1"
        )))
        .bind(deal_id)
        .bind(user.company_id)
        .fetch_optional(&state.pool)
        .await?;
        if let Some(found) = found {
            return Ok(found);
        }
    }
    let Some(customer_id) = customer_id else {
        return Ok(Lead::default());
    };
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {LEAD_COLUMNS} FROM customers c WHERE c.id = ? AND c.company_id = ? AND c.deleted_at IS NULL LIMIT 1"
    )))
    .bind(customer_id)
    .bind(user.company_id)
    .fetch_optional(&state.pool)
    .await?
    .unwrap_or_default())
}

/// `POST /v1/ai/first-contact-email`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct FirstContactRequest {
    /// The lead's deal.
    pub deal_id: Option<i32>,
    /// The lead, when there is no deal.
    pub customer_id: Option<i32>,
}

/// A drafted email.
#[derive(Debug, Serialize, ToSchema)]
pub struct DraftedEmail {
    /// Short, sentence case.
    pub subject: String,
    /// The email text (the signature is added when sent).
    pub body: String,
}

/// Draft a first email
///
/// The first email to a new lead, from what they told us (their request,
/// project and source), introducing the rep and ending with a question.
#[utoipa::path(
    post,
    path = "/v1/ai/first-contact-email",
    operation_id = "draft_first_contact_email",
    tag = "ai",
    request_body = FirstContactRequest,
    responses(
        (status = 200, description = "The draft", body = DraftedEmail),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 502, description = "The model failed", body = ErrorBody),
        (status = 503, description = "AI is not configured", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn first_contact_email(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(request): JsonBody<FirstContactRequest>,
) -> ApiResult<Json<DraftedEmail>> {
    let lead = lead(&state, &user, request.deal_id, request.customer_id).await?;
    let company: Option<Option<String>> =
        sqlx::query_scalar("SELECT name FROM company WHERE id = ? LIMIT 1")
            .bind(user.company_id)
            .fetch_optional(&state.pool)
            .await?;
    let sender = Sender {
        name: user.name.clone(),
        company: company.flatten(),
        phone: user.phone_number.clone(),
        email: Some(user.email.clone()),
    };
    // A different hint each time, so "Generate" again gives another draft.
    let variation = uuid::Uuid::new_v4().simple().to_string()[..11].to_string();
    let prompt = build_user_prompt(&lead, &sender, &variation);
    let chat = Chat::new(MINI_MODEL)
        .message("system", SYSTEM_PROMPT)
        .message("user", &prompt);
    let caller = Caller {
        feature: "email_draft",
        user_id: Some(user.id),
        company_id: Some(user.company_id),
    };
    let text = state
        .ai
        .chat(&state.pool, caller, &chat)
        .await
        .map_err(|err| match err {
            AiError::NotConfigured => ApiError::Unavailable(err.to_string()),
            other => ApiError::Upstream(format!("Failed to generate AI email: {other}")),
        })?;
    let (subject, body) = split_reply(&text);
    Ok(Json(DraftedEmail { subject, body }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prompt_carries_the_lead_and_the_rep() {
        let lead = Lead {
            customer_name: Some("Ann Lee".into()),
            lead_message: Some("Can you come quote my kitchen?".into()),
            referral_source: Some("Facebook".into()),
            ..Lead::default()
        };
        let sender = Sender {
            name: Some("Rita Rep".into()),
            company: Some("Acme Stone".into()),
            phone: None,
            email: Some("rep@acme.test".into()),
        };
        let prompt = build_user_prompt(&lead, &sender, "abc");
        assert!(prompt.starts_with("Write a neutral, friendly sales email. Email type: first-contact. Verboseness: concise."));
        assert!(prompt.contains("\nCONTEXT DETAILS:\n- Recipient Name: Ann Lee\n- Customer's Original Request: \"Can you come quote my kitchen?\"\n INSTRUCTION:"));
        assert!(prompt.contains("\n- Source: Facebook\nVariation hint: abc. \n\nHere is my (the sales rep) contact information: Name: Rita Rep, Company: Acme Stone, Email: rep@acme.test. INTRODUCTION RULE"));
        assert!(prompt.ends_with("3. CONTENT: Relevant to the project.\n"));
    }

    #[test]
    fn the_reply_splits_into_subject_and_body() {
        assert_eq!(
            split_reply(" Your kitchen quote \n---BODY---\nHi Ann, this is Rita.\n"),
            ("Your kitchen quote".into(), "Hi Ann, this is Rita.".into())
        );
        assert_eq!(
            split_reply("Only subject"),
            ("Only subject".into(), String::new())
        );
    }
}
