//! Whether the next message greets the customer by name.
//!
//! One rule for every AI-written SMS and email (the AI Reply button, inline
//! autocomplete, follow-ups): greet by name only when we have never written
//! with this customer, or the newest message with them — SMS or email,
//! theirs or ours — is more than [`GREET_AFTER_DAYS`] days old. Otherwise
//! the conversation is still going: no greeting and no name, the message
//! starts with its content. Either way the name appears at most once.
//!
//! The gap is counted in hours from now, so it does not depend on a
//! calendar day's time zone.

use super::crm_hold::parse_followup_timeline_at;
use super::dates::Instant;
use super::prompt::FollowupTimelineMessage;
use chrono::Duration;

/// A gap longer than this many days reopens the conversation with a greeting.
pub const GREET_AFTER_DAYS: i64 = 60;

/// How recent the conversation with this customer is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversationRecency {
    /// No dated SMS or email with them yet: greet by name.
    NoHistory,
    /// Still going: no greeting, no name.
    Continuing {
        /// Since the newest message, either way, on any channel.
        minutes_since_last: i64,
    },
    /// Picked up after more than [`GREET_AFTER_DAYS`] days: greet by name.
    Reopening {
        /// Whole days since the newest message.
        days_since_last: i64,
    },
}

impl ConversationRecency {
    /// No greeting and no name.
    pub const fn is_continuing(self) -> bool {
        matches!(self, Self::Continuing { .. })
    }
}

/// How recent the conversation is at `now`, from every dated message.
pub fn conversation_recency(
    timeline: &[FollowupTimelineMessage],
    now: Instant,
) -> ConversationRecency {
    let Some(last) = timeline
        .iter()
        .filter_map(|m| parse_followup_timeline_at(&m.at))
        .max()
    else {
        return ConversationRecency::NoHistory;
    };
    // A message stamped a little ahead of the server clock is "just now".
    let since = (now - last).max(Duration::zero());
    if since > Duration::days(GREET_AFTER_DAYS) {
        ConversationRecency::Reopening {
            days_since_last: since.num_days(),
        }
    } else {
        ConversationRecency::Continuing {
            minutes_since_last: since.num_minutes(),
        }
    }
}

/// "12 minutes ago", "about 5 hours ago", "3 days ago".
pub fn describe_ago(minutes: i64) -> String {
    match minutes {
        m if m < 2 => "just now".into(),
        m if m < 60 => format!("{m} minutes ago"),
        m if m < 120 => "about an hour ago".into(),
        m if m < 48 * 60 => format!("about {} hours ago", m / 60),
        m => format!("{} days ago", m / (24 * 60)),
    }
}

/// The greeting and name rule for a drafted message, as a prompt line.
pub fn greeting_instruction(recency: ConversationRecency) -> String {
    match recency {
        ConversationRecency::Continuing { minutes_since_last } => format!(
            "GREETING: NONE. This conversation is ongoing: the last message with this customer (SMS or email, either side) was {}. Do NOT open with a greeting (\"Hi\", \"Hey\", \"Hello\", \"Good morning\", \"Dear\") and do NOT use the customer's name anywhere in the message. Start straight with the content, like the next text in a running thread. This applies to email too, even when the earlier messages were texts.",
            describe_ago(minutes_since_last)
        ),
        ConversationRecency::Reopening { days_since_last } => format!(
            "GREETING: BY NAME. The last message with this customer was {days_since_last} days ago, so open with a short greeting and their first name (\"Hi Ann,\"), then get straight to the point. Use their name only that once. Never \"Hello <Full Name>\" or \"Dear\"."
        ),
        ConversationRecency::NoHistory => "GREETING: BY NAME. No earlier SMS or email with this customer, so open with a short greeting and their first name (\"Hi Ann,\"). Use their name only that once.".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::followups::send_window::FollowupChannel;
    use crate::followups::timeline::Direction;
    use Direction::{Inbound as I, Outbound as O};
    use FollowupChannel::{Email as E, Sms as S};

    fn message(
        at: &str,
        channel: FollowupChannel,
        direction: Direction,
    ) -> FollowupTimelineMessage {
        FollowupTimelineMessage {
            at: at.into(),
            channel,
            direction,
            text: "x".into(),
            from_email: None,
            from_phone: None,
        }
    }

    fn at(iso: &str) -> Instant {
        chrono::DateTime::parse_from_rfc3339(iso).unwrap().to_utc()
    }

    #[test]
    fn a_new_conversation_greets_by_name() {
        let now = at("2026-10-14T15:00:00Z");
        assert_eq!(
            conversation_recency(&[], now),
            ConversationRecency::NoHistory
        );
        // An email without a send time is not a dated message.
        assert_eq!(
            conversation_recency(&[message("", E, O)], now),
            ConversationRecency::NoHistory
        );
        let line = greeting_instruction(ConversationRecency::NoHistory);
        assert!(line.starts_with("GREETING: BY NAME") && line.contains("only that once"));
    }

    #[test]
    fn a_text_just_sent_means_no_greeting_or_name_on_the_email() {
        // The rep texted her 20 minutes ago and now writes an email.
        let timeline = [
            message("2026-09-01 10:00", E, O),
            message("2026-10-14 14:40", S, O),
        ];
        let recency = conversation_recency(&timeline, at("2026-10-14T15:00:00Z"));
        assert_eq!(
            recency,
            ConversationRecency::Continuing {
                minutes_since_last: 20
            }
        );
        let line = greeting_instruction(recency);
        assert!(line.starts_with("GREETING: NONE"));
        assert!(line.contains("20 minutes ago"));
        assert!(line.contains("do NOT use the customer's name"));
        assert!(line.contains("applies to email too"));
    }

    #[test]
    fn any_message_within_sixty_days_continues() {
        // Either side and either channel counts; the newest message decides.
        for last in [
            message("2026-08-16 09:00", S, I),
            message("2026-08-16 09:00", E, O),
        ] {
            let timeline = [message("2026-01-02 10:00", S, O), last];
            // 59 days later: still the same conversation.
            assert!(conversation_recency(&timeline, at("2026-10-14T09:00:00Z")).is_continuing());
            // Exactly 60 days is not more than 60.
            assert!(conversation_recency(&timeline, at("2026-10-15T09:00:00Z")).is_continuing());
            assert_eq!(
                conversation_recency(&timeline, at("2026-10-15T09:01:00Z")),
                ConversationRecency::Reopening {
                    days_since_last: 60
                }
            );
        }
    }

    #[test]
    fn last_evening_is_not_a_new_day_greeting() {
        // 9:30 PM in Indiana is 01:30 UTC the next day: the morning after is
        // another UTC and store date but the same conversation.
        let timeline = [message("2026-10-14 01:30", S, I)];
        let recency = conversation_recency(&timeline, at("2026-10-14T13:00:00Z"));
        assert_eq!(
            recency,
            ConversationRecency::Continuing {
                minutes_since_last: 690
            }
        );
        assert!(greeting_instruction(recency).contains("about 11 hours ago"));
    }

    #[test]
    fn a_long_silence_greets_by_name_once() {
        let timeline = [
            message("2026-07-01 18:00", S, I),
            message("2026-07-01 18:25", S, O),
        ];
        let recency = conversation_recency(&timeline, at("2026-10-14T15:00:00Z"));
        assert_eq!(
            recency,
            ConversationRecency::Reopening {
                days_since_last: 104
            }
        );
        let line = greeting_instruction(recency);
        assert!(line.starts_with("GREETING: BY NAME") && line.contains("104 days ago"));
        assert!(line.contains("Use their name only that once"));
    }

    #[test]
    fn a_message_stamped_ahead_of_the_clock_is_just_now() {
        let timeline = [message("2026-10-14 15:05", S, I)];
        assert_eq!(
            conversation_recency(&timeline, at("2026-10-14T15:00:00Z")),
            ConversationRecency::Continuing {
                minutes_since_last: 0
            }
        );
        assert_eq!(describe_ago(0), "just now");
        assert_eq!(describe_ago(90), "about an hour ago");
        assert_eq!(describe_ago(300), "about 5 hours ago");
        assert_eq!(describe_ago(3 * 24 * 60), "3 days ago");
    }
}
