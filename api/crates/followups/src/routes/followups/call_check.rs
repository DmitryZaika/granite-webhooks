//! `POST /v1/followups/call-check`: did the customer talk on a rep's call.
//!
//! After an outbound call, whether the customer talked back. A long call counts at once; a shorter one is
//! transcribed and must show turn-taking (not a mailbox greeting). A
//! conversation stops the customer's running flows, and the caller (the
//! `webhooks` Lambda, by its key) moves the deal to Contacted. Port of the
//! CRM's `api.sms-followups.call-check` (`smsFlowCallCheck.server.ts`).

use crate::ai::Caller;
use crate::auth::job::JobCaller;
use crate::error::{ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::followups::enrollments::stop_after_customer_conversation;
use crate::followups::processor::company_sms_provider;
use crate::openapi::ToSchema;
use crate::routes::calls::recording::{CallTranscriptError, transcribe_call};
use crate::routes::sms::provider::SmsProvider;
use crate::state::AppState;
use crate::transcribe::{Transcript, Utterance};
use axum::Json;
use axum::extract::State;
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

/// Shorter calls are not checked.
const MIN_TALKING_SECONDS: i64 = 30;
/// Longer calls are a conversation without a transcript.
const CONVERSATION_TALKING_SECONDS: i64 = 90;
/// A voicemail is greeting → rep (one change); a conversation goes back and forth.
const MIN_SPEAKER_CHANGES: usize = 3;
const MIN_WORDS_PER_SPEAKER: usize = 5;
/// The provider may still be writing the recording when the call ends.
const RECORDING_ATTEMPTS: usize = 4;
const RECORDING_RETRY_DELAY: Duration = Duration::from_millis(2_500);

/// The wait between two attempts: [`RECORDING_RETRY_DELAY`] unless set with
/// `AppState::with_extension` (tests shorten it).
#[derive(Clone, Copy, Debug)]
pub struct RecordingRetryDelay(pub Duration);

impl Default for RecordingRetryDelay {
    fn default() -> Self {
        Self(RECORDING_RETRY_DELAY)
    }
}

/// The customer's phone, as digits or a number.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum PhoneDigits {
    Number(u64),
    Text(String),
}

impl PhoneDigits {
    fn digits(&self) -> String {
        match self {
            Self::Number(n) => n.to_string(),
            Self::Text(text) => text.chars().filter(char::is_ascii_digit).collect(),
        }
    }
}

/// A provider's call id: `CloudTalk`'s or `RingCentral`'s number, or a
/// Granite Phone call `tx_<id>`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum CallId {
    Number(i64),
    Text(String),
}

impl CallId {
    fn text(&self) -> String {
        match self {
            Self::Number(n) => n.to_string(),
            Self::Text(text) => text.trim().to_string(),
        }
    }

    /// A Granite Phone call: its transcript is ours (no waiting for the
    /// provider) and the phone job gives it its call points.
    fn is_telnyx(&self) -> bool {
        matches!(self, Self::Text(text) if crate::routes::calls::telnyx::parse_token(text).is_some())
    }
}

/// A finished outbound call.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CallCheck {
    /// The rep's company.
    pub company_id: i32,
    /// The customer's phone.
    pub phone_digits: PhoneDigits,
    /// The provider's call id: a number, or a Granite Phone call `tx_<id>`
    /// (whose kept transcript is checked).
    pub call_id: CallId,
    /// Seconds of talk.
    pub talking_time: i64,
    /// The provider says the call reached voicemail.
    #[serde(default)]
    pub is_voicemail: bool,
    /// Where the provider keeps the recording.
    pub recording_link: Option<String>,
}

/// What the check found.
#[derive(Debug, Serialize, ToSchema)]
pub struct CallCheckResult {
    /// No conversation was found (`talking_time_too_short`,
    /// `transcription_unavailable`, `voicemail`).
    pub skipped: bool,
    /// Why: one of the above, `long_call` or `customer_conversation`.
    pub reason: &'static str,
    /// Flow runs stopped.
    pub cancelled: u64,
    /// The customer talked: the deal moves to Contacted.
    pub customer_responded: bool,
    /// The start of the transcript, when one was made.
    pub transcript_preview: Option<String>,
}

fn back_and_forth(utterances: &[Utterance]) -> bool {
    let changes = utterances
        .windows(2)
        .filter(|pair| pair[0].speaker != pair[1].speaker)
        .count();
    let mut words: HashMap<&str, usize> = HashMap::new();
    for utterance in utterances {
        *words.entry(utterance.speaker.as_str()).or_default() +=
            utterance.text.split_whitespace().count();
    }
    let talkative = words
        .values()
        .filter(|count| **count >= MIN_WORDS_PER_SPEAKER)
        .count();
    changes >= MIN_SPEAKER_CHANGES && talkative >= 2
}

/// The customer talked back on the call (`isCustomerConversation`).
pub fn is_customer_conversation(transcript: &Transcript, provider_says_voicemail: bool) -> bool {
    if provider_says_voicemail {
        return false;
    }
    match transcript.utterances.as_deref() {
        // No speaker turns (e.g. RingCentral's own voicemail text): the phrases only.
        None | Some([]) => !crate::voicemail::sounds_like_voicemail(&transcript.text),
        // Turn-taking beats phrases: "I'm not available Tuesday" is real talk.
        Some(utterances) => back_and_forth(utterances),
    }
}

async fn transcribe_with_retry(
    state: &AppState,
    check: &CallCheck,
) -> Result<Transcript, CallTranscriptError> {
    let call_id = check.call_id.text();
    // Our own recordings are there or not: no waiting.
    let attempts = if check.call_id.is_telnyx() {
        1
    } else {
        RECORDING_ATTEMPTS
    };
    let mut attempt = 0;
    loop {
        let caller = Caller {
            feature: "sms_followup_call_check",
            user_id: None,
            company_id: Some(check.company_id),
        };
        match transcribe_call(
            state,
            caller,
            &call_id,
            check.recording_link.as_deref(),
            true,
        )
        .await
        {
            Ok(transcript) => return Ok(transcript),
            Err(
                err @ (CallTranscriptError::NotAvailable
                | CallTranscriptError::FetchFailed
                | CallTranscriptError::Empty
                | CallTranscriptError::Provider(_)),
            ) if attempt + 1 < attempts => {
                tracing::info!(%err, attempt, "call recording not ready, retrying");
                attempt += 1;
                let delay = state.extensions.get_or_default::<RecordingRetryDelay>().0;
                tokio::time::sleep(delay).await;
            }
            Err(err) => return Err(err),
        }
    }
}

/// Saves the outcome the call points classifier would reach, so it never
/// transcribes the call again (`CloudTalk` companies only; `RingCentral`
/// calls are classified by the call points job). A failure only logs.
async fn record_call_points(
    state: &AppState,
    check: &CallCheck,
    transcript: &str,
    voicemail: bool,
) {
    // Granite Phone calls get their points from the phone job.
    if check.call_id.is_telnyx() {
        return;
    }
    let result = async {
        if company_sms_provider(state, check.company_id).await? != Some(SmsProvider::Cloudtalk) {
            return Ok(());
        }
        let digits = check.phone_digits.digits();
        let digits = if digits.len() > 10 { digits[digits.len() - 10..].to_string() } else { digits };
        sqlx::query(
            "INSERT INTO phone_call_points
               (company_id, provider, provider_call_id, talk_seconds, customer_digits, outcome, classified_at)
             VALUES (?, 'cloudtalk', ?, ?, ?, ?, UTC_TIMESTAMP()) AS new
             ON DUPLICATE KEY UPDATE
               talk_seconds = new.talk_seconds,
               customer_digits = new.customer_digits,
               classified_at = IF(phone_call_points.outcome = 'pending', new.classified_at, phone_call_points.classified_at),
               outcome = IF(phone_call_points.outcome = 'pending', new.outcome, phone_call_points.outcome)",
        )
        .bind(check.company_id)
        .bind(check.call_id.text())
        .bind(check.talking_time)
        .bind(digits)
        .bind(crate::voicemail::classify(voicemail, transcript))
        .execute(&state.pool)
        .await
        .map(|_| ())
    }
    .await;
    if let Err(err) = result {
        tracing::error!(
            ?err,
            call_id = %check.call_id.text(),
            "call points classification not saved"
        );
    }
}

fn preview(text: &str) -> Option<String> {
    Some(text.encode_utf16().take(120).collect::<Vec<_>>())
        .map(|units| String::from_utf16_lossy(&units))
}

/// Check an outbound call
///
/// Whether the customer talked back on a rep's call. A conversation stops
/// the customer's running SMS / email flows. `call_id` may be a Granite
/// Phone call `tx_<id>` (the transcript kept on it is checked); the API runs
/// those checks itself.
#[crate::openapi::path(
    post,
    path = "/v1/followups/call-check",
    operation_id = "check_followup_call",
    tag = "followups",
    request_body = CallCheck,
    responses(
        (status = 200, description = "What the check found", body = CallCheckResult),
        (status = 400, description = "Invalid body", body = ErrorBody),
        (status = 401, description = "Missing or wrong key", body = ErrorBody),
    ),
    security(("job_key" = []))
)]
pub async fn call_check(
    State(state): State<AppState>,
    _caller: JobCaller,
    JsonBody(check): JsonBody<CallCheck>,
) -> ApiResult<Json<CallCheckResult>> {
    Ok(Json(check_call(&state, &check).await?))
}

/// The check of `POST /v1/followups/call-check`, for any caller (the phone
/// job checks Granite Phone calls in-process).
pub async fn check_call(state: &AppState, check: &CallCheck) -> sqlx::Result<CallCheckResult> {
    let skipped = |reason, transcript_preview| CallCheckResult {
        skipped: true,
        reason,
        cancelled: 0,
        customer_responded: false,
        transcript_preview,
    };
    if check.talking_time <= MIN_TALKING_SECONDS {
        return Ok(skipped("talking_time_too_short", None));
    }
    let phone = check.phone_digits.digits();
    if check.talking_time > CONVERSATION_TALKING_SECONDS {
        let cancelled = stop_after_customer_conversation(
            &state.pool,
            SmsProvider::Cloudtalk,
            check.company_id,
            &phone,
        )
        .await?;
        return Ok(CallCheckResult {
            skipped: false,
            reason: "long_call",
            cancelled,
            customer_responded: true,
            transcript_preview: None,
        });
    }
    let Ok(transcript) = transcribe_with_retry(state, check).await else {
        return Ok(skipped("transcription_unavailable", None));
    };
    // Points keep the text-only classifier; the decision below uses turns.
    let voicemail = check.is_voicemail || crate::voicemail::sounds_like_voicemail(&transcript.text);
    record_call_points(state, check, &transcript.text, voicemail).await;
    if !is_customer_conversation(&transcript, check.is_voicemail) {
        return Ok(skipped("voicemail", preview(&transcript.text)));
    }
    let cancelled = stop_after_customer_conversation(
        &state.pool,
        SmsProvider::Cloudtalk,
        check.company_id,
        &phone,
    )
    .await?;
    Ok(CallCheckResult {
        skipped: false,
        reason: "customer_conversation",
        cancelled,
        customer_responded: true,
        transcript_preview: preview(&transcript.text),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turns(turns: &[(&str, &str)]) -> Transcript {
        Transcript {
            text: turns
                .iter()
                .map(|(_, text)| *text)
                .collect::<Vec<_>>()
                .join(" "),
            utterances: Some(
                turns
                    .iter()
                    .map(|(speaker, text)| Utterance {
                        speaker: (*speaker).into(),
                        text: (*text).into(),
                    })
                    .collect(),
            ),
            audio_seconds: None,
        }
    }

    #[test]
    fn turn_taking_is_a_conversation_and_a_greeting_is_not() {
        let talk = turns(&[
            ("A", "Hello this is Rita from Acme Stone"),
            ("B", "Oh hi, I am not available Tuesday though, sorry"),
            ("A", "No problem, how about Thursday morning then"),
            ("B", "Thursday works great for me, thank you so much"),
        ]);
        assert!(is_customer_conversation(&talk, false));
        assert!(!is_customer_conversation(&talk, true));

        let greeting = turns(&[
            (
                "A",
                "You have reached the voicemail of John, please leave a message",
            ),
            (
                "B",
                "Hi John this is Rita from Acme Stone calling about your quote",
            ),
        ]);
        assert!(!is_customer_conversation(&greeting, false));

        let plain = Transcript {
            text: "Please leave a message after the tone".into(),
            utterances: None,
            audio_seconds: None,
        };
        assert!(!is_customer_conversation(&plain, false));
        let plain = Transcript {
            text: "Sure, send me the quote".into(),
            utterances: None,
            audio_seconds: None,
        };
        assert!(is_customer_conversation(&plain, false));
    }

    #[test]
    fn previews_are_cut_like_the_crm() {
        assert_eq!(preview(&"é".repeat(200)).unwrap().chars().count(), 120);
    }
}
