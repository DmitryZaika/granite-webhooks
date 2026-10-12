//! `POST /v1/ai/email-template`: an email template drafted from a short
//! description. Remix: `api.aiRecommend.emailTemplate` (prompt unchanged).

use crate::ai::{Caller, Chat, MINI_MODEL};
use crate::auth::AdminUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use std::fmt::Write;
use utoipa::ToSchema;

/// The CRM's `TEMPLATE_VARIABLES`: key and label.
const VARIABLES: [(&str, &str); 10] = [
    ("user.first_name", "Employee first name"),
    ("customer.first_name", "Customer First name"),
    ("customer.inventory_page", "Customer inventory page"),
    ("company.name", "Company name"),
    ("user.email", "Employee email"),
    ("user.phone_number", "Employee phone number"),
    ("customer.name", "Customer Full name"),
    ("current_date", "Current day and month"),
    ("company.hours_of_operation", "Hours of operation"),
    ("customer.address", "Customer address"),
];

fn system_prompt() -> String {
    let mut variables = String::new();
    for (index, (key, label)) in VARIABLES.iter().enumerate() {
        if index > 0 {
            variables.push('\n');
        }
        let _ = write!(variables, "  - {{{{{key}}}}}: {label}");
    }
    format!(
        r#"You are an expert email template designer. Your task is to generate professional email templates for a CRM system.

The system supports template variables enclosed in double curly braces like {{{{variable_name}}}}.

AVAILABLE BUILT-IN VARIABLES (automatically replaced by the system):
{variables}

IMPORTANT RULES:
1. Prefer using the built-in variables above whenever possible — they are automatically filled in by the system.
2. You MAY introduce custom variables like {{{{custom_field}}}} ONLY if absolutely necessary for the email's purpose. Note that custom variables require manual replacement by the user every time they send the email, so avoid them unless the template truly cannot work without that specific data.
3. The template body should be in HTML format suitable for a rich text editor.
4. Keep the language professional and warm.
5. Keep the subject line concise and compelling.
6. Do NOT include a signature block (the system adds it automatically).
7. Do NOT include the sender's name at the end.

OUTPUT FORMAT:
Respond with a JSON object containing three fields:

{{
  "template_name": "A short, descriptive name for this template (e.g. 'Follow-up After Estimate')",
  "template_subject": "The email subject line (may include template variables)",
  "template_body": "The HTML body of the email (may include template variables and basic HTML tags like <p>, <br>, <b>, <i>)"
}}

Return ONLY the JSON object with no additional text, markdown fences, or explanations."#
    )
}

/// Body of `POST /v1/ai/email-template`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct EmailTemplateInput {
    /// What the email is about.
    pub prompt: String,
}

/// A drafted template.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct EmailTemplateDraft {
    /// A short name for the template.
    pub template_name: String,
    /// Subject line; may hold `{{variables}}`.
    pub template_subject: String,
    /// HTML body; may hold `{{variables}}`.
    pub template_body: String,
}

/// Draft an email template
///
/// Writes a template name, subject and HTML body from a description, using
/// the built-in `{{variables}}` where it can.
#[utoipa::path(
    post,
    path = "/v1/ai/email-template",
    operation_id = "draft_email_template",
    tag = "ai",
    request_body = EmailTemplateInput,
    responses(
        (status = 200, description = "Template", body = EmailTemplateDraft),
        (status = 400, description = "Empty description", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 502, description = "OpenAI failed or answered without a template", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn email_template(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<EmailTemplateInput>,
) -> ApiResult<Json<EmailTemplateDraft>> {
    let prompt = input.prompt.trim();
    if prompt.is_empty() {
        return Err(ApiError::BadRequest(
            "Please describe what the email is about".into(),
        ));
    }
    let chat = Chat::new(MINI_MODEL)
        .message("system", &system_prompt())
        .message(
            "user",
            &format!("Generate an email template based on this description: \"{prompt}\""),
        );
    let caller = Caller {
        feature: "email_template_generation",
        user_id: Some(user.id),
        company_id: Some(user.company_id),
    };
    let reply = state
        .ai
        .chat(&state.pool, caller, &chat)
        .await
        .map_err(|err| ApiError::Upstream(err.to_string()))?;
    let cleaned = reply.replace("```json", "").replace("```", "");
    let draft: EmailTemplateDraft = serde_json::from_str(cleaned.trim())
        .map_err(|_| ApiError::Upstream("Failed to parse AI response. Please try again.".into()))?;
    if draft.template_name.is_empty()
        || draft.template_subject.is_empty()
        || draft.template_body.is_empty()
    {
        return Err(ApiError::Upstream(
            "AI response missing required fields. Please try again.".into(),
        ));
    }
    Ok(Json(draft))
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_prompt_lists_variables_in_braces() {
        let prompt = super::system_prompt();
        assert!(
            prompt.contains(
                "  - {{user.first_name}}: Employee first name\n  - {{customer.first_name}}"
            )
        );
        assert!(prompt.contains("like {{variable_name}}."));
        assert!(prompt.contains("{\n  \"template_name\""));
    }
}
