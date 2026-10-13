//! `POST /v1/ai/improve`: rewrite a draft email or text at a friendly,
//! professional tone. The page tidies the spacing and restores links.
//! Remix: `api.aiImprove.email` (prompts unchanged).

use crate::ai::{Caller, Chat, MINI_MODEL};
use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

const SHARED: &str = "Your job is to improve customer-facing business messages at tone level 2 on a 1-3 scale. Level 1 is too plain: only grammar and spelling fixes with almost no word changes. Level 3 is too fancy: stiff, wordy, or corporate. Always write level 2: clear, friendly, professional, and noticeably improved wording. Fix grammar and spelling, and upgrade weak or casual phrasing to slightly better words while keeping the same meaning and a natural voice. You must improve wording, not just proofread. Words like typically, prepare, and please let me know are good level 2 wording. Do not avoid them. Good level 2 upgrades: \"you said you'll call me\" -> \"you mentioned you'll call me back\"; \"I wanted to check in\" -> \"I wanted to follow up\"; \"let me know if that works\" -> \"please let me know if that works for you\"; \"we cut a 30 inch hole\" -> \"we typically prepare a 30 inch opening for the stove\". Do not go to level 3. Avoid stiff phrasing such as \"consistently ensures\", \"please be advised\", \"identified any factors\", \"may necessitate\", \"at your earliest convenience\", or adding extra clauses that sound like legal or marketing copy. Keep sentences straightforward. Keep commas. Do not delete commas that are already there, and use a comma where normal English needs one: after a name you are speaking to (\"I'm sorry, Kevin\") and before but (\"isn't feeling well, but\"). Do not add new facts, promises, dates, prices, or ideas. Never change a day, date, time, or other fact that is already in the draft. Monday must stay Monday. Do not use any outside conversation to rewrite the draft. The only customer detail you may correct is a misspelled first name, and only when a separate name instruction gives that exact name. Keep every http or https URL exactly as written — do not remove, shorten, or rewrite deposit, payment, or other links. Keep the same language as the input. Return only the improved text with no explanations or labels. Bad level 3: \"Please be advised that we will prepare your opening on schedule, which consistently ensures a proper fit. Kindly inform us if you have identified any factors that may necessitate on-site cutting at your earliest convenience.\" Good level 2: \"We typically prepare a 30 inch opening for the stove, which should give you a proper fit. Please let me know if anything on your end would need cutting on site.\"";

const PARTIAL: &str = "The input is only a selected part of a longer message, not the whole message. Improve just this part so it still fits back in the same place: do not add a greeting, sign-off, or anything before or after it, and keep a sentence fragment a fragment. If it starts or ends mid-sentence, keep it starting or ending the same way.";

const LISTS: &str = "If the input has numbered lists (1. 2. 3.), bullet lists, or intentional line breaks, keep each item on its own line in the same order. Do not flatten a list into one paragraph. Match the spacing of the input between list items: if the input has a blank line between list items, keep a blank line between them; if it does not, put each item on the line right after the previous one. If the input has no blank line between lines, do not insert blank lines between sentences.";

/// Which kind of message.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    /// An email.
    #[default]
    Email,
    /// A text.
    Sms,
}

/// Body of `POST /v1/ai/improve`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ImproveInput {
    /// The draft.
    pub body: String,
    /// `email` (default) or `sms`.
    #[serde(default)]
    pub channel: Channel,
    /// The customer's name, to fix the spelling of their first name.
    #[serde(default)]
    pub customer_name: Option<String>,
    /// `true` when `body` is only the part of a longer draft the user
    /// selected: the reply then fits back in its place (no greeting or
    /// sign-off, fragments stay fragments). Defaults to `false`.
    #[serde(default)]
    pub partial: bool,
}

/// `POST /v1/ai/improve`.
#[derive(Debug, Serialize, ToSchema)]
pub struct Improved {
    /// The rewritten draft.
    pub body: String,
}

fn system_prompt(channel: Channel) -> String {
    match channel {
        Channel::Email => format!(
            "{SHARED} You edit customer-facing business emails. Preserve the original structure, paragraphs, and line breaks when they help readability. {LISTS} Keep a warm, natural tone. If you see the term \"wordPress\" as a source, replace it with \"Web-site\"."
        ),
        Channel::Sms => format!(
            "{SHARED} You edit SMS text messages. Keep the message concise and natural for texting. Preserve the original structure. {LISTS} For short single-paragraph SMS with no list, you may keep it as one or two short lines. Keep proper names capitalized. If the original includes a payment or deposit link, that exact URL must stay in the improved message."
        ),
    }
}

fn name_rule(customer_name: Option<&str>) -> Option<String> {
    let first = customer_name?.split_whitespace().next()?;
    Some(format!(
        "The customer's first name is \"{first}\". If the message addresses this customer by a first name, correct that name to this exact spelling. Do not change anyone else's name. Do not add this name when the message does not already address the customer by name."
    ))
}

/// Improve a draft
///
/// Rewrites a customer-facing email or text: grammar, spelling and slightly
/// better wording, same meaning, facts and links. The reply is the raw text.
/// With `partial`, only a selected part of a draft is sent and the reply is
/// shaped to be put back in its place.
#[utoipa::path(
    post,
    path = "/v1/ai/improve",
    operation_id = "improve_message",
    tag = "ai",
    request_body = ImproveInput,
    responses(
        (status = 200, description = "Improved draft", body = Improved),
        (status = 400, description = "Empty draft", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 502, description = "OpenAI failed", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn improve(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<ImproveInput>,
) -> ApiResult<Json<Improved>> {
    if input.body.is_empty() {
        return Err(ApiError::BadRequest("Invalid request data".into()));
    }
    let mut chat = Chat::new(MINI_MODEL).message("system", &system_prompt(input.channel));
    if let Some(rule) = name_rule(input.customer_name.as_deref()) {
        chat = chat.message("system", &rule);
    }
    if input.partial {
        chat = chat.message("system", PARTIAL);
    }
    chat = chat.message("user", &input.body);
    let caller = Caller {
        feature: "email_improve",
        user_id: Some(user.id),
        company_id: Some(user.company_id),
    };
    let text = state
        .ai
        .chat(&state.pool, caller, &chat)
        .await
        .map_err(|err| ApiError::Upstream(err.to_string()))?;
    Ok(Json(Improved {
        body: text.trim().to_string(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompts_keep_the_crm_wording() {
        let email = system_prompt(Channel::Email);
        assert!(email.starts_with(
            "Your job is to improve customer-facing business messages at tone level 2"
        ));
        assert!(email.contains("You edit customer-facing business emails."));
        assert!(email.ends_with("replace it with \"Web-site\"."));
        assert!(system_prompt(Channel::Sms).contains("You edit SMS text messages."));
        assert_eq!(name_rule(Some("  ")), None);
        assert!(
            name_rule(Some("Alice Lead"))
                .unwrap()
                .starts_with("The customer's first name is \"Alice\".")
        );
    }
}
