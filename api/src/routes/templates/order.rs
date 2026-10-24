//! Per-user order of the template pickers (`user_template_order_mode`,
//! `user_template_order`, `user_template_usage`).
//!
//! Automatic (default): most used first, ties in the list's normal order.
//! Custom: the saved drag order; templates without a saved position follow
//! in the normal order, so new templates never jump into an arranged list.

use super::schemas::{OrderingMode, OrderingType};
use sqlx::MySqlPool;
use std::cmp::Ordering;
use std::collections::HashMap;

/// What sorting one picker for one user needs.
#[derive(Debug, Default)]
pub struct OrderingContext {
    pub mode: OrderingMode,
    pub positions: HashMap<i32, i32>,
    pub usage: HashMap<i32, i64>,
}

pub async fn mode(
    pool: &MySqlPool,
    user_id: i32,
    ordering: OrderingType,
) -> Result<OrderingMode, sqlx::Error> {
    let mode: Option<String> = sqlx::query_scalar(
        "SELECT mode FROM user_template_order_mode WHERE user_id = ? AND ordering_type = ?",
    )
    .bind(user_id)
    .bind(ordering.as_str())
    .fetch_optional(pool)
    .await?;
    Ok(if mode.as_deref() == Some("custom") {
        OrderingMode::Custom
    } else {
        OrderingMode::Automatic
    })
}

pub async fn context(
    pool: &MySqlPool,
    user_id: i32,
    ordering: OrderingType,
) -> Result<OrderingContext, sqlx::Error> {
    let positions: Vec<(i32, i32)> = sqlx::query_as(
        "SELECT template_id, custom_position FROM user_template_order
         WHERE user_id = ? AND ordering_type = ?",
    )
    .bind(user_id)
    .bind(ordering.as_str())
    .fetch_all(pool)
    .await?;
    let usage: Vec<(i32, i64)> = match ordering {
        // Quote emails record their template and sender already.
        OrderingType::Quote => {
            sqlx::query_as(
                "SELECT template_id, CAST(COUNT(*) AS SIGNED) FROM quote_emails
                 WHERE sent_by = ? AND template_id IS NOT NULL GROUP BY template_id",
            )
            .bind(user_id)
            .fetch_all(pool)
            .await?
        }
        OrderingType::Sms | OrderingType::Email => {
            let types = if ordering == OrderingType::Sms {
                ("cloudtalk_sms", "ringcentral_sms", "telnyx_sms")
            } else {
                ("email", "email", "email")
            };
            sqlx::query_as(
                "SELECT template_id, CAST(SUM(usage_count) AS SIGNED) FROM user_template_usage
                 WHERE user_id = ? AND template_type IN (?, ?, ?) GROUP BY template_id",
            )
            .bind(user_id)
            .bind(types.0)
            .bind(types.1)
            .bind(types.2)
            .fetch_all(pool)
            .await?
        }
    };
    Ok(OrderingContext {
        mode: mode(pool, user_id, ordering).await?,
        positions: positions.into_iter().collect(),
        usage: usage.into_iter().collect(),
    })
}

/// Sorts `items` by the user's preference; `fallback` is the list's
/// normal order.
pub fn sort<T>(
    items: &mut [T],
    id: impl Fn(&T) -> i32,
    context: &OrderingContext,
    fallback: impl Fn(&T, &T) -> Ordering,
) {
    match context.mode {
        OrderingMode::Custom => items.sort_by(|a, b| {
            match (context.positions.get(&id(a)), context.positions.get(&id(b))) {
                (Some(x), Some(y)) => x.cmp(y),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => fallback(a, b),
            }
        }),
        OrderingMode::Automatic => items.sort_by(|a, b| {
            let used = |item: &T| context.usage.get(&id(item)).copied().unwrap_or(0);
            used(b).cmp(&used(a)).then_with(|| fallback(a, b))
        }),
    }
}

/// Saves the drag order (the whole visible list) and switches to custom.
pub async fn save_custom(
    pool: &MySqlPool,
    user_id: i32,
    ordering: OrderingType,
    template_ids: &[i32],
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO user_template_order_mode (user_id, ordering_type, mode)
         VALUES (?, ?, 'custom') ON DUPLICATE KEY UPDATE mode = 'custom'",
    )
    .bind(user_id)
    .bind(ordering.as_str())
    .execute(&mut *tx)
    .await?;
    for (position, template_id) in template_ids.iter().enumerate() {
        sqlx::query(
            "INSERT INTO user_template_order (user_id, ordering_type, template_id, custom_position)
             VALUES (?, ?, ?, ?) ON DUPLICATE KEY UPDATE custom_position = VALUES(custom_position)",
        )
        .bind(user_id)
        .bind(ordering.as_str())
        .bind(template_id)
        .bind(i32::try_from(position).unwrap_or(i32::MAX))
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await
}

/// Drops the saved order and switches back to automatic.
pub async fn reset(
    pool: &MySqlPool,
    user_id: i32,
    ordering: OrderingType,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM user_template_order WHERE user_id = ? AND ordering_type = ?")
        .bind(user_id)
        .bind(ordering.as_str())
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO user_template_order_mode (user_id, ordering_type, mode)
         VALUES (?, ?, 'automatic') ON DUPLICATE KEY UPDATE mode = 'automatic'",
    )
    .bind(user_id)
    .bind(ordering.as_str())
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(items: &[(i32, i32)]) -> Vec<i32> {
        items.iter().map(|(id, _)| *id).collect()
    }

    #[test]
    fn automatic_puts_most_used_first() {
        let mut items = vec![(1, 0), (2, 1), (3, 2)];
        let context = OrderingContext {
            usage: [(3, 5), (2, 5), (1, 1)].into_iter().collect(),
            ..OrderingContext::default()
        };
        sort(&mut items, |item| item.0, &context, |a, b| a.1.cmp(&b.1));
        assert_eq!(ids(&items), [2, 3, 1]);
    }

    #[test]
    fn custom_keeps_unplaced_templates_last() {
        let mut items = vec![(1, 0), (2, 1), (3, 2), (4, 3)];
        let context = OrderingContext {
            mode: OrderingMode::Custom,
            positions: [(3, 0), (1, 1)].into_iter().collect(),
            ..OrderingContext::default()
        };
        sort(&mut items, |item| item.0, &context, |a, b| a.1.cmp(&b.1));
        assert_eq!(ids(&items), [3, 1, 2, 4]);
    }
}
