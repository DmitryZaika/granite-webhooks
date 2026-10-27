//! Telling a voicemail from a conversation by its transcript. Port of
//! `app/lib/callAiHelpers.ts` (same patterns).

use crate::transcribe::{Transcript, Utterance};
use regex::Regex;
use std::collections::HashMap;
use std::sync::LazyLock;

/// Talk time up to this is an attempt, never transcribed (the CRM's
/// `CALL_ATTEMPT_MAX_TALK_SECONDS`): too short for a message.
pub const ATTEMPT_MAX_TALK_SECONDS: i64 = 7;
/// Talk time up to this is a conversation only with real back-and-forth: a
/// "Hello?" and a hang-up has no mailbox greeting either.
pub const SHORT_CALL_MAX_TALK_SECONDS: i64 = 30;
/// A voicemail is greeting → rep (one change); a conversation goes back and forth.
const MIN_SPEAKER_CHANGES: usize = 3;
const MIN_WORDS_PER_SPEAKER: usize = 5;

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

/// Both sides talked: a few turns, each side with a few words.
pub fn back_and_forth(utterances: &[Utterance]) -> bool {
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

/// [`classify`] for a call of `talk_seconds`: up to
/// [`SHORT_CALL_MAX_TALK_SECONDS`] a conversation needs [`back_and_forth`]
/// in the transcript's speaker turns, else it is an attempt.
pub fn classify_call(
    talk_seconds: i64,
    provider_voicemail: bool,
    transcript: &Transcript,
) -> &'static str {
    let outcome = classify(provider_voicemail, &transcript.text);
    let short = talk_seconds <= SHORT_CALL_MAX_TALK_SECONDS;
    if outcome == "conversation"
        && short
        && !transcript.utterances.as_deref().is_some_and(back_and_forth)
    {
        "attempt"
    } else {
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::{classify, classify_call};
    use crate::transcribe::Transcript;

    #[test]
    fn short_calls_need_both_sides() {
        let hang_up = Transcript::from_labeled_text("Speaker A: Hello? Hello?");
        assert_eq!(classify_call(12, false, &hang_up), "attempt");
        let talk = Transcript::from_labeled_text(
            "Speaker A: Hello, who is this?\nSpeaker B: Hi, it's Rita with Acme about your quartz quote.\nSpeaker A: Oh hi, I'm driving right now, sorry.\nSpeaker B: No problem, I'll call you tomorrow then.",
        );
        assert_eq!(classify_call(25, false, &talk), "conversation");
        let message = Transcript::from_labeled_text(
            "Speaker A: Please leave a message after the tone.\nSpeaker B: Hi, it's Rita about your quote, call me back.",
        );
        assert_eq!(classify_call(15, false, &message), "voicemail");
        // Longer calls keep the phrases-only rule.
        assert_eq!(classify_call(45, false, &hang_up), "conversation");
    }

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
