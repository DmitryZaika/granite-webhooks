//! Call audio and its transcript.
//!
//! `CloudTalk` call ids are numeric; `RingCentral` audio is addressed by the media tokens of
//! [`super::ringcentral`] (`rec_<id>`, `vm_<extension>_<message>_<attachment>`).
//! Port of `callTranscription.server.ts` and `ringcentralCallMedia.ts`.

use super::ringcentral::attachment_of;
use crate::ai::Caller;
use crate::state::AppState;
use crate::transcribe::{TranscribeError, Transcript};
use common::telephony::{cloudtalk, ringcentral};
use reqwest::Method;

/// `RingCentral` audio a token points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaToken {
    Recording(String),
    Voicemail {
        extension: String,
        message: String,
        attachment: String,
    },
}

fn numeric(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
}

impl MediaToken {
    /// Strict parse: these come back from the browser.
    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        if let Some(id) = value.strip_prefix("rec_") {
            return numeric(id).then(|| Self::Recording(id.to_string()));
        }
        let parts: Vec<&str> = value.strip_prefix("vm_")?.split('_').collect();
        match parts.as_slice() {
            [extension, message, attachment]
                if (numeric(extension) || *extension == "me")
                    && numeric(message)
                    && numeric(attachment) =>
            {
                Some(Self::Voicemail {
                    extension: (*extension).to_string(),
                    message: (*message).to_string(),
                    attachment: (*attachment).to_string(),
                })
            }
            _ => None,
        }
    }

    fn extension_segment(extension: &str) -> &str {
        if extension == "me" { "~" } else { extension }
    }

    /// The REST path of the audio.
    pub fn path(&self) -> String {
        match self {
            Self::Recording(id) => format!("account/~/recording/{id}/content"),
            Self::Voicemail {
                extension,
                message,
                attachment,
            } => format!(
                "account/~/extension/{}/message-store/{message}/content/{attachment}",
                Self::extension_segment(extension)
            ),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CallTranscriptError {
    #[error("Recording not available")]
    NotAvailable,
    #[error("Failed to fetch recording")]
    FetchFailed,
    #[error("Recording is empty")]
    Empty,
    #[error("No speech detected in recording")]
    NoSpeech,
    #[error("Could not download the call recording from {0}")]
    Provider(&'static str),
    #[error("{0}")]
    Transcribe(#[from] TranscribeError),
}

impl CallTranscriptError {
    /// What the page shows.
    pub fn user_message(&self) -> String {
        match self {
            Self::Transcribe(TranscribeError::NotConfigured) => {
                "Transcription is not configured".into()
            }
            Self::Transcribe(_) => "Failed to transcribe call".into(),
            other => other.to_string(),
        }
    }
}

fn is_audio_type(content_type: &str) -> bool {
    let lower = content_type.to_ascii_lowercase();
    lower.contains("audio/") || lower.contains("octet-stream") || lower.contains("video/mp4")
}

fn looks_like_audio(bytes: &[u8]) -> bool {
    bytes.len() >= 12
        && (bytes.starts_with(b"RIFF")
            || bytes.starts_with(b"ID3")
            || bytes.starts_with(b"fLaC")
            || bytes.starts_with(b"OggS")
            || (bytes[0] == 0xff && bytes[1] & 0xe0 == 0xe0))
}

/// `RingCentral`'s own voicemail transcript, if it made one.
async fn voicemail_transcript(
    state: &AppState,
    company_id: i32,
    token: &MediaToken,
) -> Option<String> {
    let MediaToken::Voicemail {
        extension, message, ..
    } = token
    else {
        return None;
    };
    let segment = MediaToken::extension_segment(extension);
    let record = ringcentral::request(
        &state.pool,
        &state.telephony_http,
        company_id,
        Method::GET,
        &format!("account/~/extension/{segment}/message-store/{message}"),
        &ringcentral::Body::Empty,
        None,
    )
    .await
    .ok()?;
    let attachment = attachment_of(&record, "AudioTranscription")?;
    let (bytes, _) = ringcentral::get_raw(
        &state.pool,
        &state.telephony_http,
        company_id,
        &format!("account/~/extension/{segment}/message-store/{message}/content/{attachment}"),
    )
    .await
    .ok()?;
    let text = String::from_utf8_lossy(&bytes).trim().to_string();
    let lower = text.to_ascii_lowercase();
    // RingCentral writes "Transcription not available" when it could not.
    let unavailable = lower.strip_prefix("transcription").is_some_and(|rest| {
        let words = rest.trim_start();
        words.len() < rest.len()
            && (words.starts_with("not available") || words.starts_with("failed"))
    });
    (!text.is_empty() && !unavailable).then_some(text)
}

/// The call's audio and its content type.
pub async fn fetch_audio(
    state: &AppState,
    company_id: i32,
    call_id: &str,
    recording_link: Option<&str>,
) -> Result<(Vec<u8>, String), CallTranscriptError> {
    if let Some(token) = recording_link.and_then(MediaToken::parse) {
        let (bytes, content_type) = ringcentral::get_raw(
            &state.pool,
            &state.telephony_http,
            company_id,
            &token.path(),
        )
        .await
        .map_err(|_| CallTranscriptError::FetchFailed)?;
        let content_type = if content_type.is_empty() {
            "audio/mpeg".into()
        } else {
            content_type
        };
        return Ok((bytes, content_type));
    }
    if !numeric(call_id) {
        return Err(CallTranscriptError::NotAvailable);
    }
    let api = cloudtalk::Api {
        pool: &state.pool,
        client: &state.telephony_http,
        base_url: &state.cloudtalk_base,
        company_id,
    };
    let recording = cloudtalk::get_raw(api, &format!("calls/recording/{call_id}.json"))
        .await
        .map_err(|_| CallTranscriptError::Provider("CloudTalk"))?;
    if !recording.bytes.is_empty()
        && (is_audio_type(&recording.content_type) || looks_like_audio(&recording.bytes))
    {
        let content_type =
            if is_audio_type(&recording.content_type) && !recording.content_type.is_empty() {
                recording.content_type
            } else {
                "audio/wav".into()
            };
        return Ok((recording.bytes, content_type));
    }
    let link = recording_link
        .map(str::trim)
        .filter(|link| link.starts_with("https://") || link.starts_with("http://"))
        .ok_or(CallTranscriptError::NotAvailable)?;
    let linked = cloudtalk::get_link(api, link)
        .await
        .map_err(|_| CallTranscriptError::FetchFailed)?;
    let content_type = if linked.content_type.is_empty() {
        "audio/wav".into()
    } else {
        linked.content_type
    };
    Ok((linked.bytes, content_type))
}

/// Transcribes a call, reusing `RingCentral`'s voicemail transcript when it
/// has one.
pub async fn transcribe_call(
    state: &AppState,
    caller: Caller<'_>,
    call_id: &str,
    recording_link: Option<&str>,
    speaker_labels: bool,
) -> Result<Transcript, CallTranscriptError> {
    let company_id = caller.company_id.unwrap_or_default();
    if let Some(token) = recording_link.and_then(MediaToken::parse)
        && let Some(text) = voicemail_transcript(state, company_id, &token).await
    {
        return Ok(Transcript {
            text,
            utterances: None,
            audio_seconds: None,
        });
    }
    let (audio, _) = fetch_audio(state, company_id, call_id, recording_link).await?;
    if audio.is_empty() {
        return Err(CallTranscriptError::Empty);
    }
    let mut transcript = state
        .transcriber
        .transcribe(&state.pool, caller, audio, speaker_labels)
        .await?;
    transcript.text = transcript.text.trim().to_string();
    if transcript.text.is_empty() {
        return Err(CallTranscriptError::NoSpeech);
    }
    Ok(transcript)
}

#[cfg(test)]
mod tests {
    use super::MediaToken;

    #[test]
    fn media_tokens_are_parsed_strictly() {
        assert_eq!(
            MediaToken::parse("rec_77"),
            Some(MediaToken::Recording("77".into()))
        );
        assert_eq!(
            MediaToken::parse("vm_me_5_6").map(|token| token.path()),
            Some("account/~/extension/~/message-store/5/content/6".into())
        );
        assert_eq!(MediaToken::parse("rec_7/../x"), None);
        assert_eq!(MediaToken::parse("vm_1_2"), None);
        assert_eq!(MediaToken::parse("https://x.test/a.wav"), None);
    }
}
