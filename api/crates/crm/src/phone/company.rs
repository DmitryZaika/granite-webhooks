//! A company's Telnyx setup (`telnyx_company_config`): the kill switch,
//! Telnyx resource ids, recording mode, business hours and limits.

use super::availability::WorkingHours;
use super::clock::time_zone;
use super::{PhoneConfig, wire_enum};
use chrono_tz::Tz;
use sqlx::{FromRow, MySqlPool};

/// The company time zone when its business hours name none.
pub const DEFAULT_TIME_ZONE: Tz = chrono_tz::America::Indiana::Indianapolis;

wire_enum! {
    /// Which calls are recorded: `off`, `inbound` (the default) or `both`.
    pub enum RecordingMode {
        /// No call is recorded.
        Off = "off",
        /// Incoming calls are recorded (callers hear a notice first).
        Inbound = "inbound",
        /// Incoming and outgoing calls are recorded.
        Both = "both",
    }
}

/// A company's Telnyx setup.
#[derive(Debug, Clone)]
pub struct CompanyConfig {
    pub company_id: i32,
    /// The kill switch: off, no phone of the company can call.
    pub enabled: bool,
    /// The company itself is deleted (`company.deleted_at`): nothing but an
    /// emergency call goes through, whatever `enabled` says.
    pub company_deleted: bool,
    /// Names the company in Telnyx's callback URLs.
    pub webhook_token: String,
    pub credential_connection_id: Option<String>,
    pub texml_app_id: Option<String>,
    pub outbound_voice_profile_id: Option<String>,
    pub billing_group_id: Option<String>,
    pub telnyx_account_id: Option<String>,
    pub ai_assistant_id: Option<String>,
    pub fallback_texml_url: Option<String>,
    pub recording_mode: RecordingMode,
    pub recording_notice_url: Option<String>,
    /// The Telnyx messaging profile the company's numbers text through;
    /// texting is on when this is set and calling is on.
    pub messaging_profile_id: Option<String>,
    /// Hours of the main line; their `tz` is the company time zone.
    pub business_hours: Option<WorkingHours>,
    /// `YYYY-MM-DD` dates the main line is closed.
    pub holidays: Vec<String>,
    pub min_app_version: Option<String>,
    /// Outgoing calls a user may start per (local) day.
    pub daily_call_limit: i32,
}

#[derive(FromRow)]
struct ConfigRow {
    company_id: i32,
    enabled: bool,
    company_deleted: bool,
    webhook_token: String,
    credential_connection_id: Option<String>,
    texml_app_id: Option<String>,
    outbound_voice_profile_id: Option<String>,
    billing_group_id: Option<String>,
    telnyx_account_id: Option<String>,
    ai_assistant_id: Option<String>,
    fallback_texml_url: Option<String>,
    recording_mode: String,
    recording_notice_url: Option<String>,
    messaging_profile_id: Option<String>,
    business_hours: Option<String>,
    holidays: Option<String>,
    min_app_version: Option<String>,
    daily_call_limit: i32,
}

const COLUMNS: &str = "SELECT company_id, enabled <> 0 AS enabled,
        COALESCE((SELECT co.deleted_at IS NOT NULL FROM company co
                   WHERE co.id = telnyx_company_config.company_id), 1) AS company_deleted,
        webhook_token,
        credential_connection_id, texml_app_id, outbound_voice_profile_id, billing_group_id,
        telnyx_account_id, ai_assistant_id, fallback_texml_url,
        CAST(recording_mode AS CHAR) AS recording_mode, recording_notice_url,
        messaging_profile_id,
        CAST(business_hours AS CHAR) AS business_hours, CAST(holidays AS CHAR) AS holidays,
        min_app_version, daily_call_limit
   FROM telnyx_company_config";

/// A blank text column as `None`.
fn text(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

impl From<ConfigRow> for CompanyConfig {
    fn from(row: ConfigRow) -> Self {
        Self {
            company_id: row.company_id,
            enabled: row.enabled,
            company_deleted: row.company_deleted,
            webhook_token: row.webhook_token,
            credential_connection_id: text(row.credential_connection_id),
            texml_app_id: text(row.texml_app_id),
            outbound_voice_profile_id: text(row.outbound_voice_profile_id),
            billing_group_id: text(row.billing_group_id),
            telnyx_account_id: text(row.telnyx_account_id),
            ai_assistant_id: text(row.ai_assistant_id),
            fallback_texml_url: text(row.fallback_texml_url),
            recording_mode: RecordingMode::parse(&row.recording_mode)
                .unwrap_or(RecordingMode::Inbound),
            recording_notice_url: text(row.recording_notice_url),
            messaging_profile_id: text(row.messaging_profile_id),
            business_hours: row
                .business_hours
                .as_deref()
                .and_then(WorkingHours::from_json),
            holidays: row
                .holidays
                .as_deref()
                .and_then(|json| serde_json::from_str(json).ok())
                .unwrap_or_default(),
            min_app_version: text(row.min_app_version),
            daily_call_limit: row.daily_call_limit,
        }
    }
}

/// The company's Telnyx setup; `None` when it has none (calling is off).
pub async fn load(pool: &MySqlPool, company_id: i32) -> sqlx::Result<Option<CompanyConfig>> {
    let row: Option<ConfigRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{COLUMNS} WHERE company_id = ?"
    )))
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(CompanyConfig::from))
}

/// The setup of the company whose callback URLs carry `token`.
pub async fn by_webhook_token(
    pool: &MySqlPool,
    token: &str,
) -> sqlx::Result<Option<CompanyConfig>> {
    let row: Option<ConfigRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{COLUMNS} WHERE webhook_token = ?"
    )))
    .bind(token)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(CompanyConfig::from))
}

impl CompanyConfig {
    /// The root of this company's Telnyx callbacks:
    /// `{PHONE_PUBLIC_API_URL}/v1/webhooks/telnyx/{webhook_token}`.
    pub fn webhook_base(&self, phone: &PhoneConfig) -> String {
        format!(
            "{}/v1/webhooks/telnyx/{}",
            phone.public_api_url, self.webhook_token
        )
    }

    /// Calling is switched on and the company is not deleted.
    pub const fn calling_on(&self) -> bool {
        self.enabled && !self.company_deleted
    }

    /// Calling is on ([`Self::calling_on`]) and the TeXML app our server
    /// calls through is set.
    pub const fn can_place_calls(&self) -> bool {
        self.calling_on() && self.texml_app_id.is_some()
    }

    /// Texting through Telnyx is on: calling is on ([`Self::calling_on`])
    /// and the messaging profile is set.
    pub const fn texting_on(&self) -> bool {
        self.calling_on() && self.messaging_profile_id.is_some()
    }

    /// The oldest app version that may call: the company's, else
    /// `PHONE_MIN_APP_VERSION`.
    pub fn min_app_version(&self, phone: &PhoneConfig) -> Option<String> {
        self.min_app_version
            .clone()
            .or_else(|| phone.min_app_version.as_deref().map(str::to_string))
    }
}

/// The company time zone: its business hours' `tz`, else
/// [`DEFAULT_TIME_ZONE`] (also for a company without a setup).
pub fn time_zone_of(config: Option<&CompanyConfig>) -> Tz {
    config
        .and_then(|config| config.business_hours.as_ref())
        .and_then(|hours| time_zone(&hours.tz))
        .unwrap_or(DEFAULT_TIME_ZONE)
}
