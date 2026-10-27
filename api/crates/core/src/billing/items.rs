//! A subscription's items into the company's add-ons and seat item.
//!
//! Self-serve subscriptions have several items (base, extra users, one per
//! add-on), told apart by their price's lookup key (`catalog::PriceKind`).
//! Each subscription change upserts the add-on items into `company_addons`
//! (source `subscription`), ends the ones no longer there, keeps `comp`
//! rows, and stores the extra-user item in `company_billing.extra_user_item_id`.
//! A legacy single-price subscription has no known lookup key: nothing changes.

use super::catalog::PriceKind;
use super::{SubscriptionSnapshot, is_ended_subscription_status};
use chrono::NaiveDateTime;
use sqlx::{MySqlConnection, MySqlPool};

/// Syncs `snapshot`'s items into the company's rows. The caller holds the
/// company's billing lock.
pub async fn sync_items(
    pool: &MySqlPool,
    company_id: i32,
    snapshot: &SubscriptionSnapshot,
    now: NaiveDateTime,
) -> sqlx::Result<()> {
    let mut tx = pool.begin().await?;
    sync_items_in(&mut tx, company_id, snapshot, now).await?;
    tx.commit().await
}

/// [`sync_items`] inside the caller's transaction.
pub async fn sync_items_in(
    tx: &mut MySqlConnection,
    company_id: i32,
    snapshot: &SubscriptionSnapshot,
    now: NaiveDateTime,
) -> sqlx::Result<()> {
    let kinds: Vec<(PriceKind, &str)> = snapshot
        .items
        .iter()
        .filter_map(|item| {
            let kind = PriceKind::from_lookup_key(item.lookup_key.as_deref()?)?;
            Some((kind, item.id.as_str()))
        })
        .collect();
    if kinds.is_empty() {
        return Ok(());
    }
    if snapshot.deleted || is_ended_subscription_status(&snapshot.status) {
        sqlx::query(
            "UPDATE company_addons SET ended_at = ?
              WHERE company_id = ? AND source = 'subscription' AND ended_at IS NULL",
        )
        .bind(now)
        .bind(company_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE company_billing SET extra_user_item_id = NULL WHERE company_id = ?")
            .bind(company_id)
            .execute(&mut *tx)
            .await?;
        // The subscription really ended: a scheduled cancellation is done.
        sqlx::query(
            "UPDATE billing_cancellation_requests SET status = 'completed'
              WHERE company_id = ? AND status = 'scheduled'",
        )
        .bind(company_id)
        .execute(&mut *tx)
        .await?;
        return Ok(());
    }
    let mut extra_user_item = None;
    let mut kept: Vec<&str> = Vec::new();
    for (kind, item_id) in &kinds {
        match kind {
            PriceKind::Base => {}
            PriceKind::ExtraUser => extra_user_item = Some(*item_id),
            PriceKind::Addon(key) => {
                kept.push(key.as_str());
                sqlx::query(
                    "INSERT INTO company_addons
                       (company_id, addon_key, source, stripe_subscription_item_id, started_at, ended_at)
                     VALUES (?, ?, 'subscription', ?, ?, NULL)
                     ON DUPLICATE KEY UPDATE
                       started_at = IF(ended_at IS NULL, started_at, VALUES(started_at)),
                       stripe_subscription_item_id = VALUES(stripe_subscription_item_id),
                       ended_at = NULL",
                )
                .bind(company_id)
                .bind(key.as_str())
                .bind(item_id)
                .bind(now)
                .execute(&mut *tx)
                .await?;
            }
        }
    }
    let mut ending = String::from(
        "UPDATE company_addons SET ended_at = ?
          WHERE company_id = ? AND source = 'subscription' AND ended_at IS NULL",
    );
    if !kept.is_empty() {
        ending.push_str(" AND addon_key NOT IN (");
        ending.push_str(&vec!["?"; kept.len()].join(", "));
        ending.push(')');
    }
    let mut query = sqlx::query(sqlx::AssertSqlSafe(ending))
        .bind(now)
        .bind(company_id);
    for key in kept {
        query = query.bind(key);
    }
    query.execute(&mut *tx).await?;
    sqlx::query("UPDATE company_billing SET extra_user_item_id = ? WHERE company_id = ?")
        .bind(extra_user_item)
        .bind(company_id)
        .execute(&mut *tx)
        .await?;
    Ok(())
}
