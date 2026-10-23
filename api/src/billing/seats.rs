//! Billable users and the extra-user quantity on Stripe.
//!
//! Billable (Anna): the users that count toward the company's user limit
//! ([`COUNTS_TOWARD_LIMIT`]: active, not Check-in-only), except the Walk-in
//! system user and superusers. The base price covers
//! [`super::catalog::INCLUDED_USERS`]; each one above is a unit of the
//! extra-user item. After a user is added, deactivated, restored or changes
//! position the quantity is pushed (prorated) with the idempotency key
//! `seats:<company_id>:<seat_change_seq>`, a counter bumped on every change
//! (never the target quantity: a 2, 3, 2 sequence would replay the first
//! answer). The daily reconcile of `POST /v1/jobs/subscription-maintenance`
//! fixes missed pushes. Legacy companies are never pushed.

use super::catalog::{Catalog, EXTRA_USER_LOOKUP_KEY};
use super::is_ended_subscription_status;
use super::lock::CompanyLock;
use super::stripe::{StripeError, subscription_snapshot};
use crate::state::AppState;
use lambda_http::tracing;
use sqlx::MySqlPool;

macro_rules! counts_toward_limit {
    () => {
        "u.is_deleted = 0 AND (
    EXISTS (SELECT 1 FROM users_positions up WHERE up.user_id = u.id AND up.company_id = u.company_id
                                              AND up.position_id <> 8)
    OR NOT EXISTS (SELECT 1 FROM users_positions up WHERE up.user_id = u.id AND up.company_id = u.company_id))"
    };
}

/// SQL condition on `users u`: counts toward `max_users` (Check-in-only
/// accounts do not).
pub const COUNTS_TOWARD_LIMIT: &str = counts_toward_limit!();

/// SQL condition on `users u`: a billable user.
pub const BILLABLE_USER: &str = concat!(
    counts_toward_limit!(),
    " AND u.is_system = 0 AND u.is_superuser = 0"
);

/// Units of the extra-user item for `billable` users.
pub const fn extra_users(billable: i64) -> i64 {
    let extra = billable - super::catalog::INCLUDED_USERS;
    if extra > 0 { extra } else { 0 }
}

/// What a seat sync found and did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeatSync {
    pub billable_users: i64,
    pub extra_users: i64,
    /// Stripe was changed.
    pub pushed: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum SeatError {
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Lock(#[from] super::lock::LockError),
    #[error(transparent)]
    Stripe(#[from] super::stripe::StripeError),
    #[error(transparent)]
    Catalog(#[from] super::catalog::CatalogError),
}

/// The company's billable users.
pub async fn billable_users(pool: &MySqlPool, company_id: i32) -> sqlx::Result<i64> {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM users u WHERE u.company_id = ? AND {BILLABLE_USER}"
    )))
    .bind(company_id)
    .fetch_one(pool)
    .await
}

#[derive(Debug, sqlx::FromRow)]
struct SeatRow {
    plan_source: String,
    stripe_subscription_id: Option<String>,
    subscription_status: String,
    extra_user_item_id: Option<String>,
    billable_users: Option<i64>,
    seat_change_seq: i64,
}

async fn seat_row(pool: &MySqlPool, company_id: i32) -> sqlx::Result<Option<SeatRow>> {
    sqlx::query_as(
        "SELECT CAST(c.plan_source AS CHAR) AS plan_source, b.stripe_subscription_id,
                b.subscription_status, b.extra_user_item_id,
                CAST(b.billable_users AS SIGNED) AS billable_users,
                CAST(b.seat_change_seq AS SIGNED) AS seat_change_seq
           FROM company c JOIN company_billing b ON b.company_id = c.id
          WHERE c.id = ?",
    )
    .bind(company_id)
    .fetch_optional(pool)
    .await
}

/// A Stripe 400 for an idempotency key reused with other parameters.
fn is_key_reuse(err: &StripeError) -> bool {
    matches!(err, StripeError::Api(message) if message.starts_with("Keys for idempotent requests"))
}

/// What a push changed: the extra-user item afterwards, and whether Stripe
/// was written.
struct Pushed {
    item_id: Option<String>,
    pushed: bool,
}

/// One attempt to bring Stripe to `target` extra users under `key`.
async fn push(
    state: &AppState,
    subscription_id: &str,
    item_id: Option<&str>,
    target: i64,
    key: &str,
) -> Result<Pushed, SeatError> {
    let stripe = &state.stripe;
    let Some(item_id) = item_id else {
        if target == 0 {
            return Ok(Pushed {
                item_id: None,
                pushed: false,
            });
        }
        let live = subscription_snapshot(&stripe.subscription(subscription_id).await?);
        let adopted = live
            .items
            .iter()
            .find(|item| item.lookup_key.as_deref() == Some(EXTRA_USER_LOOKUP_KEY));
        if let Some(item) = adopted {
            if item.quantity == target {
                return Ok(Pushed {
                    item_id: Some(item.id.clone()),
                    pushed: false,
                });
            }
            stripe
                .update_subscription_item_quantity(&item.id, target, key)
                .await?;
            return Ok(Pushed {
                item_id: Some(item.id.clone()),
                pushed: true,
            });
        }
        let catalog = Catalog::load(stripe).await?;
        let created = stripe
            .create_subscription_item(subscription_id, &catalog.extra_user.price_id, target, key)
            .await?;
        let id = created["id"].as_str().map(str::to_string);
        return Ok(Pushed {
            item_id: id,
            pushed: true,
        });
    };
    if target > 0 {
        stripe
            .update_subscription_item_quantity(item_id, target, key)
            .await?;
        return Ok(Pushed {
            item_id: Some(item_id.to_string()),
            pushed: true,
        });
    }
    match stripe.delete_subscription_item(item_id).await {
        Ok(_) | Err(StripeError::Missing(_)) => Ok(Pushed {
            item_id: None,
            pushed: true,
        }),
        Err(err) => Err(err.into()),
    }
}

/// Pushes the company's extra-user quantity to Stripe when it differs.
/// `None`: not a subscription company with a live subscription (nothing to do).
pub async fn sync_seats(state: &AppState, company_id: i32) -> Result<Option<SeatSync>, SeatError> {
    if !state.stripe.is_enabled() {
        return Ok(None);
    }
    let Some(row) = seat_row(&state.pool, company_id).await? else {
        return Ok(None);
    };
    if row.plan_source != "subscription" || row.stripe_subscription_id.is_none() {
        return Ok(None);
    }
    let lock = CompanyLock::acquire(&state.pool, company_id).await?;
    let result = sync_locked(state, company_id).await;
    lock.release().await;
    result
}

async fn sync_locked(state: &AppState, company_id: i32) -> Result<Option<SeatSync>, SeatError> {
    let pool = &state.pool;
    let Some(row) = seat_row(pool, company_id).await? else {
        return Ok(None);
    };
    let Some(subscription_id) = row.stripe_subscription_id.as_deref() else {
        return Ok(None);
    };
    if row.plan_source != "subscription" || is_ended_subscription_status(&row.subscription_status) {
        return Ok(None);
    }
    let count = billable_users(pool, company_id).await?;
    let target = extra_users(count);
    let mut item_id = row.extra_user_item_id.clone();
    let mut seq = row.seat_change_seq;
    let up_to_date = item_id.is_some() && row.billable_users.map(extra_users) == Some(target);
    let mut pushed = false;
    if !up_to_date {
        for attempt in 0..2 {
            let key = format!("seats:{company_id}:{seq}");
            match push(state, subscription_id, item_id.as_deref(), target, &key).await {
                Ok(done) => {
                    item_id = done.item_id;
                    pushed = done.pushed;
                    break;
                }
                Err(SeatError::Stripe(err)) if attempt == 0 && is_key_reuse(&err) => {
                    seq = bump_seq(pool, company_id).await?;
                }
                Err(SeatError::Stripe(StripeError::Missing(_)))
                    if attempt == 0 && item_id.is_some() =>
                {
                    item_id = None;
                    seq = bump_seq(pool, company_id).await?;
                }
                Err(err) => return Err(err),
            }
        }
    }
    sqlx::query("UPDATE company_billing SET billable_users = ?, extra_user_item_id = ? WHERE company_id = ?")
        .bind(count)
        .bind(&item_id)
        .bind(company_id)
        .execute(pool)
        .await?;
    Ok(Some(SeatSync {
        billable_users: count,
        extra_users: target,
        pushed,
    }))
}

async fn bump_seq(pool: &MySqlPool, company_id: i32) -> sqlx::Result<i64> {
    sqlx::query(
        "UPDATE company_billing SET seat_change_seq = seat_change_seq + 1 WHERE company_id = ?",
    )
    .bind(company_id)
    .execute(pool)
    .await?;
    sqlx::query_scalar(
        "SELECT CAST(seat_change_seq AS SIGNED) FROM company_billing WHERE company_id = ?",
    )
    .bind(company_id)
    .fetch_one(pool)
    .await
}

/// After a user change in the company: bumps `seat_change_seq` and syncs.
/// Best effort: failures are logged (the daily reconcile retries).
pub async fn after_user_change(state: &AppState, company_id: i32) {
    let subscription: sqlx::Result<Option<i64>> =
        sqlx::query_scalar("SELECT 1 FROM company WHERE id = ? AND plan_source = 'subscription'")
            .bind(company_id)
            .fetch_optional(&state.pool)
            .await;
    match subscription {
        Ok(Some(_)) => {}
        Ok(None) => return,
        Err(err) => {
            tracing::warn!(company_id, error = %err, "seat change: plan lookup failed");
            return;
        }
    }
    if let Err(err) = bump_seq(&state.pool, company_id).await {
        tracing::warn!(company_id, error = %err, "seat change: counter bump failed");
        return;
    }
    if let Err(err) = sync_seats(state, company_id).await {
        tracing::warn!(company_id, error = %err, "seat sync failed; the daily reconcile retries");
    }
}
