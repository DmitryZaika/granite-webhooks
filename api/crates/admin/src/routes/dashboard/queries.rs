use super::schemas::{DashboardWidget, WidgetAction};
use sqlx::{MySql, MySqlPool, Transaction};

/// The saved layout, in order. Ids the API does not know (retired widgets)
/// are skipped.
pub async fn saved_layout(
    pool: &MySqlPool,
    user_id: i32,
    company_id: i32,
) -> Result<Vec<DashboardWidget>, sqlx::Error> {
    let ids: Vec<String> = sqlx::query_scalar(
        r"
        SELECT widget_id FROM user_dashboard_widgets
         WHERE user_id = ? AND company_id = ?
         ORDER BY position, id
        ",
    )
    .bind(user_id)
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    Ok(ids
        .iter()
        .filter_map(|id| DashboardWidget::from_id(id))
        .collect())
}

/// Replaces the saved layout, recording added and removed widgets.
///
/// One transaction; each added and removed widget becomes an analytics
/// event, except on the first save. Nothing is written when the layout is
/// unchanged. Remix: the
/// dashboard's `set-widgets` action and `saveUserDashboardWidgets`.
pub async fn replace_layout(
    pool: &MySqlPool,
    user_id: i32,
    company_id: i32,
    next: &[DashboardWidget],
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    let current: Vec<String> = sqlx::query_scalar(
        r"
        SELECT widget_id FROM user_dashboard_widgets
         WHERE user_id = ? AND company_id = ?
         ORDER BY position, id
         FOR UPDATE
        ",
    )
    .bind(user_id)
    .bind(company_id)
    .fetch_all(&mut *tx)
    .await?;
    if current
        .iter()
        .map(String::as_str)
        .eq(next.iter().map(|w| w.id()))
    {
        return tx.commit().await;
    }

    sqlx::query("DELETE FROM user_dashboard_widgets WHERE user_id = ? AND company_id = ?")
        .bind(user_id)
        .bind(company_id)
        .execute(&mut *tx)
        .await?;
    for (position, widget) in (0_i32..).zip(next) {
        sqlx::query(
            "INSERT INTO user_dashboard_widgets (user_id, company_id, widget_id, position)
             VALUES (?, ?, ?, ?)",
        )
        .bind(user_id)
        .bind(company_id)
        .bind(widget.id())
        .bind(position)
        .execute(&mut *tx)
        .await?;
    }

    // A first save has nothing to compare with (the page showed defaults the
    // API never saw), so it records no adds or removes.
    if current.is_empty() {
        return tx.commit().await;
    }
    for widget in next {
        if !current.iter().any(|id| id == widget.id()) {
            insert_event(
                &mut tx,
                user_id,
                company_id,
                widget.id(),
                WidgetAction::Add,
                "add",
            )
            .await?;
        }
    }
    for id in &current {
        if !next.iter().any(|widget| widget.id() == id) {
            insert_event(
                &mut tx,
                user_id,
                company_id,
                id,
                WidgetAction::Remove,
                "remove",
            )
            .await?;
        }
    }
    tx.commit().await
}

async fn insert_event(
    tx: &mut Transaction<'_, MySql>,
    user_id: i32,
    company_id: i32,
    widget_id: &str,
    action: WidgetAction,
    target: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO widget_analytics_events (user_id, company_id, widget_id, action, target)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(user_id)
    .bind(company_id)
    .bind(widget_id)
    .bind(action.as_str())
    .bind(target)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Remix: `insertWidgetAnalytics`.
pub async fn record_event(
    pool: &MySqlPool,
    user_id: i32,
    company_id: i32,
    widget: DashboardWidget,
    action: WidgetAction,
    target: &str,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    insert_event(&mut tx, user_id, company_id, widget.id(), action, target).await?;
    tx.commit().await
}
