//! Phone pushes of what the Telegram bots also send.
//!
//! - `notification`: a `notifications` row of the bell (activity, note and
//!   comment changes on a deal, activity deadline and appointment
//!   reminders); the app reads it with `GET /v1/notifications/{id}`
//! - `lead`: lead alerts (a new lead for the managers, a repeated lead, a
//!   lead assigned to its rep, a finished online quote); the app reads the
//!   customer with `GET /v1/customers/{id}` and opens the deal
//! - `maintenance`: the shop's maintenance checks due today
//!
//! Ids only, like every Granite push (never names, messages or numbers).
//! Telegram's on/off settings are Telegram's: a phone gets these whether or
//! not its user linked Telegram, as it does new email.

use super::calls::SALES_MANAGER_POSITION;
use super::devices::push_to_users;
use crate::state::AppState;
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use sqlx::MySqlPool;
use std::collections::BTreeMap;
use crate::openapi::ToSchema;

/// A push's data: `{granite: "1", type: kind}` and `fields`.
fn data(kind: &str, fields: Vec<(&str, String)>) -> BTreeMap<String, String> {
    let mut data = BTreeMap::from([
        ("granite".to_string(), "1".to_string()),
        ("type".to_string(), kind.to_string()),
    ]);
    data.extend(
        fields
            .into_iter()
            .map(|(key, value)| (key.to_string(), value)),
    );
    data
}

/// Pushes `notifications` row `notification_id` to its user's phones as
/// `{granite: "1", type: "notification", notificationId}`. The number of
/// phones told; 0 for an unknown row or a deleted user.
pub async fn push_notification(state: &AppState, notification_id: i64) -> sqlx::Result<usize> {
    if !state.push.is_enabled() {
        return Ok(0);
    }
    let owner: Option<(i32, i32)> = sqlx::query_as(
        "SELECT n.user_id, u.company_id FROM notifications n
           JOIN users u ON u.id = n.user_id AND u.is_deleted = 0
          WHERE n.id = ?",
    )
    .bind(notification_id)
    .fetch_optional(&state.pool)
    .await?;
    let Some((user_id, company_id)) = owner else {
        return Ok(0);
    };
    let data = data(
        "notification",
        vec![("notificationId", notification_id.to_string())],
    );
    push_to_users(state, company_id, &[user_id], &data).await
}

/// [`push_notification`], logged instead of returned: the notification is
/// already saved.
pub async fn push_notification_logged(state: &AppState, notification_id: i64) {
    if let Err(error) = push_notification(state, notification_id).await {
        tracing::warn!(?error, notification_id, "notification push failed");
    }
}

/// What a lead push is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum LeadKind {
    /// A new lead, for the managers to assign.
    New,
    /// A lead that came in again.
    Repeat,
    /// A lead a manager assigned to this rep.
    Assigned,
    /// A customer finished the online quote.
    Quote,
}

impl LeadKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Repeat => "repeat",
            Self::Assigned => "assigned",
            Self::Quote => "quote",
        }
    }
}

/// The company's live sales managers, by id.
pub async fn sales_managers(pool: &MySqlPool, company_id: i32) -> sqlx::Result<Vec<i32>> {
    sqlx::query_scalar(
        "SELECT DISTINCT u.id FROM users u
           JOIN users_positions up ON up.user_id = u.id AND up.company_id = u.company_id
          WHERE u.company_id = ? AND u.is_deleted = 0 AND up.position_id = ?
          ORDER BY u.id",
    )
    .bind(company_id)
    .bind(SALES_MANAGER_POSITION)
    .fetch_all(pool)
    .await
}

/// Pushes lead news about customer `customer_id` of `company_id` to the
/// phones of `user_ids` as `{granite: "1", type: "lead", kind, customerId,
/// dealId}`: `dealId` is `deal_id`, else the customer's newest live deal,
/// and left out when there is none (a lead nobody was assigned yet). The
/// number of phones told; 0 for another company's customer.
pub async fn push_lead(
    state: &AppState,
    company_id: i32,
    user_ids: &[i32],
    kind: LeadKind,
    customer_id: i32,
    deal_id: Option<i64>,
) -> sqlx::Result<usize> {
    if user_ids.is_empty() || !state.push.is_enabled() {
        return Ok(0);
    }
    let customer: Option<i32> = sqlx::query_scalar(
        "SELECT id FROM customers WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(customer_id)
    .bind(company_id)
    .fetch_optional(&state.pool)
    .await?;
    if customer.is_none() {
        return Ok(0);
    }
    let deal_id = match deal_id {
        Some(id) => Some(id),
        None => {
            sqlx::query_scalar(
                "SELECT CAST(id AS SIGNED) FROM deals
                  WHERE customer_id = ? AND deleted_at IS NULL ORDER BY id DESC LIMIT 1",
            )
            .bind(customer_id)
            .fetch_optional(&state.pool)
            .await?
        }
    };
    let mut fields = vec![
        ("kind", kind.as_str().to_string()),
        ("customerId", customer_id.to_string()),
    ];
    if let Some(deal_id) = deal_id {
        fields.push(("dealId", deal_id.to_string()));
    }
    push_to_users(state, company_id, user_ids, &data("lead", fields)).await
}

/// [`push_lead`], logged instead of returned: the lead is already saved.
pub async fn push_lead_logged(
    state: &AppState,
    company_id: i32,
    user_ids: &[i32],
    kind: LeadKind,
    customer_id: i32,
    deal_id: Option<i64>,
) {
    if let Err(error) = push_lead(state, company_id, user_ids, kind, customer_id, deal_id).await {
        tracing::warn!(?error, company_id, customer_id, "lead push failed");
    }
}

/// Pushes the maintenance checks due today to `user_id`'s phones as
/// `{granite: "1", type: "maintenance", count}`. The number of phones told.
pub async fn push_maintenance(
    state: &AppState,
    company_id: i32,
    user_id: i32,
    count: usize,
) -> sqlx::Result<usize> {
    let data = data("maintenance", vec![("count", count.to_string())]);
    push_to_users(state, company_id, &[user_id], &data).await
}
