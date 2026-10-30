//! `POST /v1/ai/call-note`: a CRM note from a call transcript. The page
//! skips greeting-only voicemail and tidies the note. Remix:
//! `api.aiSummarize.callNote` (prompts unchanged).

use crate::ai::{Caller, Chat, MINI_MODEL};
use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};

pub const VOICEMAIL_PROMPT: &str = r#"Summarize this outbound sales voicemail into a minimal CRM note for a countertop company.

Return plain text only. Use "- Label: details" bullets. Do not include Voicemail, Call type, or Customer bullets. Maximum 2 bullets.

Required:
- Action: One line combining (a) why the rep called and (b) any prior timing they mention from an older conversation — use a semicolon between them (~18 words total). Example: "Follow-up on inquiry ~5–6 months ago; prior note: ready in May". Never use a separate Previously or Project timing bullet.

Rules:
- Prior customer timing the rep recalls belongs inside Action after a semicolon, not its own bullet.
- Never imply the customer spoke on this voicemail unless they did.
- Never quote, copy, or paraphrase the transcript as prose — output only the bullet lines.
- No filler. Never include personal names."#;

/// Granite Phone's AI note of a voicemail a caller left for the company
/// (`VOICEMAIL_PROMPT` is for the rep's own voicemails). Its note is tidied
/// as a voicemail note: the `Action` bullet.
pub const INBOUND_VOICEMAIL_PROMPT: &str = r#"Summarize this voicemail a caller left for a countertop company into a minimal CRM note.

Return plain text only. Use one "- Label: details" bullet. Do not include Voicemail, Call type, or Customer bullets.

Required:
- Action: One line with what the caller wants and when or how they asked to be called back (~18 words). Example: "Wants a quote for a quartz kitchen; asked for a call back after 5 pm".

Rules:
- Only what the caller said; never invent a project, a time or a request.
- Never quote, copy, or paraphrase the transcript as prose — output only the bullet line.
- No filler. Never include personal names or phone numbers."#;

pub const CALL_PROMPT: &str = r#"Summarize this phone sales call into a CRM note for a countertop and stone fabrication company.

Return plain text only. Use one bullet per topic. Every bullet must use the format "- **Label:** details" with a bold label and a colon (e.g. "- **Project:** ...", "- **Sink:** ..."). The note must be complete—never end mid-sentence or mid-thought. Never repeat the same label twice—merge related facts into one bullet.

Include only job-relevant facts. Use plain, direct wording—do not rephrase into formal CRM language. Do not stack synonyms (e.g. avoid "provided by the customer, updated and retained"). Never include customer names, caller names, or other personal names. Do not add Customer or Call type bullets.

Omit any bullet when the topic was not discussed or the value would be unknown—never write "not specified", "unknown", "N/A", "no specific profile mentioned", or any placeholder saying something was not mentioned or not discussed.

Whenever the call states a calendar date, day (today, tomorrow, Monday), or clock time (3 pm, 6:00, this afternoon), include that exact wording in the relevant bullet—never drop or generalize it.

When mentioned, include these bullets using exactly these labels (skip any label with nothing concrete to say):
- Project: room/project type, unit or kitchen counts, stone or material type (granite, quartz, etc.), approximate dimensions, and any agreed install/measure phasing or cadence (e.g. 52 kitchens total, install 13 at a time every two weeks, two phases). Put all project scope and install-plan facts in this single Project bullet.
- Color: cabinet color and stone color options the customer is considering, plus look preferences (e.g. not too busy). Omit if no color discussion.
- Sink: single or double, color if stated, who provides it for the job, and where it is when stated (e.g. in L-shaped section). Plain words only—do not add retained, updated, upgraded, provided by, or similar unless the call clearly requires it for the quote. Never add negations or clarifications about where the sink is not (e.g. do not write "not in the island"). Omit sink renovation history or condition
- Backsplash: height and which walls
- Edge profile: flat, rounded, ogee, 1/4 bevel, 1/2 bevel, bullnose, etc.—omit this bullet entirely if edge profile was not discussed
- Location: full street address when the customer gave one (street number, street name, city, state); otherwise city or area only—omit this bullet entirely if no place was mentioned. Include Location at most once—never duplicate this bullet
- Budget: customer's stated budget or price target with the exact dollar amount when mentioned (e.g. "$8,000 for quartz and installation")
- Pricing: how this job will be priced when discussed for the customer's project (e.g. charged per square foot, not per slab). Omit generic company policy the customer did not engage with.
- Sample: customer wants to see a stone sample, and how they prefer to see it when stated (showroom, mailed sample, colleague pickup). Omit if sample viewing was not discussed.
- Estimate: on-site visit scheduling only—when the salesperson will go to the customer's location to measure or give an estimate, with an agreed day and clock time. Never use Estimate for phone callbacks, quote follow-up calls, material lead times, cabinet timelines, or days the rep will call the customer. Never use Estimate when the customer only asked for an on-site estimate but no appointment day or time was agreed. Never use Estimate for company process the salesperson explained to the customer—omit lead times, how far out you are, when to call to schedule template, measurement-to-install workflow, and advising the customer to call in advance. When the salesperson says "give me a call" or "call in advance", the customer should call the company—never write salesperson will call the customer. If an appointment was missed or rescheduled, state who missed it from the call—if the salesperson apologized, said they missed it, or called it their mistake, write salesperson missed [day/time] appointment, never customer missed. Example: salesperson missed Saturday 10:30 appointment; rescheduled for today at 5:30 pm
- Project timing: only what the customer said about their project status or when they might proceed (project on hold or resuming, waiting on cabinets or other trades, litigation, permits, ready to start, comparison shopping at other suppliers, expired quote needing refresh, material order lead times such as 6–12 weeks, months away, framing still in progress)—not on-site estimate appointments, not phone callback scheduling, and not company process the salesperson explained. If the customer is waiting on something, state that. If the customer will contact the company when ready, write that explicitly—do not rephrase as the customer agreeing to the rep's two-or-three-week callback window. When the customer says "call me [day]" or "you can call me", they want the salesperson to call them—never write customer will call for that agreement
- Tear-out: only if removing existing countertops was discussed. Always state who will remove them and the confirmed existing material type (e.g. company removes existing granite countertops, customer removes existing granite countertops). If the customer wants the company to do removal, write company removes existing [material] countertops. If the customer will remove themselves, write customer removes existing [material] countertops. For current top material, include only what the customer confirmed; if they do not know the material, write material unknown—never state a material type the rep guessed or asked about. Never put send photos, links, quotes, or callbacks in Tear-out.

Do not include plumbing unless the customer asked about it or raised it as part of their project. Omit plumbing when only the salesperson stated company policy. Omit company turnaround, template-to-install lead times, and other standard process the salesperson explained unless the customer tied it to their install plan—the rep already knows the process. Keep customer-stated project needs, materials, colors, pricing for this job, sample needs, timing, and install phasing.

Never include a Next steps discussed bullet or any task list (send link, send photos, quotes, callbacks, follow-ups, website, inventory, or agreed call-back dates). Notes are job facts only—tasks are handled separately as CRM activities. No filler such as "no details discussed"."#;

/// Body of `POST /v1/ai/call-note`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CallNoteInput {
    /// The call's transcript.
    pub transcript: String,
    /// It was a voicemail (a shorter note).
    #[serde(default)]
    pub voicemail: bool,
}

/// `POST /v1/ai/call-note`.
#[derive(Debug, Serialize, ToSchema)]
pub struct CallNote {
    /// "- **Label:** details" bullets.
    pub content: String,
}

/// Summarize a call
///
/// A CRM note of the job facts in a sales call (or a short voicemail note).
#[crate::openapi::path(
    post,
    path = "/v1/ai/call-note",
    operation_id = "summarize_call_note",
    tag = "ai",
    request_body = CallNoteInput,
    responses(
        (status = 200, description = "Note", body = CallNote),
        (status = 400, description = "Empty transcript", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 422, description = "Nothing work-related in the transcript", body = ErrorBody),
        (status = 502, description = "OpenAI failed", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn call_note(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<CallNoteInput>,
) -> ApiResult<axum::response::Response> {
    use axum::response::IntoResponse;
    let transcript = input.transcript.trim();
    if transcript.is_empty() {
        return Err(ApiError::BadRequest("Transcript is empty".into()));
    }
    let prompt = if input.voicemail {
        VOICEMAIL_PROMPT
    } else {
        CALL_PROMPT
    };
    let chat = Chat::new(MINI_MODEL)
        .message("system", prompt)
        .message("user", transcript)
        .max_tokens(input.voicemail.then_some(180));
    let caller = Caller {
        feature: "call_note_summary",
        user_id: Some(user.id),
        company_id: Some(user.company_id),
    };
    let note = state
        .ai
        .chat(&state.pool, caller, &chat)
        .await
        .map_err(|err| ApiError::Upstream(err.to_string()))?;
    let note = note.trim();
    if note.is_empty() {
        return Ok((
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            Json(ErrorBody {
                error: "No work-related content found in transcript".into(),
            }),
        )
            .into_response());
    }
    Ok(Json(CallNote {
        content: note.to_string(),
    })
    .into_response())
}
