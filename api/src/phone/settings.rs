//! A user's phone settings (`phone_user_settings`; a user without a row has
//! the defaults) and the notices they accepted (`phone_notice_acks`).
//!
//! Settings belong to the user in one company: a row left from another
//! company reads as the defaults and is replaced on the next write.

use super::availability::{Availability, WorkingHours};
use super::lines::CallerId;
use super::wire_enum;
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{FromRow, MySql, MySqlPool, Transaction};
use utoipa::ToSchema;

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
}

/// The user's settings in `company_id` (the defaults when there are none).
pub async fn load(pool: &MySqlPool, user_id: i32, company_id: i32) -> sqlx::Result<UserSettings> {
    let row: Option<SettingsRow> = sqlx::query_as(
        "SELECT default_caller_number_id, ai_notes <> 0 AS ai_notes, dnd_until,
                dnd_indefinite <> 0 AS dnd_indefinite, CAST(working_hours AS CHAR) AS working_hours,
                CASE WHEN cell_verified_at IS NOT NULL THEN cell_number END AS cell_number,
                cell_fallback <> 0 AS cell_fallback,
                CAST(ring_timeout_secs AS SIGNED) AS ring_timeout_secs
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
