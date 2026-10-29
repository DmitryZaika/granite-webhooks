//! The AI helper beside the 10DLC texting registration form: a chat that
//! answers questions about filling it in. It reads the form as it is on
//! screen (unsaved edits included) and the step shown, what still blocks
//! marking it ready ([`form::problems`]), the company (name, address,
//! website, numbers), where the registration is (Telnyx's and the carriers'
//! reasons, the AI check's changes) and the carriers' rules the AI check
//! applies ([`CHECKS`]). The conversation is the browser's: each question
//! sends it whole, and nothing is saved.

use super::tendlc::{
    self as form, DESCRIPTION_CHARS, LIST_CHARS, MESSAGE_CHARS, MESSAGE_FLOW_CHARS, NAME_CHARS,
    SAMPLES, TenDlcDraft,
};
use super::tendlc_check::{self as ai_check, CHECKS};
use super::tendlc_registry as registry;
use crate::ai::{AiError, Caller, Chat, LARGE_FALLBACK_MODEL, LARGE_MODEL};
use crate::openapi::ToSchema;
use crate::state::AppState;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::fmt::Write as _;
use std::time::Duration;

/// Messages of the conversation the model reads: the latest ones.
pub const MAX_TURNS: usize = 20;
/// Characters of one message the model reads.
pub const MAX_MESSAGE_CHARS: usize = 4_000;
/// One answer (the model reasons briefly first).
const AI_TIMEOUT: Duration = Duration::from_secs(120);
const REASONING_EFFORT: &str = "low";
const MAX_COMPLETION_TOKENS: u32 = 8_000;

/// Who wrote a message of the conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ChatRole {
    User,
    Assistant,
}

/// One message of the conversation.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ChatTurn {
    pub role: ChatRole,
    pub content: String,
}

/// Who is asking: the company's Manager filling it in, or a Granite
/// Manager superuser reviewing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Asker {
    Manager,
    Superuser,
}

/// The form's step on screen (`business`, `texting`, `review`), for people.
fn step_title(step: &str) -> &'static str {
    match step {
        "business" => "1. Business",
        "texting" => "2. How you text",
        "review" => "3. Review",
        _ => "unknown",
    }
}

/// The form's fields and the rules it checks before it can be marked ready.
fn form_rules() -> String {
    let (description_min, description_max) = DESCRIPTION_CHARS;
    let (flow_min, flow_max) = MESSAGE_FLOW_CHARS;
    let (message_min, message_max) = MESSAGE_CHARS;
    let (samples_min, samples_max) = SAMPLES;
    format!(
        "\
The form (field label = draft key; the rules it checks before **Mark as ready**):

Step 1. Business (\"Carriers check these against public records: use your exact legal details.\")
- Type of business = brand.entityType: Private company (LLC, Inc.) PRIVATE_PROFIT, Publicly traded company PUBLIC_PROFIT, Non-profit NON_PROFIT, Government GOVERNMENT.
- Industry = brand.vertical (countertop and stone shops: Construction and remodeling, CONSTRUCTION).
- Business name = brand.displayName: the name customers know them by (DBA); every text must name it. At most {NAME_CHARS} characters.
- Legal name = brand.companyName: exactly as the IRS has it for the EIN (CP 575 letter, or the 147C letter the IRS sends on request). A mismatch with the EIN is the most common reason the registry fails the business.
- Tax id (EIN) = brand.ein: 9 digits like 12-3456789; Canada: the 9-digit business number.
- Country of registration = brand.country: US or CA only.
- Contact: Business phone (US or Canada number), Business email, Website (starts with https://, must open and show the business name; not parked or under construction), Contact first/last name (optional).
- Business address: Street address, City, State or province (2 letters), ZIP or postal code; as on the tax records, no PO boxes.
- Publicly traded only: Stock symbol, Stock exchange, Business contact email.

Step 2. How you text
- The texts are = campaign.usecase. Several kinds (mixed) MIXED is the usual choice; Low volume, several kinds LOW_VOLUME is under 6,000 texts a month with a lower fee and lower limits; or a single kind (Customer care, Account updates, Delivery updates, Marketing, Surveys, Sign-in codes, Security alerts, Fraud alerts, Public service announcements, Higher education).
- Kinds of texts = campaign.subUsecases: mixed picks 2 to 5, low volume 1 to 5 (for example Customer care, Account updates, Marketing).
- What you text and why = campaign.description: {description_min} to {description_max} characters. Who sends (the business name), to whom (customers who agreed) and what about, matching the kinds of texts and the samples.
- How customers opt in = campaign.messageFlow: {flow_min} to {flow_max} characters. The quote form's exact address, that customers tick an optional checkbox (not pre-checked) to agree to texts, the opt-in words quoted, every other way they agree (phone, showroom) and how that is recorded, and the privacy policy and terms links.
- Sample texts = campaign.samples: {samples_min} to {samples_max}, each {message_min} to {message_max} characters, each names the business, says how to opt out (\"Reply STOP to opt out.\"), different from the others, full links only (no bit.ly or other shorteners). Together they show every chosen kind of text. \"Use examples\" fills three with the business name.
- Replies to keywords, each {message_min} to {message_max} characters and naming the business: Answer to HELP (how to reach the business: phone, email or website, and STOP to opt out) with Help keywords (must include HELP); Answer to opting in (what texts they get, message frequency varies, Msg & data rates may apply, Reply HELP for help, Reply STOP to opt out) with Opt-in keywords; Answer to STOP (confirms they are unsubscribed and get no more texts) with Opt-out keywords (must include STOP). Keywords are single words of letters and digits separated by commas, at most {LIST_CHARS} characters.
- About the texts: The texts contain links (required when a sample has a link; optional Example link), The texts contain phone numbers (required when a sample has a phone number), We text from a pool of 50 or more numbers (most companies do not), age-restricted products, loans or direct lending (leave unticked for a countertop shop).
- Your website pages, each a full https:// address: Quote form (the page where customers give their phone number and agree to texts), Privacy policy, Texting terms. Carriers open them; the AI check reads them when the form is marked ready.
- Carrier rules: the checkbox \"We text only people who agreed to get texts from us, stop right away when they reply STOP, and follow the carriers' rules for business texting.\"

Step 3. Review: a summary and \"Still to do\", every missing or wrong field with \"Fix it\".

Buttons: the company's Managers click \"Save draft\" and \"Mark as ready\"; marking it ready starts an AI check of the form and the three pages (result by email in minutes: changes to make, or it goes to the Granite Manager team). The team sends it to Telnyx (\"Send to Telnyx\" charges for the brand and the campaign). Once the carriers have the campaign only the sample texts, How customers opt in, the Answer to HELP and the quote form link can change."
    )
}

/// The carriers' rules the AI check applies, one per line.
fn carrier_rules() -> String {
    CHECKS.iter().fold(String::new(), |mut text, check| {
        let _ = writeln!(text, "- {}: {}", check.title, check.rule);
        text
    })
}

fn system_prompt(asker: Asker) -> String {
    let who = match asker {
        Asker::Manager => "a Manager of the company, who fills the form in and marks it ready",
        Asker::Superuser => {
            "a Granite Manager team member (superuser) who reviews the company's form, may edit it and sends it to Telnyx"
        }
    };
    format!(
        "\
You are the helper beside the 10DLC texting registration form in Granite Manager, a CRM for countertop and stone shops. US carriers block business texts from local numbers until the business (the brand) and how it texts (the campaign) are registered; Granite Manager sends this form to Telnyx, which registers it with The Campaign Registry and the carriers. You are talking with {who}.

You see what they see: the form as it is on their screen right now (unsaved edits included), the step they are on, what still blocks marking it ready, the company's details in Granite Manager and where the registration is. Use them: answer about their form and their company, not in general.

How to answer:
- Short and practical: a few sentences or a short list. Name fields exactly as the form labels them, with their step.
- When you suggest text for a field, give the complete text to paste, ready to use: within the field's limits, with the business name, and following the rules below so it passes the form's check and the carriers' review.
- When asked what to fix, start with what blocks marking it ready, then the AI check's changes and the carriers' reasons, then anything in the rules below the form does not check (the carriers do).
- Never make up facts that must match official records: the EIN, the legal name, the business address. When the company's details below don't give them, say where to find them (EIN and legal name: the IRS CP 575 or 147C letter, or the business tax return; the address as on those records).
- For the quote form, privacy policy and terms pages, say what each page must show and give wording they can add to their website. You cannot open the pages: the AI check reads them when the form is marked ready.
- You cannot change, save or send the form: the user types or pastes your suggestions.
- Answer in the language of the question. Questions that are not about this registration or business texting: say in one sentence that you help with this form.

{rules}
What the carriers' reviewers check (the AI check applies the same rules):
{carriers}",
        rules = form_rules(),
        carriers = carrier_rules(),
    )
}

/// The company in Granite Manager and where its registration is.
async fn company_context(pool: &MySqlPool, company_id: i32) -> sqlx::Result<String> {
    let company: Option<(String, Option<String>, Option<String>)> =
        sqlx::query_as("SELECT name, address, domain FROM company WHERE id = ?")
            .bind(company_id)
            .fetch_optional(pool)
            .await?;
    let (name, address, domain) = company.unwrap_or_default();
    let shown = |value: Option<String>| {
        value
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "not set".into())
    };
    let mut text = format!(
        "The company in Granite Manager:\n- Name: {name}\n- Address: {}\n- Email domain (website): {}\n",
        shown(address),
        shown(domain),
    );
    let numbers = registry::numbers(pool, company_id).await?;
    if numbers.is_empty() {
        text.push_str("- Phone numbers (Granite Phone): none yet\n");
    } else {
        text.push_str(
            "- Phone numbers (Granite Phone) that will text once the campaign is approved:\n",
        );
        for number in &numbers {
            let label = number.label.trim();
            let _ = writeln!(
                text,
                "  - {}{}",
                number.e164,
                if label.is_empty() {
                    String::new()
                } else {
                    format!(" ({label})")
                }
            );
        }
    }

    let Some(row) = form::load(pool, company_id).await? else {
        text.push_str(
            "\nThe registration: never saved. The form on screen is the suggested first draft Granite Manager made from the company's details: its EIN and address are empty, and its legal name is only the company's name in Granite Manager.\n",
        );
        return Ok(text);
    };
    let _ = writeln!(
        text,
        "\nThe registration: status {} (draft, ready = waiting for the Granite Manager team, sent = the registry checks the business, campaign_pending = the carriers review the campaign, approved, rejected = needs changes, suspended).",
        row.status.as_str()
    );
    if row.campaign_locked() {
        text.push_str("The carriers have the campaign: only the sample texts, How customers opt in, the Answer to HELP and the quote form link can change.\n");
    }
    for (about, feedback) in [
        ("business", row.brand_feedback()),
        ("texting", row.campaign_feedback()),
    ] {
        for item in feedback {
            let _ = writeln!(
                text,
                "- The carriers said about the {about}: {}: {}",
                item.title, item.description
            );
        }
    }
    if let Some(check) = ai_check::view(&row) {
        let _ = writeln!(text, "The AI check: {}.", check.status.as_str());
        if let Some(summary) = check.summary.filter(|summary| !summary.trim().is_empty()) {
            let _ = writeln!(text, "Its summary: {summary}");
        }
        for fix in check.fixes {
            let _ = writeln!(
                text,
                "- Change {}{}: {} Now: \"{}\". Use: \"{}\"",
                fix.label,
                fix.link
                    .map(|link| format!(" ({link})"))
                    .unwrap_or_default(),
                fix.problem,
                fix.current,
                fix.replacement
            );
        }
    }
    Ok(text)
}

/// The form on screen, its step and what blocks marking it ready.
fn screen_context(draft: &TenDlcDraft, step: &str) -> String {
    let form = serde_json::to_string_pretty(draft).unwrap_or_default();
    let problems = form::problems(draft);
    let mut text = format!(
        "The form on the user's screen now (step {}):\n```json\n{form}\n```\n",
        step_title(step)
    );
    if problems.is_empty() {
        text.push_str(
            "Nothing blocks marking it ready: \"Everything the carriers need is filled in.\"\n",
        );
    } else {
        text.push_str("What still blocks marking it ready (the form's \"Still to do\"):\n");
        for problem in problems {
            let _ = writeln!(text, "- {}: {}", problem.field, problem.message);
        }
    }
    text
}

/// The model's messages: the rules, the company, the screen, then the
/// latest of the conversation (each message clipped).
pub async fn messages(
    pool: &MySqlPool,
    company_id: i32,
    asker: Asker,
    draft: &TenDlcDraft,
    step: &str,
    turns: &[ChatTurn],
) -> sqlx::Result<Vec<Value>> {
    let context = format!(
        "{}\n{}",
        company_context(pool, company_id).await?,
        screen_context(draft, step)
    );
    let mut messages = vec![
        json!({ "role": "system", "content": system_prompt(asker) }),
        json!({ "role": "system", "content": context }),
    ];
    let start = turns.len().saturating_sub(MAX_TURNS);
    for turn in &turns[start..] {
        let role = match turn.role {
            ChatRole::User => "user",
            ChatRole::Assistant => "assistant",
        };
        let content: String = turn.content.chars().take(MAX_MESSAGE_CHARS).collect();
        messages.push(json!({ "role": role, "content": content }));
    }
    Ok(messages)
}

/// Streams the answer to `on_text`; the whole answer is returned. Falls
/// back to Terra when the key may not use Sol.
pub async fn answer(
    state: &AppState,
    caller: Caller<'_>,
    messages: Vec<Value>,
    mut on_text: impl FnMut(&str) + Send,
) -> Result<String, AiError> {
    let mut chat = Chat::new(LARGE_MODEL);
    chat.messages = messages;
    chat.reasoning_effort = Some(REASONING_EFFORT);
    chat.max_completion_tokens = Some(MAX_COMPLETION_TOKENS);
    chat.timeout = AI_TIMEOUT;
    match state
        .ai
        .chat_stream(&state.pool, caller, &chat, &mut on_text)
        .await
    {
        Err(err) if err.is_model_permission() => {
            chat.model = LARGE_FALLBACK_MODEL;
            state
                .ai
                .chat_stream(&state.pool, caller, &chat, &mut on_text)
                .await
        }
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prompt_has_every_carrier_rule() {
        let prompt = system_prompt(Asker::Manager);
        for check in CHECKS {
            assert!(prompt.contains(check.rule), "{} missing", check.id);
        }
        assert!(prompt.contains("a Manager of the company"));
        assert!(system_prompt(Asker::Superuser).contains("superuser"));
    }

    #[test]
    fn the_screen_lists_what_blocks_marking_it_ready() {
        let text = screen_context(&TenDlcDraft::default(), "business");
        assert!(text.contains("step 1. Business"));
        assert!(text.contains("brand.ein: Enter the tax id (EIN)."));
    }
}
