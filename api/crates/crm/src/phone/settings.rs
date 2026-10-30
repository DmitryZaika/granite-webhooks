//! A user's phone settings (`phone_user_settings`; a user without a row has
//! the defaults) and the notices they accepted (`phone_notice_acks`).
//!
//! Settings belong to the user in one company: a row left from another
//! company reads as the defaults and is replaced on the next write.

use super::availability::{Availability, WorkingHours};
use super::call_rules::ForwardAllTo;
use super::lines::CallerId;
use super::wire_enum;
use crate::openapi::ToSchema;
use chrono::{DateTime, Utc};
use lambda_http::tracing;
use serde::Serialize;
use sqlx::{FromRow, MySql, MySqlPool, Transaction};

/// The recording notice's current version; the app shows it again when this
/// grows.
pub const RECORDING_NOTICE_VERSION: i32 = 1;
/// The 911 notice's current version.
pub const E911_NOTICE_VERSION: i32 = 1;
/// How long calls ring a user's phones when they chose nothing.
pub const DEFAULT_RING_TIMEOUT_SECS: i64 = 25;
/// The ring timeouts a user may choose.
pub const RING_TIMEOUT_SECS: std::ops::RangeInclusive<i64> = 15..=60;

wire_enum! {
    /// A notice the app shows once per version: `recording` (calls may be
    /// recorded) or `e911` (911 goes through the phone's own dialer).
    pub enum NoticeKind {
        /// Calls may be recorded.
        Recording = "recording",
        /// 911 and other N11 codes open the phone's own dialer.
        E911 = "e911",
    }
}

/// A user's settings as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserSettings {
    pub default_caller_number_id: Option<i32>,
    pub ai_notes: bool,
    pub dnd_until: Option<DateTime<Utc>>,
    pub dnd_indefinite: bool,
    pub working_hours: Option<WorkingHours>,
    /// The verified cell (E.164).
    pub cell_number: Option<String>,
    /// The user wants their cell rung when the app does not answer.
    pub cell_fallback: bool,
    pub ring_timeout_secs: i64,
    /// "Forward all calls" is on, to `forward_all_to`.
    pub forward_all: bool,
    pub forward_all_to: ForwardAllTo,
    /// With `forward_all_to = number`: E.164.
    pub forward_all_number: Option<String>,
    /// What the user's phones show for an incoming call.
    pub display_number: DisplayNumber,
    /// Show the number called when the caller hides theirs.
    pub unknown_show_mine: bool,
    /// Announce calls to my direct lines, and to shared and main lines,
    /// before they connect; with the line's name.
    pub announce_direct: bool,
    pub announce_shared: bool,
    pub announce_line_name: bool,
    /// A fallback cell asks for the PIN (whose hash this is) instead of
    /// "press 1"; only with a PIN set.
    pub answer_pin_hash: Option<String>,
    /// What callers hear on hold: a built-in track id, `custom` or `none`.
    pub hold_music: String,
    /// The uploaded track (with `custom`): its public URL and file name.
    pub hold_music_url: Option<String>,
    pub hold_music_name: Option<String>,
}

wire_enum! {
    /// What my phones show for an incoming call: `caller` (the caller's
    /// number) or `called` (the company number they called).
    pub enum DisplayNumber {
        /// The caller's number (their caller ID).
        Caller = "caller",
        /// The company number that was called.
        Called = "called",
    }
}

impl Default for UserSettings {
    fn default() -> Self {
        Self {
            default_caller_number_id: None,
            ai_notes: true,
            dnd_until: None,
            dnd_indefinite: false,
            working_hours: None,
            cell_number: None,
            cell_fallback: false,
            ring_timeout_secs: DEFAULT_RING_TIMEOUT_SECS,
            forward_all: false,
            forward_all_to: ForwardAllTo::Voicemail,
            forward_all_number: None,
            display_number: DisplayNumber::Caller,
            unknown_show_mine: false,
            announce_direct: false,
            announce_shared: false,
            announce_line_name: true,
            answer_pin_hash: None,
            hold_music: super::hold_music::DEFAULT_TRACK.to_string(),
            hold_music_url: None,
            hold_music_name: None,
        }
    }
}

#[derive(FromRow)]
struct SettingsRow {
    default_caller_number_id: Option<i32>,
    ai_notes: bool,
    dnd_until: Option<DateTime<Utc>>,
    dnd_indefinite: bool,
    working_hours: Option<String>,
    cell_number: Option<String>,
    cell_fallback: bool,
    ring_timeout_secs: i64,
    forward_all: bool,
    forward_all_to: String,
    forward_all_number: Option<String>,
    display_number: String,
    unknown_show_mine: bool,
    announce_direct: bool,
    announce_shared: bool,
    announce_line_name: bool,
    require_pin: bool,
    answer_pin_hash: Option<String>,
    hold_music: String,
    hold_music_url: Option<String>,
    hold_music_name: Option<String>,
}

/// The user's settings in `company_id` (the defaults when there are none).
pub async fn load(pool: &MySqlPool, user_id: i32, company_id: i32) -> sqlx::Result<UserSettings> {
    let row: Option<SettingsRow> = sqlx::query_as(
        "SELECT default_caller_number_id, ai_notes <> 0 AS ai_notes, dnd_until,
                dnd_indefinite <> 0 AS dnd_indefinite, CAST(working_hours AS CHAR) AS working_hours,
                CASE WHEN cell_verified_at IS NOT NULL THEN cell_number END AS cell_number,
                cell_fallback <> 0 AS cell_fallback,
                CAST(ring_timeout_secs AS SIGNED) AS ring_timeout_secs,
                forward_all <> 0 AS forward_all, CAST(forward_all_to AS CHAR) AS forward_all_to,
                forward_all_number, CAST(display_number AS CHAR) AS display_number,
                unknown_show_mine <> 0 AS unknown_show_mine,
                announce_direct <> 0 AS announce_direct, announce_shared <> 0 AS announce_shared,
                announce_line_name <> 0 AS announce_line_name, require_pin <> 0 AS require_pin,
                answer_pin_hash, hold_music, hold_music_url, hold_music_name
           FROM phone_user_settings WHERE user_id = ? AND company_id = ?",
    )
    .bind(user_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map_or_else(UserSettings::default, |row| UserSettings {
        default_caller_number_id: row.default_caller_number_id,
        ai_notes: row.ai_notes,
        dnd_until: row.dnd_until,
        dnd_indefinite: row.dnd_indefinite,
        working_hours: row
            .working_hours
            .as_deref()
            .and_then(WorkingHours::from_json),
        cell_number: row.cell_number.filter(|cell| !cell.trim().is_empty()),
        cell_fallback: row.cell_fallback,
        ring_timeout_secs: row.ring_timeout_secs,
        forward_all: row.forward_all,
        forward_all_to: ForwardAllTo::parse(&row.forward_all_to).unwrap_or(ForwardAllTo::Voicemail),
        forward_all_number: row.forward_all_number,
        display_number: DisplayNumber::parse(&row.display_number).unwrap_or(DisplayNumber::Caller),
        unknown_show_mine: row.unknown_show_mine,
        announce_direct: row.announce_direct,
        announce_shared: row.announce_shared,
        announce_line_name: row.announce_line_name,
        answer_pin_hash: row.answer_pin_hash.filter(|_| row.require_pin),
        hold_music: row.hold_music,
        hold_music_url: row.hold_music_url,
        hold_music_name: row.hold_music_name,
    }))
}

/// Gives the user a settings row in `company_id`, locked.
///
/// A row left from another company is replaced with the defaults. The row
/// stays locked for the rest of the caller's transaction, so two requests
/// of one user that change the row or count the user's calls
/// (`limits::take_call_slot`) run one after the other.
///
/// The upsert takes the row's exclusive lock whether it inserts or finds
/// the row (a plain `INSERT IGNORE` would take a shared one, and two
/// requests upgrading shared locks deadlock).
pub async fn ensure_row(
    tx: &mut Transaction<'_, MySql>,
    user_id: i32,
    company_id: i32,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO phone_user_settings (user_id, company_id) VALUES (?, ?)
         ON DUPLICATE KEY UPDATE user_id = user_id",
    )
    .bind(user_id)
    .bind(company_id)
    .execute(&mut **tx)
    .await?;
    let stored: i32 = sqlx::query_scalar(
        "SELECT company_id FROM phone_user_settings WHERE user_id = ? FOR UPDATE",
    )
    .bind(user_id)
    .fetch_one(&mut **tx)
    .await?;
    if stored != company_id {
        sqlx::query("DELETE FROM phone_user_settings WHERE user_id = ?")
            .bind(user_id)
            .execute(&mut **tx)
            .await?;
        sqlx::query("INSERT INTO phone_user_settings (user_id, company_id) VALUES (?, ?)")
            .bind(user_id)
            .bind(company_id)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

/// The user's phone settings, as the app shows them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PhoneSettings {
    /// The caller ID the user chose as their default (one of
    /// `GET /v1/phone/caller-ids`); null when they chose none or it is no
    /// longer theirs.
    pub default_caller_number_id: Option<i32>,
    /// Transcribe my recorded calls and add an AI note to the deal.
    pub ai_notes: bool,
    /// When the user takes calls; null = always.
    pub working_hours: Option<WorkingHours>,
    /// The user's verified cell, E.164; null until one is verified
    /// (`POST /v1/phone/cell/verify`, then `/confirm`).
    pub cell_number: Option<String>,
    /// A cell number is verified.
    pub cell_verified: bool,
    /// Ring the verified cell after about 15 seconds, and when the app has
    /// no data connection.
    pub cell_fallback: bool,
    /// How long calls ring the user's phones, in seconds (15 to 60, default
    /// 25).
    pub ring_timeout_secs: i64,
    /// "Forward all calls" is on: calls to my own direct line go to
    /// `forwardAllTo`; on shared and main lines my phones just don't ring.
    /// Do not disturb wins over it.
    pub forward_all: bool,
    /// Where "Forward all calls" sends them: `voicemail` or `number`.
    pub forward_all_to: ForwardAllTo,
    /// With `number`: where to, E.164 (US or Canada); null when none was
    /// set.
    pub forward_all_number: Option<String>,
    /// What my phones show for an incoming call: `caller` (their number) or
    /// `called` (the company number they called).
    pub display_number: DisplayNumber,
    /// When the caller hides their number, show the number they called.
    pub unknown_show_mine: bool,
    /// Hear an announcement ("Call for <line> from <caller>") before calls
    /// to my direct lines connect.
    pub announce_direct: bool,
    /// The same for calls to shared lines and the main line.
    pub announce_shared: bool,
    /// The announcement names the line called.
    pub announce_line_name: bool,
    /// My fallback cell asks for my 4-digit PIN instead of "press 1"
    /// before it takes a call.
    pub require_pin: bool,
    /// What callers hear while I have them on hold: a built-in track id
    /// (`GET /v1/phone/hold-music`), `custom` (my upload) or `none`
    /// (silence).
    pub hold_music: String,
    /// My uploaded track's URL (MP3 or WAV), kept while another choice is
    /// made so it can be chosen again; null until I upload one. It plays
    /// only with `custom`.
    pub hold_music_url: Option<String>,
    /// The uploaded file's name, as shown; null without an upload.
    pub hold_music_name: Option<String>,
}

impl UserSettings {
    /// The settings as the app sees them; `caller_ids` are the user's.
    pub fn wire(&self, caller_ids: &[CallerId]) -> PhoneSettings {
        let cell_verified = self.cell_number.is_some();
        PhoneSettings {
            default_caller_number_id: self
                .default_caller_number_id
                .filter(|id| caller_ids.iter().any(|caller| caller.number_id == *id)),
            ai_notes: self.ai_notes,
            working_hours: self.working_hours.clone(),
            cell_number: self.cell_number.clone(),
            cell_verified,
            cell_fallback: self.cell_fallback && cell_verified,
            ring_timeout_secs: self.ring_timeout_secs,
            forward_all: self.forward_all,
            forward_all_to: self.forward_all_to,
            forward_all_number: self.forward_all_number.clone(),
            display_number: self.display_number,
            unknown_show_mine: self.unknown_show_mine,
            announce_direct: self.announce_direct,
            announce_shared: self.announce_shared,
            announce_line_name: self.announce_line_name,
            require_pin: self.answer_pin_hash.is_some(),
            hold_music: self.hold_music.clone(),
            hold_music_url: self.hold_music_url.clone(),
            hold_music_name: self.hold_music_name.clone(),
        }
    }

    /// The user's availability at `now`.
    pub fn availability(&self, now: DateTime<Utc>) -> Availability {
        Availability::at(
            now,
            self.dnd_until,
            self.dnd_indefinite,
            self.working_hours.as_ref(),
        )
    }
}

/// The digits of an answer PIN.
pub const PIN_DIGITS: usize = 4;

/// What is stored of a user's answer PIN: SHA-256 hex of
/// `granite-phone-pin:{user id}:{pin}`.
pub fn pin_hash(user_id: i32, pin: &str) -> String {
    crate::auth::sha256_hex(&format!("granite-phone-pin:{user_id}:{pin}"))
}

/// The user saved an answer PIN in `company_id` (whether or not it is
/// required now).
pub async fn has_pin(pool: &MySqlPool, user_id: i32, company_id: i32) -> sqlx::Result<bool> {
    let hash: Option<Option<String>> = sqlx::query_scalar(
        "SELECT answer_pin_hash FROM phone_user_settings WHERE user_id = ? AND company_id = ?",
    )
    .bind(user_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    Ok(hash.flatten().is_some())
}

/// The user's calls say the recording notice to both sides when their
/// recording starts (`recording_voice`; on without a row).
pub async fn recording_voice(
    pool: &MySqlPool,
    user_id: i32,
    company_id: i32,
) -> sqlx::Result<bool> {
    let on: Option<bool> = sqlx::query_scalar(
        "SELECT recording_voice <> 0 FROM phone_user_settings WHERE user_id = ? AND company_id = ?",
    )
    .bind(user_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    Ok(on.unwrap_or(true))
}

/// The notice for the user's calls, as [`recording_voice`]; on (the law's
/// safe side) when it cannot be read.
pub async fn says_recording_notice(pool: &MySqlPool, user_id: i32, company_id: i32) -> bool {
    recording_voice(pool, user_id, company_id)
        .await
        .unwrap_or_else(|err| {
            tracing::warn!(user_id, error = %err, "recording voice setting not read; saying the notice");
            true
        })
}

/// Turns the recording notice of the user's calls on or off.
pub async fn set_recording_voice(
    pool: &MySqlPool,
    user_id: i32,
    company_id: i32,
    on: bool,
) -> sqlx::Result<()> {
    let mut tx = pool.begin().await?;
    ensure_row(&mut tx, user_id, company_id).await?;
    sqlx::query("UPDATE phone_user_settings SET recording_voice = ? WHERE user_id = ?")
        .bind(on)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await
}

/// The PIN `digits` is the user's (`hash` as stored).
pub fn pin_matches(hash: &str, user_id: i32, digits: &str) -> bool {
    digits.len() == PIN_DIGITS
        && crate::auth::constant_time_eq(pin_hash(user_id, digits).as_bytes(), hash.as_bytes())
}

/// The newest version of each notice the user accepted: (recording, 911).
pub async fn accepted_notices(
    pool: &MySqlPool,
    user_id: i32,
) -> sqlx::Result<(Option<i64>, Option<i64>)> {
    sqlx::query_as(
        "SELECT CAST(MAX(CASE WHEN notice = 'recording' THEN version END) AS SIGNED),
                CAST(MAX(CASE WHEN notice = 'e911' THEN version END) AS SIGNED)
           FROM phone_notice_acks WHERE user_id = ?",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await
}
