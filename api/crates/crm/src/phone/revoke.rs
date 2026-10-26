//! Signing phones out: the device session, its phone device, the push token
//! and the phone's Telnyx credential.
//!
//! The database changes commit first, in one transaction, so the token stops
//! working at once (every request re-checks its device session). Then, best
//! effort, the phone gets a `revoked` push (unless it signed itself out) and
//! its Telnyx credential is deleted. A credential that could not be deleted
//! keeps `credential_deleted_at` NULL; the phone job deletes it later.
//!
//! Called by sign-out, `DELETE /v1/me/devices/{id}` and the admin's revoke,
//! by password resets and changes, by the admin user delete and save (a
//! user who lost phone access), by the superuser's company delete, and by
//! the phone job, which also signs out the phones of users who left by any
//! other way and the sign-ins that expired or idled.

use super::routing::PHONE_USER;
use crate::auth::device::IDLE_DAYS;
use crate::push::PushError;
use crate::state::AppState;
use lambda_http::tracing;
use sqlx::{FromRow, MySqlPool};
use std::collections::BTreeMap;

/// Why phones are signed out (`device_sessions.revoked_reason`,
/// `phone_devices.revoke_reason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevokeReason {
    /// The phone signed itself out (`DELETE /v1/auth/device-sessions/current`).
    SignedOut,
    /// Its user signed it out from another device or the CRM.
    ByUser,
    /// A company admin signed it out.
    ByAdmin,
    /// The user's password was changed or reset.
    PasswordChanged,
    /// The user was deleted, deactivated or lost phone access.
    UserRemoved,
    /// The company was deleted.
    CompanyDeleted,
    /// The sign-in expired (12 months) or went unused for 30 days.
    Expired,
}

impl RevokeReason {
    /// As stored.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SignedOut => "signed_out",
            Self::ByUser => "user_revoked",
            Self::ByAdmin => "admin_revoked",
            Self::PasswordChanged => "password_changed",
            Self::UserRemoved => "user_removed",
            Self::CompanyDeleted => "company_deleted",
            Self::Expired => "expired",
        }
    }

    /// The phone is told (a `revoked` push) so the app wipes itself, unless
    /// it is the one signing out.
    const fn notifies_phone(self) -> bool {
        !matches!(self, Self::SignedOut)
    }
}

/// A phone device being signed out, read before its push token is cleared.
#[derive(Debug, FromRow)]
struct RevokedDevice {
    id: i64,
    /// The Telnyx credential still to delete, if any.
    credential_id: Option<String>,
    push_token: Option<String>,
}

const DEVICE_COLUMNS: &str = "CAST(id AS SIGNED) AS id,
       CASE WHEN credential_deleted_at IS NULL THEN telnyx_credential_id END AS credential_id,
       CASE WHEN revoked_at IS NULL THEN push_token END AS push_token";

/// Signs one device session out, with its phone device. False when it was
/// not live (unknown or revoked already); nothing changes then.
pub async fn revoke_device_session(
    state: &AppState,
    session_id: i64,
    reason: RevokeReason,
) -> Result<bool, sqlx::Error> {
    let mut tx = state.pool.begin().await?;
    let revoked = sqlx::query(
        "UPDATE device_sessions SET revoked_at = UTC_TIMESTAMP(), revoked_reason = ?
          WHERE id = ? AND revoked_at IS NULL",
    )
    .bind(reason.as_str())
    .bind(session_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if revoked == 0 {
        tx.rollback().await?;
        return Ok(false);
    }
    let devices: Vec<RevokedDevice> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {DEVICE_COLUMNS} FROM phone_devices WHERE device_session_id = ? FOR UPDATE"
    )))
    .bind(session_id)
    .fetch_all(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE phone_devices
            SET revoked_at = COALESCE(revoked_at, UTC_TIMESTAMP()),
                revoke_reason = COALESCE(revoke_reason, ?),
                push_token = NULL, push_token_updated_at = UTC_TIMESTAMP()
          WHERE device_session_id = ?",
    )
    .bind(reason.as_str())
    .bind(session_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    finish(state, &devices, reason).await;
    Ok(true)
}

/// Signs every phone of a user out (password changed or reset, user
/// deleted, deactivated or demoted), and retries the user's credential
/// deletes that failed before. The number of device sessions ended.
pub async fn revoke_user_phones(
    state: &AppState,
    user_id: i32,
    reason: RevokeReason,
) -> Result<u64, sqlx::Error> {
    let mut tx = state.pool.begin().await?;
    let sessions = sqlx::query(
        "UPDATE device_sessions SET revoked_at = UTC_TIMESTAMP(), revoked_reason = ?
          WHERE user_id = ? AND revoked_at IS NULL",
    )
    .bind(reason.as_str())
    .bind(user_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    let devices: Vec<RevokedDevice> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {DEVICE_COLUMNS} FROM phone_devices
          WHERE user_id = ?
            AND (revoked_at IS NULL
                 OR (telnyx_credential_id IS NOT NULL AND credential_deleted_at IS NULL))
          FOR UPDATE"
    )))
    .bind(user_id)
    .fetch_all(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE phone_devices
            SET revoked_at = UTC_TIMESTAMP(), revoke_reason = ?,
                push_token = NULL, push_token_updated_at = UTC_TIMESTAMP()
          WHERE user_id = ? AND revoked_at IS NULL",
    )
    .bind(reason.as_str())
    .bind(user_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    finish(state, &devices, reason).await;
    Ok(sessions)
}

/// Signs out every phone of a company (it was deleted). The number of
/// device sessions ended.
pub async fn revoke_company_phones(
    state: &AppState,
    company_id: i32,
    reason: RevokeReason,
) -> Result<u64, sqlx::Error> {
    let mut tx = state.pool.begin().await?;
    let sessions = sqlx::query(
        "UPDATE device_sessions SET revoked_at = UTC_TIMESTAMP(), revoked_reason = ?
          WHERE company_id = ? AND revoked_at IS NULL",
    )
    .bind(reason.as_str())
    .bind(company_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    let devices: Vec<RevokedDevice> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {DEVICE_COLUMNS} FROM phone_devices
          WHERE company_id = ?
            AND (revoked_at IS NULL
                 OR (telnyx_credential_id IS NOT NULL AND credential_deleted_at IS NULL))
          FOR UPDATE"
    )))
    .bind(company_id)
    .fetch_all(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE phone_devices
            SET revoked_at = UTC_TIMESTAMP(), revoke_reason = ?,
                push_token = NULL, push_token_updated_at = UTC_TIMESTAMP()
          WHERE company_id = ? AND revoked_at IS NULL",
    )
    .bind(reason.as_str())
    .bind(company_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    finish(state, &devices, reason).await;
    Ok(sessions)
}

/// Device sessions whose user may no longer use Granite Phone.
///
/// Sessions not signed out (of `user_id`, else anyone's) whose user is
/// deleted, moved to another company, or no longer an employee, admin,
/// superuser or super admin, or whose company is deleted. At most `limit`,
/// oldest first.
pub async fn lost_access_sessions(
    pool: &MySqlPool,
    user_id: Option<i32>,
    limit: i64,
) -> sqlx::Result<Vec<i64>> {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT CAST(ds.id AS SIGNED) FROM device_sessions ds
           LEFT JOIN users u ON u.id = ds.user_id
           LEFT JOIN company co ON co.id = ds.company_id
          WHERE ds.revoked_at IS NULL AND (? IS NULL OR ds.user_id = ?)
            AND NOT (u.id IS NOT NULL AND u.is_deleted = 0 AND u.company_id = ds.company_id
                     AND co.id IS NOT NULL AND co.deleted_at IS NULL AND {PHONE_USER})
          ORDER BY ds.id
          LIMIT ?"
    )))
    .bind(user_id)
    .bind(user_id)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Device sessions not signed out that expired (12 months) or went unused
/// for 30 days, and browser phones whose web session (the CRM sign-in)
/// ended: they no longer sign in, and their phones' credentials and push
/// tokens are to go. At most `limit`, oldest first.
pub async fn expired_sessions(pool: &MySqlPool, limit: i64) -> sqlx::Result<Vec<i64>> {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT CAST(ds.id AS SIGNED) FROM device_sessions ds
          WHERE ds.revoked_at IS NULL
            AND (ds.expires_at <= UTC_TIMESTAMP()
                 OR COALESCE(ds.last_seen_at, ds.created_at)
                    <= UTC_TIMESTAMP() - INTERVAL {IDLE_DAYS} DAY
                 OR (ds.web_session_id IS NOT NULL AND NOT EXISTS (
                      SELECT 1 FROM sessions ws
                       WHERE ws.id = ds.web_session_id AND ws.is_deleted = 0
                         AND ws.expiration_date > CURRENT_TIMESTAMP)))
          ORDER BY ds.id
          LIMIT ?"
    )))
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Signs out the browser phones of web session `web_session_id` (the CRM
/// signed out). The number of device sessions ended.
pub async fn revoke_web_session(
    state: &AppState,
    web_session_id: &str,
    reason: RevokeReason,
) -> Result<u64, sqlx::Error> {
    let sessions: Vec<i64> = sqlx::query_scalar(
        "SELECT CAST(id AS SIGNED) FROM device_sessions
          WHERE web_session_id = ? AND revoked_at IS NULL",
    )
    .bind(web_session_id)
    .fetch_all(&state.pool)
    .await?;
    let mut ended = 0;
    for session_id in sessions {
        if revoke_device_session(state, session_id, reason).await? {
            ended += 1;
        }
    }
    Ok(ended)
}

/// Counts a failed delete of the phone's Telnyx credential, for the phone
/// job's back-off.
pub async fn credential_delete_failed(pool: &MySqlPool, device_id: i64) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE phone_devices
            SET credential_delete_attempts = LEAST(credential_delete_attempts + 1, 65535),
                credential_delete_tried_at = UTC_TIMESTAMP()
          WHERE id = ?",
    )
    .bind(device_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Signs out the user's phones when the user lost phone access.
///
/// Every phone of the user that may no longer be used (see
/// [`lost_access_sessions`]), e.g. after an admin removed the user's
/// employee and admin rights or moved them to another company. The number
/// of device sessions ended.
pub async fn revoke_if_lost_access(state: &AppState, user_id: i32) -> Result<u64, sqlx::Error> {
    let mut ended = 0;
    for session_id in lost_access_sessions(&state.pool, Some(user_id), 1000).await? {
        if revoke_device_session(state, session_id, RevokeReason::UserRemoved).await? {
            ended += 1;
        }
    }
    Ok(ended)
}

/// After the commit: tell the phones and delete their Telnyx credentials.
async fn finish(state: &AppState, devices: &[RevokedDevice], reason: RevokeReason) {
    for device in devices {
        if reason.notifies_phone()
            && let Some(token) = &device.push_token
        {
            let data = BTreeMap::from([
                ("granite".to_string(), "1".to_string()),
                ("type".to_string(), "revoked".to_string()),
            ]);
            match state.push.send(token, &data, true).await {
                Ok(_) | Err(PushError::NotConfigured) => {}
                Err(err) => {
                    tracing::warn!(device_id = device.id, error = %err, "revoked push not sent");
                }
            }
        }
        let Some(credential_id) = &device.credential_id else {
            continue;
        };
        match state.telnyx.delete_credential(credential_id).await {
            Ok(()) => {
                let marked = sqlx::query(
                    "UPDATE phone_devices SET credential_deleted_at = UTC_TIMESTAMP() WHERE id = ?",
                )
                .bind(device.id)
                .execute(&state.pool)
                .await;
                if let Err(err) = marked {
                    tracing::warn!(
                        device_id = device.id,
                        ?err,
                        "credential delete not recorded"
                    );
                }
            }
            Err(err) => {
                tracing::warn!(
                    device_id = device.id,
                    error = %err,
                    "Telnyx credential not deleted; the phone job retries"
                );
                if let Err(err) = credential_delete_failed(&state.pool, device.id).await {
                    tracing::warn!(device_id = device.id, ?err, "failed delete not recorded");
                }
            }
        }
    }
}
