//! The company's "Spam and blocked numbers", set by its admins.
//!
//! Blocked numbers and area codes, trusted numbers, calls with no caller
//! ID, and what happens to calls the caller-ID check (STIR/SHAKEN) did not
//! vouch for. Applied by the inbound TeXML before anything rings
//! ([`screen`]).
//!
//! A trusted number is never blocked, not even as spam. Otherwise a call is
//! stopped when it has no caller ID (and the company blocks those), its
//! number or area code is blocked, or it is likely spam and the company
//! blocks spam or sends it to voicemail. A blocked call gets the company's
//! action for blocked calls: the announcement then hang up, hang up, or
//! voicemail.

use super::numbers::{is_nanp, to_e164};
use super::wire_enum;
use crate::openapi::ToSchema;
use serde::Serialize;
use sqlx::{FromRow, MySqlPool};

/// What callers from blocked numbers hear by default.
pub const DEFAULT_ANNOUNCEMENT: &str =
    "I'm sorry. The number you dialed can not be reached from your calling area. Goodbye.";
/// Entries a company's blocked list, and its trusted list, may hold.
pub const MAX_ENTRIES: usize = 1000;
/// The longest announcement.
pub const MAX_ANNOUNCEMENT_CHARS: usize = 500;

wire_enum! {
    /// What happens to likely spam: `ring` (allow to ring, marked as
    /// possible spam), `block` (like a blocked number) or `voicemail`.
    pub enum SpamAction {
        /// Allow to ring; the call shows as possible spam.
        Ring = "ring",
        /// Block it: the action for blocked calls.
        Block = "block",
        /// Send it to voicemail.
        Voicemail = "voicemail",
    }
}

wire_enum! {
    /// What a blocked caller gets: `announce` (the announcement, then hang
    /// up), `hangup`, or `voicemail`.
    pub enum BlockedAction {
        /// Play the announcement, then hang up.
        Announce = "announce",
        /// Hang up at once.
        Hangup = "hangup",
        /// Send the caller to voicemail (no phone rings).
        Voicemail = "voicemail",
    }
}

wire_enum! {
    /// Why a call was stopped: `number`, `area_code`, `no_caller_id` or
    /// `spam`.
    pub enum BlockReason {
        /// The caller's number is blocked.
        Number = "number",
        /// The caller's area code is blocked.
        AreaCode = "area_code",
        /// The caller hid their number.
        NoCallerId = "no_caller_id",
        /// The caller-ID check marked the call likely spam.
        Spam = "spam",
    }
}

/// The company's blocking, as stored (the defaults when it saved none).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocking {
    pub spam_action: SpamAction,
    pub blocked_action: BlockedAction,
    /// The announcement as saved; None = [`DEFAULT_ANNOUNCEMENT`].
    pub announcement: Option<String>,
    pub block_no_caller_id: bool,
    /// E.164 numbers and 3-digit area codes.
    pub blocked: Vec<String>,
    /// E.164 numbers.
    pub trusted: Vec<String>,
}

impl Default for Blocking {
    fn default() -> Self {
        Self {
            spam_action: SpamAction::Ring,
            blocked_action: BlockedAction::Announce,
            announcement: None,
            block_no_caller_id: false,
            blocked: Vec::new(),
            trusted: Vec::new(),
        }
    }
}

#[derive(FromRow)]
struct BlockingRow {
    spam_action: String,
    blocked_action: String,
    announcement: Option<String>,
    block_no_caller_id: bool,
}

/// The company's blocking.
pub async fn load(pool: &MySqlPool, company_id: i32) -> sqlx::Result<Blocking> {
    let row: Option<BlockingRow> = sqlx::query_as(
        "SELECT CAST(spam_action AS CHAR) AS spam_action,
                CAST(blocked_action AS CHAR) AS blocked_action, announcement,
                block_no_caller_id <> 0 AS block_no_caller_id
           FROM phone_call_blocking WHERE company_id = ?",
    )
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    let entries: Vec<(String, String)> = sqlx::query_as(
        "SELECT CAST(list AS CHAR), pattern FROM phone_call_block_list
          WHERE company_id = ? ORDER BY pattern, id",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    let mut blocking = row.map_or_else(Blocking::default, |row| Blocking {
        spam_action: SpamAction::parse(&row.spam_action).unwrap_or(SpamAction::Ring),
        blocked_action: BlockedAction::parse(&row.blocked_action)
            .unwrap_or(BlockedAction::Announce),
        announcement: row
            .announcement
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty()),
        block_no_caller_id: row.block_no_caller_id,
        ..Blocking::default()
    });
    for (list, pattern) in entries {
        match list.as_str() {
            "blocked" => blocking.blocked.push(pattern),
            "trusted" => blocking.trusted.push(pattern),
            _ => {}
        }
    }
    Ok(blocking)
}

/// An entry as the company may save it: an E.164 number (a US or Canada
/// number may be typed any way), or with `area_codes`, a 3-digit US or
/// Canada area code (2 to 9 first). None when it is neither.
pub fn normalize_entry(text: &str, area_codes: bool) -> Option<String> {
    let trimmed = text.trim();
    let digits: String = trimmed.chars().filter(char::is_ascii_digit).collect();
    if area_codes
        && digits.len() == 3
        && !trimmed.starts_with('+')
        && trimmed
            .chars()
            .all(|c| c.is_ascii_digit() || "() ".contains(c))
    {
        return digits
            .starts_with(|c: char| ('2'..='9').contains(&c))
            .then_some(digits);
    }
    to_e164(trimmed).filter(|number| number.len() >= 8)
}

/// A call the blocking stops: why, and what the caller gets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Screened {
    pub reason: BlockReason,
    pub action: BlockedAction,
}

/// Whether the blocking stops a call from `caller` (E.164; None when the
/// caller hid their number or sent none that reads as one), `spam` when the
/// caller-ID check marked it likely spam.
pub fn screen(blocking: &Blocking, caller: Option<&str>, spam: bool) -> Option<Screened> {
    let blocked = |reason| {
        Some(Screened {
            reason,
            action: blocking.blocked_action,
        })
    };
    let Some(caller) = caller else {
        return if blocking.block_no_caller_id {
            blocked(BlockReason::NoCallerId)
        } else {
            None
        };
    };
    if blocking.trusted.iter().any(|number| number == caller) {
        return None;
    }
    if blocking.blocked.iter().any(|number| number == caller) {
        return blocked(BlockReason::Number);
    }
    let area_code = caller
        .strip_prefix("+1")
        .filter(|_| is_nanp(caller))
        .map(|national| &national[..3]);
    if area_code.is_some_and(|code| blocking.blocked.iter().any(|entry| entry == code)) {
        return blocked(BlockReason::AreaCode);
    }
    if !spam {
        return None;
    }
    match blocking.spam_action {
        SpamAction::Ring => None,
        SpamAction::Block => blocked(BlockReason::Spam),
        SpamAction::Voicemail => Some(Screened {
            reason: BlockReason::Spam,
            action: BlockedAction::Voicemail,
        }),
    }
}

/// Logs a call the blocking stopped (once per Telnyx call): no Recents row
/// and no push for it, so this is where it shows.
pub async fn log_blocked(
    pool: &MySqlPool,
    company_id: i32,
    number_id: i32,
    other_number: &str,
    screened: &Screened,
    call_sid: Option<&str>,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT IGNORE INTO phone_blocked_calls
           (company_id, number_id, other_number, reason, action, telnyx_call_sid)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(company_id)
    .bind(number_id)
    .bind(other_number)
    .bind(screened.reason.as_str())
    .bind(screened.action.as_str())
    .bind(call_sid)
    .execute(pool)
    .await
    .map(drop)
}

impl Blocking {
    /// What blocked callers hear with `announce`.
    pub fn announcement(&self) -> &str {
        self.announcement.as_deref().unwrap_or(DEFAULT_ANNOUNCEMENT)
    }
}

/// The company's blocking, as the app shows and saves it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CallBlocking {
    /// What happens to likely spam: `ring`, `block` or `voicemail`.
    pub spam_action: SpamAction,
    /// What blocked callers get: `announce`, `hangup` or `voicemail`.
    pub blocked_action: BlockedAction,
    /// What they hear with `announce` (the default when none was saved).
    pub announcement: String,
    /// The announcement is the default one.
    pub default_announcement: bool,
    /// Calls that hide their number are blocked.
    pub block_no_caller_id: bool,
    /// Blocked numbers (E.164) and 3-digit area codes, sorted.
    pub blocked: Vec<String>,
    /// Trusted numbers (E.164, never blocked), sorted.
    pub trusted: Vec<String>,
}

impl Blocking {
    pub fn wire(&self) -> CallBlocking {
        CallBlocking {
            spam_action: self.spam_action,
            blocked_action: self.blocked_action,
            announcement: self.announcement().to_string(),
            default_announcement: self.announcement.is_none(),
            block_no_caller_id: self.block_no_caller_id,
            blocked: self.blocked.clone(),
            trusted: self.trusted.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocking() -> Blocking {
        Blocking {
            blocked: vec!["+13175550666".into(), "212".into()],
            trusted: vec!["+12125550100".into()],
            ..Blocking::default()
        }
    }

    #[test]
    fn entries_are_numbers_or_area_codes() {
        assert_eq!(
            normalize_entry("(317) 555-0666", true).as_deref(),
            Some("+13175550666")
        );
        assert_eq!(normalize_entry("212", true).as_deref(), Some("212"));
        assert_eq!(normalize_entry("(212)", true).as_deref(), Some("212"));
        assert_eq!(normalize_entry("212", false), None);
        // Area codes start with 2 to 9.
        assert_eq!(normalize_entry("112", true), None);
        assert_eq!(normalize_entry("+212", true), None);
        assert_eq!(normalize_entry("call me", true), None);
        assert_eq!(
            normalize_entry("+44 20 7946 0958", true).as_deref(),
            Some("+442079460958")
        );
    }

    #[test]
    fn trusted_numbers_are_never_blocked() {
        let mut company = blocking();
        company.spam_action = SpamAction::Block;
        // In a blocked area code, likely spam, but trusted.
        assert_eq!(screen(&company, Some("+12125550100"), true), None);
        assert_eq!(
            screen(&company, Some("+12125550101"), false),
            Some(Screened {
                reason: BlockReason::AreaCode,
                action: BlockedAction::Announce
            })
        );
        assert_eq!(
            screen(&company, Some("+13175550666"), false),
            Some(Screened {
                reason: BlockReason::Number,
                action: BlockedAction::Announce
            })
        );
        assert_eq!(screen(&company, Some("+13175550667"), false), None);
    }

    #[test]
    fn hidden_numbers_and_spam_follow_the_companys_choices() {
        let mut company = blocking();
        assert_eq!(screen(&company, None, false), None);
        company.block_no_caller_id = true;
        company.blocked_action = BlockedAction::Hangup;
        assert_eq!(
            screen(&company, None, false),
            Some(Screened {
                reason: BlockReason::NoCallerId,
                action: BlockedAction::Hangup
            })
        );
        // Likely spam rings unless the company says otherwise.
        assert_eq!(screen(&company, Some("+13175550101"), true), None);
        company.spam_action = SpamAction::Voicemail;
        assert_eq!(
            screen(&company, Some("+13175550101"), true),
            Some(Screened {
                reason: BlockReason::Spam,
                action: BlockedAction::Voicemail
            })
        );
        company.spam_action = SpamAction::Block;
        assert_eq!(
            screen(&company, Some("+13175550101"), true),
            Some(Screened {
                reason: BlockReason::Spam,
                action: BlockedAction::Hangup
            })
        );
        // An area code never matches a number abroad.
        assert_eq!(screen(&company, Some("+442125550101"), false), None);
    }
}
