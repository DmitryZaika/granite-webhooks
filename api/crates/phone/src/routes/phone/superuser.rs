//! A company's Telnyx setup (`telnyx_company_config`), as superusers edit
//! it: the kill switch, Telnyx resource ids, limits, and the
//! webhook base to paste into Telnyx. Session cookie only;
//! superusers only.

use super::audit;
use crate::auth::CurrentUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::phone::availability::WorkingHours;
use crate::phone::company::{self, CompanyConfig, RecordingMode};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Most holidays a company may list.
const MAX_HOLIDAYS: usize = 366;
/// The largest daily call limit.
const MAX_DAILY_CALLS: i32 = 1_000_000;

/// A company's Telnyx setup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TelnyxCompanyConfig {
    /// Calling is on: off, no phone of the company can call or ring.
    pub enabled: bool,
    /// The Telnyx credential connection the phones register on (their
    /// calls come to `{webhookBase}/outbound`); null when not set.
    pub credential_connection_id: Option<String>,
    /// The TeXML application of the company's numbers (calls come to
    /// `{webhookBase}/inbound`); null when not set.
    pub texml_app_id: Option<String>,
    /// The Telnyx outbound voice profile; null when not set.
    pub outbound_voice_profile_id: Option<String>,
    /// The Telnyx billing group; null when not set.
    pub billing_group_id: Option<String>,
    /// The company's Telnyx account id (Mission Control's account SID, the
    /// `AccountSid` of Telnyx's TeXML requests). When set, every TeXML
    /// request to the company's webhooks must carry it (403 otherwise); null
    /// = not checked.
    pub telnyx_account_id: Option<String>,
    /// The Telnyx AI Assistant that takes calls nobody answers (numbers
    /// whose no-answer action is `ai`); null = voicemail instead.
    pub ai_assistant_id: Option<String>,
    /// Where callers go when our webhook fails (a TeXML Bin with the AI
    /// Assistant, then voicemail); null = `TELNYX_FALLBACK_TEXML_URL`.
    pub fallback_texml_url: Option<String>,
    /// Which calls are recorded: `off`, `inbound` or `both`.
    pub recording_mode: RecordingMode,
    /// The notice played to the customer on recorded outgoing calls (an
    /// audio file URL), before the call is bridged; null = "This call may be
    /// recorded." is said to both sides once the customer answers, before
    /// the recording starts.
    pub recording_notice_url: Option<String>,
    /// The Telnyx messaging profile the company's numbers text through (its
    /// webhook URL is `{webhookBase}/messaging`); texting is on while calling
    /// is on and this is set; null = no texting through Telnyx.
    pub messaging_profile_id: Option<String>,
    /// When the main line rings (its `tz` is the company time zone); null =
    /// always.
    pub business_hours: Option<WorkingHours>,
    /// Dates the main line rings nobody, `YYYY-MM-DD` in the company time
    /// zone, sorted.
    pub holidays: Vec<String>,
    /// The oldest app version that may call (e.g. `1.4.0`); null =
    /// `PHONE_MIN_APP_VERSION`.
    pub min_app_version: Option<String>,
    /// Outgoing calls one user may start per day.
    pub daily_call_limit: i32,
    /// The root of the company's Telnyx webhooks, to paste into Telnyx:
    /// `https://api.../v1/webhooks/telnyx/<token>` (`/inbound` and
    /// `/call-status` on the TeXML app, `/outbound` on the credential
    /// connection). Read-only.
    pub webhook_base: String,
}

impl TelnyxCompanyConfig {
    pub(super) fn of(config: &CompanyConfig, state: &AppState) -> Self {
        Self {
            enabled: config.enabled,
            credential_connection_id: config.credential_connection_id.clone(),
            texml_app_id: config.texml_app_id.clone(),
            outbound_voice_profile_id: config.outbound_voice_profile_id.clone(),
            billing_group_id: config.billing_group_id.clone(),
            telnyx_account_id: config.telnyx_account_id.clone(),
            ai_assistant_id: config.ai_assistant_id.clone(),
            fallback_texml_url: config.fallback_texml_url.clone(),
            recording_mode: config.recording_mode,
            recording_notice_url: config.recording_notice_url.clone(),
            messaging_profile_id: config.messaging_profile_id.clone(),
            business_hours: config.business_hours.clone(),
            holidays: config.holidays.clone(),
            min_app_version: config.min_app_version.clone(),
            daily_call_limit: config.daily_call_limit,
            webhook_base: config.webhook_base(&state.phone),
        }
    }
}

/// Body of `PUT /v1/superuser/companies/{id}/telnyx`: the whole setup (as
/// `TelnyxCompanyConfig`, without `webhookBase`). Blank text counts as
/// null.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TelnyxConfigInput {
    /// Calling is on.
    pub enabled: bool,
    /// Telnyx credential connection id (at most 64 characters, no spaces).
    #[serde(default)]
    pub credential_connection_id: Option<String>,
    /// Telnyx TeXML application id (at most 64 characters, no spaces).
    #[serde(default)]
    pub texml_app_id: Option<String>,
    /// Telnyx outbound voice profile id (at most 64 characters, no spaces).
    #[serde(default)]
    pub outbound_voice_profile_id: Option<String>,
    /// Telnyx billing group id (at most 64 characters, no spaces).
    #[serde(default)]
    pub billing_group_id: Option<String>,
    /// The Telnyx account id (at most 64 characters, no spaces); null or
    /// blank = TeXML requests are not checked for it.
    #[serde(default)]
    pub telnyx_account_id: Option<String>,
    /// Telnyx AI Assistant id (at most 128 characters, no spaces).
    #[serde(default)]
    pub ai_assistant_id: Option<String>,
    /// The fallback TeXML URL (`http(s)://`, at most 512 characters).
    #[serde(default)]
    pub fallback_texml_url: Option<String>,
    /// Telnyx messaging profile id (at most 64 characters, no spaces); null
    /// or blank = no texting through Telnyx.
    #[serde(default)]
    pub messaging_profile_id: Option<String>,
    /// A version like `1.4.0` (at most 16 characters).
    #[serde(default)]
    pub min_app_version: Option<String>,
    /// 0 to 1,000,000 outgoing calls per user per day.
    pub daily_call_limit: i32,
}

/// The setup as stored, once checked (the audit log's details too).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CheckedConfig {
    enabled: bool,
    credential_connection_id: Option<String>,
    texml_app_id: Option<String>,
    outbound_voice_profile_id: Option<String>,
    billing_group_id: Option<String>,
    telnyx_account_id: Option<String>,
    ai_assistant_id: Option<String>,
    fallback_texml_url: Option<String>,
    messaging_profile_id: Option<String>,
    min_app_version: Option<String>,
    daily_call_limit: i32,
}

impl TelnyxConfigInput {
    /// The setup as stored: 400 with a message for what is wrong.
    fn checked(self) -> ApiResult<CheckedConfig> {
        if !(0..=MAX_DAILY_CALLS).contains(&self.daily_call_limit) {
            return Err(ApiError::BadRequest(
                "The daily call limit is 0 to 1,000,000 calls.".into(),
            ));
        }
        Ok(CheckedConfig {
            enabled: self.enabled,
            credential_connection_id: telnyx_id(
                self.credential_connection_id.as_ref(),
                64,
                "credential connection id",
            )?,
            texml_app_id: telnyx_id(self.texml_app_id.as_ref(), 64, "TeXML application id")?,
            outbound_voice_profile_id: telnyx_id(
                self.outbound_voice_profile_id.as_ref(),
                64,
                "outbound voice profile id",
            )?,
            billing_group_id: telnyx_id(self.billing_group_id.as_ref(), 64, "billing group id")?,
            telnyx_account_id: telnyx_id(self.telnyx_account_id.as_ref(), 64, "Telnyx account id")?,
            ai_assistant_id: telnyx_id(self.ai_assistant_id.as_ref(), 128, "AI Assistant id")?,
            fallback_texml_url: http_url(self.fallback_texml_url.as_ref(), "fallback TeXML")?,
            messaging_profile_id: telnyx_id(
                self.messaging_profile_id.as_ref(),
                64,
                "messaging profile id",
            )?,
            min_app_version: app_version(self.min_app_version.as_ref())?,
            daily_call_limit: self.daily_call_limit,
        })
    }
}

/// 403 unless the caller is a superuser.
const fn superuser(user: &crate::auth::session::SessionUser) -> ApiResult<()> {
    if user.is_superuser {
        Ok(())
    } else {
        Err(ApiError::Forbidden)
    }
}

/// 404 unless the company exists (and is not deleted).
async fn company_exists(state: &AppState, id: i32) -> ApiResult<()> {
    let found: Option<i32> =
        sqlx::query_scalar("SELECT id FROM company WHERE id = ? AND deleted_at IS NULL")
            .bind(id)
            .fetch_optional(&state.pool)
            .await?;
    found.map(|_| ()).ok_or(ApiError::NotFound)
}

/// Blank text as `None`.
fn blank_none(value: Option<&String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// A Telnyx id: at most `max` characters, no spaces or control characters.
fn telnyx_id(value: Option<&String>, max: usize, what: &str) -> ApiResult<Option<String>> {
    let value = blank_none(value);
    if value.as_deref().is_some_and(|id| {
        id.chars().count() > max || id.chars().any(|c| c.is_whitespace() || c.is_control())
    }) {
        return Err(ApiError::BadRequest(format!(
            "The {what} is at most {max} characters, without spaces."
        )));
    }
    Ok(value)
}

/// An `http(s)://` URL of at most 512 characters.
pub(super) fn http_url(value: Option<&String>, what: &str) -> ApiResult<Option<String>> {
    let value = blank_none(value);
    let bad = value.as_deref().is_some_and(|text| {
        text.chars().count() > 512
            || !(text.starts_with("https://") || text.starts_with("http://"))
            || url::Url::parse(text).is_err()
    });
    if bad {
        return Err(ApiError::BadRequest(format!(
            "Enter a full URL for the {what}, starting with https://."
        )));
    }
    Ok(value)
}

/// A version like `1.4.0` or `2.0.0-beta.1`.
fn app_version(value: Option<&String>) -> ApiResult<Option<String>> {
    let value = blank_none(value);
    let good = value.as_deref().is_none_or(|version| {
        version.chars().count() <= 16
            && version.starts_with(|c: char| c.is_ascii_digit())
            && version
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'))
    });
    if !good {
        return Err(ApiError::BadRequest(
            "Use an app version like 1.4.0 (at most 16 characters).".into(),
        ));
    }
    Ok(value)
}

/// Business hours as stored (the time zone trimmed): 400 with a message for
/// what is wrong.
pub(super) fn checked_hours(hours: Option<WorkingHours>) -> ApiResult<Option<WorkingHours>> {
    let Some(hours) = hours else {
        return Ok(None);
    };
    if let Some(problem) = hours.problem() {
        return Err(ApiError::BadRequest(problem));
    }
    Ok(Some(WorkingHours {
        tz: hours.tz.trim().to_string(),
        days: hours.days,
    }))
}

/// The holidays as stored: real `YYYY-MM-DD` dates, sorted, once each.
pub(super) fn checked_holidays(dates: &[String]) -> ApiResult<Vec<String>> {
    let mut checked = Vec::with_capacity(dates.len());
    for date in dates {
        let date = date.trim();
        let real = NaiveDate::parse_from_str(date, "%Y-%m-%d")
            .is_ok_and(|parsed| parsed.format("%Y-%m-%d").to_string() == date);
        if !real {
            return Err(ApiError::BadRequest(format!(
                "Holidays are YYYY-MM-DD dates (got {}).",
                date.chars().take(20).collect::<String>()
            )));
        }
        checked.push(date.to_string());
    }
    checked.sort();
    checked.dedup();
    if checked.len() > MAX_HOLIDAYS {
        return Err(ApiError::BadRequest(format!(
            "At most {MAX_HOLIDAYS} holidays."
        )));
    }
    Ok(checked)
}

/// A new webhook token: 32 random hex characters (128 bits).
pub(super) fn new_webhook_token() -> ApiResult<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|err| ApiError::Internal(err.to_string()))?;
    Ok(bytes
        .iter()
        .fold(String::with_capacity(32), |mut out, byte| {
            use std::fmt::Write as _;
            let _ = write!(out, "{byte:02x}");
            out
        }))
}

/// Get a company's Telnyx setup
///
/// The company's Granite Phone calling setup: the kill switch, Telnyx
/// resource ids, recording mode and notice, the main line's business hours
/// and holidays, the minimum app version, the daily call limit, and
/// `webhookBase`, the root of the company's Telnyx webhooks to paste into
/// the Telnyx portal. 404 when the company has no setup yet (save one with
/// `PUT`) or does not exist. Superusers only.
#[crate::openapi::path(
    get,
    path = "/v1/superuser/companies/{id}/telnyx",
    operation_id = "get_company_telnyx_config",
    tag = "phone",
    params(("id" = i32, Path, description = "Company id")),
    responses(
        (status = 200, description = "The setup", body = TelnyxCompanyConfig),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a superuser", body = ErrorBody),
        (status = 404, description = "No such company, or no Telnyx setup saved yet", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn company_telnyx_config(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<TelnyxCompanyConfig>> {
    superuser(&user)?;
    company_exists(&state, id).await?;
    let config = company::load(&state.pool, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(TelnyxCompanyConfig::of(&config, &state)))
}

/// Save a company's Telnyx setup
///
/// Saves the whole setup and returns it: every field of
/// `TelnyxCompanyConfig` but the read-only `webhookBase` and what the
/// company's admins decide (`businessHours` and `holidays` with `PUT
/// /v1/phone/admin/hours`, `recordingMode` and `recordingNoticeUrl` with
/// `PUT /v1/phone/admin/recording`); blank text is null. The first save makes
/// the company's webhook token (32 random characters, kept from then on),
/// so `webhookBase` is known from then on. A `telnyxAccountId` makes every
/// TeXML request to the company's webhooks carry that `AccountSid` (403
/// otherwise). Checks: Telnyx ids without spaces (64 characters, the AI
/// Assistant's 128), `http(s)://` URLs,
/// a version like `1.4.0`,
/// a daily limit of 0 to 1,000,000; 400 with a message otherwise. Written
/// to the phone audit log. Superusers only.
#[crate::openapi::path(
    put,
    path = "/v1/superuser/companies/{id}/telnyx",
    operation_id = "save_company_telnyx_config",
    tag = "phone",
    params(("id" = i32, Path, description = "Company id")),
    request_body = TelnyxConfigInput,
    responses(
        (status = 200, description = "The setup as saved", body = TelnyxCompanyConfig),
        (status = 400, description = "What is wrong", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a superuser", body = ErrorBody),
        (status = 404, description = "No such company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn save_company_telnyx_config(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<TelnyxConfigInput>,
) -> ApiResult<Json<TelnyxCompanyConfig>> {
    superuser(&user)?;
    let checked = input.checked()?;
    company_exists(&state, id).await?;
    let mut tx = state.pool.begin().await?;
    // A company's first save makes its row, and so its webhook token.
    let created = sqlx::query(
        "INSERT IGNORE INTO telnyx_company_config (company_id, webhook_token) VALUES (?, ?)",
    )
    .bind(id)
    .bind(new_webhook_token()?)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    let saved = sqlx::query(
        "UPDATE telnyx_company_config
            SET enabled = ?, credential_connection_id = ?, texml_app_id = ?,
                outbound_voice_profile_id = ?, billing_group_id = ?, telnyx_account_id = ?,
                ai_assistant_id = ?, fallback_texml_url = ?, messaging_profile_id = ?,
                min_app_version = ?,
                daily_call_limit = ?
          WHERE company_id = ?",
    )
    .bind(checked.enabled)
    .bind(&checked.credential_connection_id)
    .bind(&checked.texml_app_id)
    .bind(&checked.outbound_voice_profile_id)
    .bind(&checked.billing_group_id)
    .bind(&checked.telnyx_account_id)
    .bind(&checked.ai_assistant_id)
    .bind(&checked.fallback_texml_url)
    .bind(&checked.messaging_profile_id)
    .bind(&checked.min_app_version)
    .bind(checked.daily_call_limit)
    .bind(id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if saved == 0 {
        return Err(ApiError::Internal("the Telnyx setup was not saved".into()));
    }
    let mut details = serde_json::to_value(&checked).unwrap_or_default();
    details["created"] = json!(created);
    audit(
        &mut *tx,
        id,
        user.id,
        "telnyx_config_saved",
        &format!("company:{id}"),
        &details,
    )
    .await?;
    tx.commit().await?;
    let config = company::load(&state.pool, id)
        .await?
        .ok_or_else(|| ApiError::Internal("the Telnyx setup was not saved".into()))?;
    Ok(Json(TelnyxCompanyConfig::of(&config, &state)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webhook_tokens_are_32_random_hex_characters() {
        let a = new_webhook_token().unwrap();
        let b = new_webhook_token().unwrap();
        assert_eq!(a.len(), 32);
        assert!(
            a.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_ne!(a, b);
    }

    #[test]
    fn holidays_are_real_dates_sorted_once() {
        let dates = |list: &[&str]| list.iter().map(ToString::to_string).collect::<Vec<_>>();
        assert_eq!(
            checked_holidays(&dates(&["2026-12-25", " 2026-01-01 ", "2026-12-25"])).unwrap(),
            ["2026-01-01", "2026-12-25"]
        );
        for bad in ["2026-02-30", "2026-1-5", "12/25/2026", ""] {
            assert!(checked_holidays(&dates(&[bad])).is_err(), "{bad}");
        }
    }

    #[test]
    fn ids_urls_and_versions_are_checked_lightly() {
        let text = |value: &str| Some(value.to_string());
        assert_eq!(
            telnyx_id(text(" 1234 ").as_ref(), 64, "x")
                .unwrap()
                .as_deref(),
            Some("1234")
        );
        assert_eq!(telnyx_id(text("  ").as_ref(), 64, "x").unwrap(), None);
        assert!(telnyx_id(text("12 34").as_ref(), 64, "x").is_err());
        assert!(telnyx_id(text(&"a".repeat(65)).as_ref(), 64, "x").is_err());
        assert!(http_url(text("https://bin.test/a?b=1").as_ref(), "x").is_ok());
        assert!(http_url(text("ftp://bin.test").as_ref(), "x").is_err());
        assert!(http_url(text("https://").as_ref(), "x").is_err());
        assert!(app_version(text("1.4.0").as_ref()).is_ok());
        assert!(app_version(text("2.0.0-beta.1").as_ref()).is_ok());
        assert!(app_version(text("v1").as_ref()).is_err());
        assert!(app_version(text("1.0.0.0.0.0.0.0.0").as_ref()).is_err());
    }
}
