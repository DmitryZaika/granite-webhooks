//! `GET /v1/phone/bootstrap`: what the app needs at start, on resume after
//! 5 minutes and after a push wake (a documented exception to "one
//! endpoint per read").

use crate::auth::device::DeviceUser;
use crate::error::{ApiResult, ErrorBody};
use crate::openapi::ToSchema;
use crate::phone::availability::Availability;
use crate::phone::calls::{self, Unread};
use crate::phone::company::{self, CompanyConfig, RecordingMode};
use crate::phone::devices::{self, PhoneDevice};
use crate::phone::lines::{self, CallerId};
use crate::phone::settings::{self, E911_NOTICE_VERSION, PhoneSettings, RECORDING_NOTICE_VERSION};
use crate::phone::{SUPPORT_EMAIL, contacts};
use crate::routes::device_sessions::{PhoneUser, phone_user};
use crate::serde_helpers::js_date_required;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use chrono::{DateTime, Utc};
use serde::Serialize;

/// The notices the app shows once per version, and what the user accepted.
// The wire names are the contract's (`recordingVersion` ...).
#[allow(clippy::struct_field_names)]
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Notices {
    /// The recording notice's current version.
    pub recording_version: i64,
    /// The 911 notice's current version.
    pub e911_version: i64,
    /// The newest recording notice the user accepted; null when none (show
    /// it, then `POST /v1/phone/notices/accept`).
    pub accepted_recording_version: Option<i64>,
    /// The newest 911 notice the user accepted; null when none.
    pub accepted_e911_version: Option<i64>,
}

/// The contact index on the server, to tell whether the phone's copy is
/// current.
#[derive(Debug, Serialize, ToSchema)]
pub struct ContactsVersion {
    /// Customers in the index.
    pub count: i64,
    /// The index's current ETag (as `GET /v1/phone/contacts/index` sends
    /// it): when it differs from the phone's, sync.
    pub etag: String,
}

/// `GET /v1/phone/bootstrap`.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Bootstrap {
    /// Our clock (RFC 3339, UTC).
    #[serde(serialize_with = "js_date_required")]
    pub server_time: DateTime<Utc>,
    /// The oldest app version that may call (semver); an older app shows
    /// "Update required" and places no calls. Null = any.
    pub min_app_version: Option<String>,
    /// The company's kill switch: false = no phone of the company can call.
    pub calling_enabled: bool,
    /// Who is signed in.
    pub user: PhoneUser,
    /// This session's phone, once registered (`POST /v1/phone/devices`);
    /// null before.
    pub device: Option<PhoneDevice>,
    /// The numbers the user may call from (direct lines first).
    pub caller_ids: Vec<CallerId>,
    /// The user's phone settings.
    pub settings: PhoneSettings,
    /// Whether the user's phones ring now.
    pub availability: Availability,
    /// Which calls the company records: `off`, `inbound` or `both`.
    pub recording_mode: RecordingMode,
    /// Notice versions and the user's acceptances.
    pub notices: Notices,
    /// The contact index's size and ETag.
    pub contacts: ContactsVersion,
    /// Unseen missed calls, voicemails and AI messages of the last 30 days
    /// in the user's Recents (the Recents badge).
    pub unread: Unread,
    /// The CRM's address, for "Open in CRM" links.
    pub web_base_url: String,
    /// Where users write for help.
    pub support_email: String,
    /// The Texts tab.
    pub texting: Texting,
}

/// The Texts tab's switch and badge.
#[derive(Debug, Serialize, ToSchema)]
pub struct Texting {
    /// The company texts through Telnyx (its messaging profile is set and
    /// calling is on): show the Texts tab.
    pub enabled: bool,
    /// My unread texts (0 while texting is off).
    pub unread: i64,
}

/// Start the phone app
///
/// One call at app start, on resume after 5 minutes and after a push wake:
/// the user, this phone's registration, caller IDs, settings, availability,
/// the company's switches (calling, recording mode, minimum app version),
/// the notices to show, the contact index's version and the unread counts.
/// Device token only. Next: `POST /v1/phone/devices` when `device` is null,
/// `GET /v1/phone/contacts/index` when `contacts.etag` changed.
#[crate::openapi::path(
    get,
    path = "/v1/phone/bootstrap",
    operation_id = "get_phone_bootstrap",
    tag = "phone",
    responses(
        (status = 200, description = "Everything the app needs", body = Bootstrap),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn bootstrap(
    State(state): State<AppState>,
    device: DeviceUser,
    headers: HeaderMap,
) -> ApiResult<Json<Bootstrap>> {
    let user = &device.user;
    let pool = &state.pool;
    if let (Some(device_id), Some(running)) =
        (device.device_id, super::devices::running_version(&headers))
    {
        super::devices::save_running_version(pool, device_id, &running).await?;
    }
    let config = company::load(pool, user.company_id).await?;
    let phone_user = phone_user(pool, user).await?;
    let phone_device = devices::session_device(pool, device.session_id).await?;
    let caller_ids = lines::caller_ids(pool, user.company_id, user.id).await?;
    let stored = settings::load(pool, user.id, user.company_id).await?;
    let (accepted_recording, accepted_e911) = settings::accepted_notices(pool, user.id).await?;
    let coworkers = contacts::coworkers(pool, user.company_id, user.id).await?;
    let version = contacts::version(pool, user.company_id, user.id, &coworkers).await?;
    let unread = calls::unread(pool, user.company_id, user.id).await?;
    let texting_enabled = config.as_ref().is_some_and(CompanyConfig::texting_on);
    let texting = Texting {
        enabled: texting_enabled,
        unread: if texting_enabled {
            crate::phone::texts::unread_total(pool, user.company_id, user.id).await?
        } else {
            0
        },
    };
    let now = Utc::now();
    Ok(Json(Bootstrap {
        server_time: now,
        min_app_version: config.as_ref().map_or_else(
            || state.phone.min_app_version.as_deref().map(str::to_string),
            |config| config.min_app_version(&state.phone),
        ),
        calling_enabled: config.as_ref().is_some_and(CompanyConfig::calling_on),
        user: phone_user,
        device: phone_device,
        settings: stored.wire(&caller_ids),
        caller_ids,
        availability: stored.availability(now),
        recording_mode: config
            .as_ref()
            .map_or(RecordingMode::Inbound, |config| config.recording_mode),
        notices: Notices {
            recording_version: i64::from(RECORDING_NOTICE_VERSION),
            e911_version: i64::from(E911_NOTICE_VERSION),
            accepted_recording_version: accepted_recording,
            accepted_e911_version: accepted_e911,
        },
        contacts: ContactsVersion {
            count: version.count,
            etag: version.etag,
        },
        unread,
        web_base_url: state.app_url.to_string(),
        support_email: SUPPORT_EMAIL.into(),
        texting,
    }))
}
