//! 911 alerts (D24).
//!
//! When a phone of the company dials an emergency or N11 code, every admin
//! of the company (and super admin) but the caller gets an in-app
//! notification and an `e911_alert` push to their signed-in phones. Used by
//! `POST /v1/phone/e911` (the app dialed natively) and by Telnyx's outbound
//! webhook (an emergency call that came through Telnyx anyway).
//!
//! Alerts are deduplicated for 10 minutes per phone (its device session),
//! or per company for a call whose phone proved no identity: within that
//! time a phone's repeat of the same code, or any code after an emergency
//! code, alerts nobody again; an emergency code after another N11 code
//! still does.

use super::devices::push_to_users;
use super::numbers::is_emergency;
use crate::state::AppState;
use sqlx::MySqlPool;
use std::collections::BTreeMap;

/// `positions.id` of a super admin (an admin of the company too).
const SUPER_ADMIN_POSITION: i32 = 9;
/// How long one alert stands for the same phone.
const DEDUPE_MINUTES: i64 = 10;

/// Whose alerts are deduplicated together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertSource {
    /// A signed-in phone (its device session).
    Session(i64),
    /// A call whose phone proved no identity: the company.
    Company,
}

/// The company's live admins and super admins, by id, but `except`.
pub async fn company_admins(
    pool: &MySqlPool,
    company_id: i32,
    except: Option<i32>,
) -> sqlx::Result<Vec<i32>> {
    sqlx::query_scalar(
        "SELECT u.id FROM users u
          WHERE u.company_id = ? AND u.is_deleted = 0 AND COALESCE(u.is_admin, 0) <> 0
            AND NOT (u.id <=> ?)
         UNION
         SELECT u.id FROM users_positions up JOIN users u ON u.id = up.user_id AND u.is_deleted = 0
          WHERE up.company_id = ? AND up.position_id = ? AND NOT (u.id <=> ?)
         ORDER BY 1",
    )
    .bind(company_id)
    .bind(except)
    .bind(company_id)
    .bind(SUPER_ADMIN_POSITION)
    .bind(except)
    .fetch_all(pool)
    .await
}

/// SQL: records the alert of a code (binds: the code, the row's key,
/// whether the code is an emergency) on the row of `table` whose `key` is
/// bound, when it is due: no alert in the last 10 minutes, or only one for
/// a code that is not an emergency while this one is.
fn due_sql(table: &str, key: &str) -> String {
    format!(
        "UPDATE {table} SET e911_alerted_at = UTC_TIMESTAMP(), e911_alert_code = ?
          WHERE {key} = ?
            AND (e911_alerted_at IS NULL
                 OR e911_alerted_at < UTC_TIMESTAMP() - INTERVAL {DEDUPE_MINUTES} MINUTE
                 OR (? AND COALESCE(e911_alert_code, '') NOT IN ('911', '112', '933', '988')))"
    )
}

/// Claims the alert of `code` from `source`: whether it is due (else it
/// repeats one sent in the last 10 minutes).
async fn claim(
    pool: &MySqlPool,
    company_id: i32,
    source: AlertSource,
    code: &str,
) -> sqlx::Result<bool> {
    let (sql, key) = match source {
        AlertSource::Session(id) => (due_sql("device_sessions", "id"), id),
        AlertSource::Company => (
            due_sql("telnyx_company_config", "company_id"),
            i64::from(company_id),
        ),
    };
    let claimed = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(code)
        .bind(key)
        .bind(is_emergency(code))
        .execute(pool)
        .await?
        .rows_affected();
    // A company without a Telnyx setup has no row to throttle on: alert.
    if claimed == 0 && source == AlertSource::Company {
        let configured: Option<i32> =
            sqlx::query_scalar("SELECT company_id FROM telnyx_company_config WHERE company_id = ?")
                .bind(company_id)
                .fetch_optional(pool)
                .await?;
        return Ok(configured.is_none());
    }
    Ok(claimed > 0)
}

/// Tells the company's admins (but `caller`) that `who` dialed `code`.
///
/// An in-app notification each ("{who} dialed {code} on Granite Phone") and
/// an `e911_alert` push, unless `source` set off this alert in the last 10
/// minutes (see the module docs). Whether it went out.
pub async fn alert_e911(
    state: &AppState,
    company_id: i32,
    source: AlertSource,
    caller: Option<i32>,
    who: &str,
    code: &str,
) -> sqlx::Result<bool> {
    let pool = &state.pool;
    if !claim(pool, company_id, source, code).await? {
        return Ok(false);
    }
    let admins = company_admins(pool, company_id, caller).await?;
    let message: String = format!("{who} dialed {code} on Granite Phone")
        .chars()
        .take(255)
        .collect();
    let actor: String = who.chars().take(100).collect();
    let mut tx = pool.begin().await?;
    for admin in &admins {
        sqlx::query(
            "INSERT INTO notifications (user_id, message, notification_type, actor_name, due_at)
             VALUES (?, ?, 'e911_alert', ?, NOW())",
        )
        .bind(admin)
        .bind(&message)
        .bind(&actor)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    let data = BTreeMap::from([
        ("granite".to_string(), "1".to_string()),
        ("type".to_string(), "e911_alert".to_string()),
    ]);
    push_to_users(state, company_id, &admins, &data).await?;
    Ok(true)
}
