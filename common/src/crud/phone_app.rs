//! Who has the Granite Phone app: their notifications go to the app, not to
//! Telegram.
//!
//! A user has the app in a company while an Android or iPhone of theirs is
//! signed in there (the API's `LIVE_DEVICE`) and has its push token, so our
//! pushes reach it. A failed lookup counts as no app: Telegram still goes.

use sqlx::{MySql, MySqlPool, QueryBuilder};
use std::collections::HashSet;

/// SQL condition: the app phone `pd` (device session `ds`) can take our pushes.
const APP_PHONE: &str = "pd.platform IN ('android', 'ios') AND pd.push_token IS NOT NULL
     AND pd.revoked_at IS NULL AND ds.revoked_at IS NULL
     AND ds.expires_at > UTC_TIMESTAMP()
     AND COALESCE(ds.last_seen_at, ds.created_at) > UTC_TIMESTAMP() - INTERVAL 30 DAY";

/// The users of `user_ids` with the app in `company_id`.
pub async fn users_with_app(pool: &MySqlPool, company_id: i32, user_ids: &[i32]) -> HashSet<i32> {
    if user_ids.is_empty() {
        return HashSet::new();
    }
    let mut query = QueryBuilder::<MySql>::new(format!(
        "SELECT DISTINCT pd.user_id FROM phone_devices pd
           JOIN device_sessions ds ON ds.id = pd.device_session_id
          WHERE {APP_PHONE} AND pd.company_id = "
    ));
    query.push_bind(company_id).push(" AND pd.user_id IN (");
    let mut list = query.separated(", ");
    for user_id in user_ids {
        list.push_bind(*user_id);
    }
    query.push(")");
    match query.build_query_scalar::<i32>().fetch_all(pool).await {
        Ok(ids) => ids.into_iter().collect(),
        Err(error) => {
            tracing::warn!(?error, company_id, "app phones not looked up");
            HashSet::new()
        }
    }
}

/// Whether `user_id` has the app in their own company, where their bell
/// notifications are pushed.
pub async fn has_app(pool: &MySqlPool, user_id: i32) -> bool {
    let found = sqlx::query_scalar::<_, i32>(sqlx::AssertSqlSafe(format!(
        "SELECT 1 FROM phone_devices pd
           JOIN device_sessions ds ON ds.id = pd.device_session_id
           JOIN users u ON u.id = pd.user_id AND u.company_id = pd.company_id
          WHERE {APP_PHONE} AND pd.user_id = ? LIMIT 1"
    )))
    .bind(user_id)
    .fetch_optional(pool)
    .await;
    match found {
        Ok(found) => found.is_some(),
        Err(error) => {
            tracing::warn!(?error, user_id, "app phones not looked up");
            false
        }
    }
}
