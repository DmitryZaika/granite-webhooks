//! Phones (`phone_devices`, one per device session): their Telnyx
//! telephony credential and our own pushes to them.

use super::company::CompanyConfig;
use crate::openapi::ToSchema;
use crate::push::{Delivery, PushError};
use crate::serde_helpers::js_date_required;
use crate::state::AppState;
use crate::telnyx::TelnyxError;
use chrono::{DateTime, Duration, Utc};
use futures_util::future::join_all;
use lambda_http::tracing;
use serde::Serialize;
use sqlx::{FromRow, MySqlPool};
use std::collections::BTreeMap;
use std::time::Duration as StdDuration;

/// A Telnyx credential works this long after it is created or extended.
pub const CREDENTIAL_DAYS: i64 = 30;

/// SQL condition: the phone `pd` and its device session `ds` are signed in
/// (not revoked, not expired, used in the last 30 days); a browser phone
/// also needs its web session (the CRM's sign-in) to be live, by the rule of
/// `auth::session::find_session_user` (60 days = `SESSION_MAX_AGE_SECONDS`).
pub const LIVE_DEVICE: &str = "pd.revoked_at IS NULL AND ds.revoked_at IS NULL
     AND ds.expires_at > UTC_TIMESTAMP()
     AND COALESCE(ds.last_seen_at, ds.created_at) > UTC_TIMESTAMP() - INTERVAL 30 DAY
     AND (ds.web_session_id IS NULL OR EXISTS (
          SELECT 1 FROM sessions ws
           WHERE ws.id = ds.web_session_id AND ws.is_deleted = 0
             AND ws.expiration_date > CURRENT_TIMESTAMP
             AND (ws.expiration_date <= CURRENT_TIMESTAMP + INTERVAL 60 DAY
                  OR ws.created_date > CURRENT_TIMESTAMP - INTERVAL 2 MONTH)))";

/// This session's phone, as the app sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PhoneDevice {
    /// The phone device: use it in `/v1/phone/devices/{id}/...`.
    pub id: i64,
    /// The phone's SIP user on Telnyx (`gencred...`); null while calling is
    /// off for the company or its credential is not made yet.
    pub sip_username: Option<String>,
    /// We have the phone's FCM token (for missed-call and other pushes).
    pub push_token_set: bool,
    /// When the phone was registered.
    #[serde(serialize_with = "js_date_required")]
    pub created_at: DateTime<Utc>,
}

/// The live phone device of a device session.
pub async fn session_device(
    pool: &MySqlPool,
    session_id: i64,
) -> sqlx::Result<Option<PhoneDevice>> {
    sqlx::query_as(
        "SELECT CAST(id AS SIGNED) AS id, sip_username, push_token IS NOT NULL AS push_token_set,
                created_at
           FROM phone_devices WHERE device_session_id = ? AND revoked_at IS NULL",
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await
}

/// Why a phone has no Telnyx credential.
#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    /// Telnyx refused or failed.
    #[error(transparent)]
    Telnyx(#[from] TelnyxError),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

/// The Telnyx credential of a device that has one: (credential id, SIP user).
pub async fn credential_of(
    pool: &MySqlPool,
    device_id: i64,
) -> sqlx::Result<Option<(String, String)>> {
    let row: Option<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT telnyx_credential_id, sip_username FROM phone_devices
          WHERE id = ? AND revoked_at IS NULL",
    )
    .bind(device_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.and_then(|(id, sip)| id.zip(sip)))
}

/// Gives the phone a Telnyx credential on the company's credential
/// connection (working 30 days), unless it has one or calling is off. The
/// credential when there is one: (credential id, SIP user).
pub async fn ensure_credential(
    state: &AppState,
    config: Option<&CompanyConfig>,
    device_id: i64,
) -> Result<Option<(String, String)>, CredentialError> {
    if let Some(existing) = credential_of(&state.pool, device_id).await? {
        return Ok(Some(existing));
    }
    let Some((config, connection_id)) = config
        .filter(|config| config.calling_on())
        .and_then(|config| Some((config, config.credential_connection_id.as_deref()?)))
    else {
        return Ok(None);
    };
    if !state.telnyx.is_enabled() {
        tracing::warn!(
            company_id = config.company_id,
            "TELNYX_API_KEY is not set; phone left without a credential"
        );
        return Ok(None);
    }
    let expires_at = Utc::now() + Duration::days(CREDENTIAL_DAYS);
    let credential = state
        .telnyx
        .create_credential(
            connection_id,
            &format!("phone-{device_id}"),
            &format!("company-{}", config.company_id),
            expires_at,
        )
        .await?;
    let saved = sqlx::query(
        "UPDATE phone_devices
            SET telnyx_credential_id = ?, sip_username = ?, credential_expires_at = ?,
                credential_deleted_at = NULL
          WHERE id = ? AND telnyx_credential_id IS NULL AND revoked_at IS NULL",
    )
    .bind(&credential.id)
    .bind(&credential.sip_username)
    .bind(expires_at)
    .bind(device_id)
    .execute(&state.pool)
    .await?
    .rows_affected();
    if saved == 0 {
        // Another request was faster (or the phone was revoked meanwhile):
        // this credential is not needed.
        if let Err(err) = state.telnyx.delete_credential(&credential.id).await {
            tracing::warn!(device_id, error = %err, "spare Telnyx credential not deleted");
        }
        return Ok(credential_of(&state.pool, device_id).await?);
    }
    Ok(Some((credential.id, credential.sip_username)))
}

/// Gives `push_token` to one phone: no other phone keeps it (a token belongs
/// to one app install, so an older sign-in on the same phone loses it).
pub async fn set_push_token(
    pool: &MySqlPool,
    device_id: i64,
    push_token: &str,
) -> sqlx::Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE phone_devices SET push_token = NULL, push_token_updated_at = UTC_TIMESTAMP()
          WHERE push_token = ? AND id <> ?",
    )
    .bind(push_token)
    .bind(device_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE phone_devices SET push_token = ?, push_token_updated_at = UTC_TIMESTAMP()
          WHERE id = ? AND revoked_at IS NULL",
    )
    .bind(push_token)
    .bind(device_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

/// A phone to push to.
#[derive(FromRow)]
struct PushTarget {
    id: i64,
    push_token: String,
}

/// How long all the pushes of one event may take together: they are sent
/// while Telnyx waits for our TeXML answer (or the app for its response).
pub const PUSH_BUDGET: StdDuration = StdDuration::from_secs(2);

/// [`PUSH_BUDGET`] unless set with `AppState::with_extension` (tests
/// shorten it).
#[derive(Clone, Copy, Debug)]
pub struct PushBudget(pub StdDuration);

impl Default for PushBudget {
    fn default() -> Self {
        Self(PUSH_BUDGET)
    }
}

/// One push to one phone: whether FCM accepted it. A phone whose app is
/// gone (`UNREGISTERED`) loses its token; other failures are logged.
async fn push_one(state: &AppState, target: &PushTarget, data: &BTreeMap<String, String>) -> bool {
    match state.push.send(&target.push_token, data, true).await {
        Ok(Delivery::Sent) => true,
        Ok(Delivery::Unregistered) => {
            let cleared = sqlx::query(
                "UPDATE phone_devices SET push_token = NULL, push_token_updated_at = UTC_TIMESTAMP()
                  WHERE id = ? AND push_token = ?",
            )
            .bind(target.id)
            .bind(&target.push_token)
            .execute(&state.pool)
            .await;
            if let Err(err) = cleared {
                tracing::warn!(device_id = target.id, error = %err, "gone push token not cleared");
            }
            false
        }
        Err(PushError::NotConfigured) => false,
        Err(err) => {
            tracing::warn!(device_id = target.id, error = %err, "push not sent");
            false
        }
    }
}

/// Pushes `data` to the signed-in phones of `user_ids` in `company_id`.
///
/// `data` holds string values, e.g. `{granite: "1", type: "missed_call",
/// callId: "42"}`, sent as a high-priority push to every such phone with a
/// push token, all at once and for [`PUSH_BUDGET`] at most (pushes still
/// out then are dropped and logged). A phone whose app is gone
/// (`UNREGISTERED`) loses its token; other failures are logged. The number
/// of phones FCM accepted it for.
pub async fn push_to_users(
    state: &AppState,
    company_id: i32,
    user_ids: &[i32],
    data: &BTreeMap<String, String>,
) -> sqlx::Result<usize> {
    if user_ids.is_empty() || !state.push.is_enabled() {
        return Ok(0);
    }
    let mut query = sqlx::QueryBuilder::new(format!(
        "SELECT CAST(pd.id AS SIGNED) AS id, pd.push_token
           FROM phone_devices pd
           JOIN device_sessions ds ON ds.id = pd.device_session_id
          WHERE pd.push_token IS NOT NULL AND {LIVE_DEVICE} AND pd.company_id = "
    ));
    query.push_bind(company_id);
    crate::sql::push_in(&mut query, "pd.user_id", user_ids);
    query.push(" ORDER BY pd.id");
    let targets: Vec<PushTarget> = query.build_query_as().fetch_all(&state.pool).await?;
    let sends = join_all(targets.iter().map(|target| push_one(state, target, data)));
    let budget = state.extensions.get_or_default::<PushBudget>().0;
    let sent = tokio::time::timeout(budget, sends).await.map_or_else(
        |_| {
            tracing::warn!(
                phones = targets.len(),
                "pushes not all sent within the time allowed"
            );
            0
        },
        |results| results.into_iter().filter(|sent| *sent).count(),
    );
    Ok(sent)
}
