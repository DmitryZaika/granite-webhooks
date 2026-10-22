//! Telling a voicemail from a conversation by its transcript. Port of
//! `app/lib/callAiHelpers.ts` (same patterns).

use regex::Regex;
use std::sync::LazyLock;

static GREETING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:please\s+leave(?:\s+(?:your|a))?\s+message|leave\s+(?:us\s+)?(?:your\s+)?(?:a\s+)?message|after\s+the\s+(?:tone|beep)|at\s+the\s+(?:tone|beep)|voice\s*mail(?:\s*box)?|mailbox\s+is\s+full|unable\s+to\s+take\s+your\s+call|can't\s+take\s+your\s+call|not\s+available(?:\s+to\s+take\s+your\s+call)?|record\s+your\s+message)\b")
        .expect("valid pattern")
});

static REP_OR_JOB_CONTENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:(?:this|hey)\s+is\s+\w+\s+with\b|grand\s*diva|grandeeper|countertop|counter\s+top|granite|quartz|marble|quote|estimate|pricing|interested|follow(?:ing)?\s+up|your\s+request|months?\s+ago|ready\s+in|send\s+(?:you|the|us)|kitchen|project|stone|fabricat|call(?:ing)?\s+back|still\s+interested|left\s+(?:you\s+)?(?:a\s+)?message|checking\s+in|touching\s+base)\b")
        .expect("valid pattern")
});

const GREETING_ONLY_MAX_WORDS: usize = 45;

/// The transcript has a mailbox greeting in it.
pub fn sounds_like_voicemail(transcript: &str) -> bool {
    let text = transcript.trim();
    !text.is_empty() && GREETING.is_match(text)
}

/// Only the mailbox greeting: the rep left no message.
pub fn greeting_only(transcript: &str) -> bool {
    let text = transcript.trim();
    sounds_like_voicemail(text)
        && !REP_OR_JOB_CONTENT.is_match(text)
        && text.split_whitespace().count() <= GREETING_ONLY_MAX_WORDS
}

/// How a call past the attempt threshold went: `attempt` (greeting only),
/// `voicemail` (a message left) or `conversation`.
pub fn classify(provider_voicemail: bool, transcript: &str) -> &'static str {
    if !(provider_voicemail || sounds_like_voicemail(transcript)) {
        return "conversation";
    }
    if greeting_only(transcript) {
        "attempt"
    } else {
        "voicemail"
    }
}

#[cfg(test)]
mod tests {
    use super::classify;

    #[test]
    fn calls_are_told_apart() {
        assert_eq!(
            classify(
                false,
                "Hi, the person you called is not available. Please leave a message after the tone."
            ),
            "attempt"
        );
        assert_eq!(
            classify(
                false,
                "Please leave a message. Hey, this is Rita with Acme about your quartz quote, call me back."
            ),
            "voicemail"
        );
        assert_eq!(
            classify(false, "Hello? Yes, we'd like the white quartz."),
            "conversation"
        );
        assert_eq!(
            classify(true, "Hi Alice, it's about your kitchen."),
            "voicemail"
        );
    }
}
