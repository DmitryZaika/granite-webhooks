//! `POST /v1/ai/call-activities`: CRM follow-up activities from a call.
//!
//! Remix: `api.aiSummarize.callActivity` (prompts unchanged; the rules that
//! clean up the AI's answer are in [`super::call_activity_rules`]).

use super::call_activity_rules::{
    CallActivity, DEFAULT_ACTIVITY_NAME, VOICEMAIL_DEFAULT_ACTIVITY_NAME, finalize, from_response,
    voicemail_follow_up_deadline,
};
use crate::ai::{Caller, Chat, MINI_MODEL};
use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::state::AppState;
use crate::voicemail::{greeting_only, sounds_like_voicemail};
use axum::Json;
use axum::extract::State;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A voicemail this short gets the default activity without asking the AI.
const VOICEMAIL_SKIP_AI_WORDS: usize = 50;
/// The CRM's `VOICEMAIL_GREETING_ACTIVITY_NAME`.
const VOICEMAIL_GREETING_ACTIVITY_NAME: &str = "No response, follow-up";

const VOICEMAIL_PROMPT: &str = r#"Extract follow-up tasks from a sales voicemail for a countertop company CRM.

Return JSON only: {"activities": [{"name": string, "deadline": string | null}, ...]}

Focus on what the salesperson asked the customer to do next (confirm interest, call back, send photos). Usually one activity such as "Follow-up to confirm interest".

Rules for deadline: null unless the rep gives a follow-up due date for this task (today or tomorrow relative to activity creation). Never use project timing (May, months ago, ready in) as the deadline.

No commitments -> {"activities":[]}

"#;

const LIVE_CALL_PROMPT: &str = r#"Extract follow-up tasks from a phone call for a CRM activity list. The note records job facts only; every commitment from the call belongs here as activities.

Return JSON only: {"activities": [{"name": string, "deadline": string | null}, ...]}

Create a separate activity for each distinct commitment (different action or different due date). Never merge unrelated tasks into one activity.

Include when stated:
- Salesperson will send something (inventory link, website, granite/stone options, quote, edge photos, samples)—only when the rep committed and the customer did not choose a different path (e.g. if the customer will send their own color or kitchen photos for matching, do not create a send-inventory-link activity for a mere offer the customer declined)
- Customer will send something—create separate activities when commitments differ: kitchen layout photos ("Request kitchen photos") and preferred stone or color photos when they have a specific material in mind ("Request picture of preferred stone")
- Salesperson offers or agrees to call back (including "call you in two or three weeks")
- On-site estimate when the rep will go to the customer to measure or quote (not when only scheduling is still being negotiated)
- Other follow-up the rep committed to
- Do not create an outbound call-back activity when only the customer said they will call the company later
- When edge style was discussed and the rep offers to send edge photos, add a separate edge-photo activity—do not merge with link or customer-photo tasks

Rules for name:
- Clear imperative title, usually 4-14 words with brief context (why you are calling back, e.g. after vacation, after trip)
- Never include customer or caller names
- Never include calendar dates in the name—use deadline only for when the task is due
- When the rep offers "call you in two weeks" and the customer confirms a different number (e.g. "in three weeks, yeah"), use the customer's confirmed number of weeks for the deadline
- For "two or three weeks" call-back from the salesperson with no customer pick: set deadline null; server uses about two weeks
- For an agreed call-back in N weeks (a week, in a week, about a week, one week, two weeks, three weeks, etc.): set deadline null; server computes the date from the transcript
- This company sells stone countertops. When sending stone options, samples, links, or pricing, include the stone type if specified (granite, quartz, marble, etc.)
- When preparing or sending an estimate or quote, include the customer's stated budget amount in the activity name if they gave one (e.g. "Prepare and send estimate under $8,000")
- Examples: "Send granite inventory link", "Request kitchen photos", "Request picture of preferred stone", "Send edge profile photos", "Prepare and send estimate under $8,000", "Call back, follow up after vacation", "Call back, follow up on countertop project after vacation to Colombia"
- When the rep already agreed to be on-site at a specific time for an estimate, use name "Be on-site for estimate"—not "Schedule on-site measurement visit" or "Schedule on-site estimate visit" (server rewrites the name to include the agreed day and time, e.g. "Be on-site for estimate at Wed 9:30 AM")
- Only use "Schedule ... estimate" when an estimate time is not yet agreed

Rules for deadline (prefer setting a deadline when timing was discussed; null only when no timing was discussed for that task):
- Always set a deadline when the call gives a date or time for that task—never omit timing that was stated
- YYYY-MM-DD when only a calendar day was committed (no specific clock time)—use the resolved calendar day, not vague wording
- ISO 8601 UTC datetime with Z suffix when a specific clock time was committed (e.g. at 3 pm, 6:00, this afternoon)
- Resolve relative phrases using the call date below: today, tomorrow, next Monday, next Friday, this afternoon, end of day
- When the customer or rep agrees to call back on a named day (especially "next Friday", "call me next Friday"), set deadline to that calendar day (YYYY-MM-DD)—never today or the activity creation time unless they explicitly said today
- If the rep offers "Friday" and the customer says "next Friday", use the later Friday (the week after the nearest Friday), not today
- Quote the call literally when choosing the day: if they said tomorrow, the deadline day must be tomorrow relative to the call date
- "Right away", "now", "immediately", "ASAP", "shortly", "a little later", "a little bit later" when sending a link or options → omit deadline; server sets 30 minutes from activity creation
- "I'll send the link today" (or similar send-today, no time) → YYYY-MM-DD for the call day only, never a datetime
- "Tomorrow" only when that party said tomorrow for that task—customer sending kitchen photos tomorrow → Request kitchen photos due tomorrow; rep sending inventory, address, or sink photos during the call with no tomorrow stated → due today (call day), never tomorrow just because the customer mentioned tomorrow for something else
- Quote due after customer sends measurements tomorrow → due tomorrow, not today
- Quote or estimate with "by tomorrow" or before the store closes → omit deadline; server sets due today at 4:00 PM store time (two hours before 6:00 PM close), but never earlier than one hour after activity creation
- Salesperson: "Would you like me to call you in two or three weeks?" while customer is in litigation → {"name":"Call back, check if litigation is finished","deadline":null}
- Rep and customer agree rep will be on-site today at 5:30 pm for an estimate → {"name":"Be on-site for estimate","deadline":null} (server sets activity name with the appointment time; reminder due same day at 9:00 AM store time because the visit is after noon)
- Rep and customer agree on Wednesday at 9:30 am → {"name":"Be on-site for estimate","deadline":null} (server sets name with Wed 9:30 AM; reminder due the previous day at 5:00 PM store time)
- Rep and customer agree on Saturday with no specific time → {"name":"Be on-site for estimate","deadline":null} (server sets name with the day only; reminder due 9:00 AM store time on that day)
- Rep offers to call Friday; customer says "call me next Friday" and rep agrees → {"name":"Call back, follow up on quote","deadline":"YYYY-MM-DD for next Friday after call date"}
- Customer: "can you call me back in a couple hours? We'll do at 2 p.m." and rep agrees → {"name":"Call back, follow up on kitchen project","deadline":null} (server sets due 2:00 PM store time on the call day)

Examples:
- Salesperson discusses square edges, then says "I can also send you pictures"
  -> include {"name":"Send square edge profile photos","deadline":null} in addition to any link or customer-photo tasks
- Salesperson: "I'll send the inventory link today" AND customer: "I'll send you pictures tomorrow"
  -> {"activities":[
    {"name":"Send granite inventory link","deadline":"YYYY-MM-DD call day"},
    {"name":"Request kitchen photos","deadline":"YYYY-MM-DD day after call"}
  ]}
- Customer has exact quartz color from Menards and will send color photo plus kitchen layout
  -> {"activities":[
    {"name":"Request kitchen photos","deadline":"YYYY-MM-DD call day"},
    {"name":"Request picture of preferred stone","deadline":"YYYY-MM-DD call day"},
    {"name":"Prepare and send estimate under $8,000","deadline":"YYYY-MM-DD call day"}
  ]}
- Salesperson: "I'll send you the link a little bit later"
  -> {"name":"Send granite inventory link","deadline":null}
- Salesperson: "I'll get you a quote by tomorrow"
  -> {"name":"Send quote","deadline":null}
- Only one vague follow-up with no timing -> single activity with deadline null
- No commitments -> {"activities":[]}

"#;

/// Body of `POST /v1/ai/call-activities`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CallActivitiesInput {
    /// The call's transcript.
    pub transcript: String,
    /// When the call started (ISO 8601); relative deadlines count from it.
    #[serde(default)]
    pub call_started_at: Option<String>,
    /// When the activities are being created (ISO 8601); now when absent.
    #[serde(default)]
    pub committed_at: Option<String>,
    /// The provider said it was a voicemail.
    #[serde(default)]
    pub is_voicemail: bool,
}

/// `POST /v1/ai/call-activities`.
#[derive(Debug, Serialize, ToSchema)]
pub struct CallActivities {
    /// Activities to add to the deal.
    pub activities: Vec<CallActivity>,
}

fn parse_instant(value: Option<&str>) -> Option<DateTime<Utc>> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(super::call_activity_rules::parse_js_date)
}

/// Extract activities from a call
///
/// The follow-up tasks of a call (call backs, links and quotes to send,
/// photos to wait for, on-site estimates) with their deadlines, read by AI
/// and checked against the transcript.
#[crate::openapi::path(
    post,
    path = "/v1/ai/call-activities",
    operation_id = "extract_call_activities",
    tag = "ai",
    request_body = CallActivitiesInput,
    responses(
        (status = 200, description = "Activities", body = CallActivities),
        (status = 400, description = "Empty transcript", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 502, description = "OpenAI failed or answered with something else", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn call_activities(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<CallActivitiesInput>,
) -> ApiResult<Json<CallActivities>> {
    let transcript = input.transcript.trim();
    if transcript.is_empty() {
        return Err(ApiError::BadRequest("Transcript is empty".into()));
    }
    let now = Utc::now();
    let committed = parse_instant(input.committed_at.as_deref()).unwrap_or(now);
    let started = parse_instant(input.call_started_at.as_deref()).unwrap_or(now);
    let voicemail = input.is_voicemail || sounds_like_voicemail(transcript);
    let reference = if voicemail {
        format!(
            "Activity creation time for relative deadlines (ISO): {}",
            super::call_activity_rules::iso(committed)
        )
    } else {
        let at = input
            .call_started_at
            .as_deref()
            .map(str::trim)
            .filter(|at| !at.is_empty())
            .map_or_else(|| super::call_activity_rules::iso(started), str::to_string);
        format!("Call date/time for relative deadlines (ISO): {at}")
    };
    if voicemail && greeting_only(transcript) {
        let only = CallActivity {
            name: VOICEMAIL_GREETING_ACTIVITY_NAME.into(),
            deadline: None,
        };
        return Ok(Json(CallActivities {
            activities: finalize(vec![only], committed, transcript),
        }));
    }
    if voicemail && transcript.split_whitespace().count() <= VOICEMAIL_SKIP_AI_WORDS {
        let only = CallActivity {
            name: VOICEMAIL_DEFAULT_ACTIVITY_NAME.into(),
            deadline: None,
        };
        return Ok(Json(CallActivities {
            activities: finalize(vec![only], committed, transcript),
        }));
    }
    let prompt = format!(
        "{}{reference}",
        if voicemail {
            VOICEMAIL_PROMPT
        } else {
            LIVE_CALL_PROMPT
        }
    );
    let mut chat = Chat::new(MINI_MODEL)
        .message("system", &prompt)
        .message("user", transcript)
        .max_tokens(voicemail.then_some(250));
    chat.json = true;
    let caller = Caller {
        feature: "call_activity_extract",
        user_id: Some(user.id),
        company_id: Some(user.company_id),
    };
    let reply = state
        .ai
        .chat(&state.pool, caller, &chat)
        .await
        .map_err(|err| ApiError::Upstream(err.to_string()))?;
    let json: Value = serde_json::from_str(if reply.trim().is_empty() {
        "{}"
    } else {
        &reply
    })
    .map_err(|_| ApiError::Upstream("Failed to parse activity response".into()))?;
    let mut activities = from_response(&json, committed, transcript);
    if voicemail {
        if let [only] = activities.as_mut_slice()
            && only.name == DEFAULT_ACTIVITY_NAME
        {
            only.name = VOICEMAIL_DEFAULT_ACTIVITY_NAME.into();
        }
        for activity in &mut activities {
            activity.deadline = voicemail_follow_up_deadline(
                activity.deadline.as_deref(),
                &activity.name,
                committed,
            );
        }
    }
    Ok(Json(CallActivities { activities }))
}
