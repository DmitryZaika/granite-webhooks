use sqlx::MySqlPool;
use sqlx::mysql::MySqlQueryResult;

use crate::telegram::crm::TELEGRAM_SENT_MARKER;

pub struct DueActivityDeadlineReminder {
    pub id: u64,
    pub user_id: i32,
    pub deal_id: u64,
    pub message: String,
    pub customer_name: Option<String>,
    pub telegram_id: Option<i64>,
}

pub async fn get_due_activity_deadline_reminders(
    pool: &MySqlPool,
) -> Result<Vec<DueActivityDeadlineReminder>, sqlx::Error> {
    sqlx::query_as!(
        DueActivityDeadlineReminder,
        r#"
        SELECT
            n.id AS "id!",
            n.user_id,
            n.deal_id AS "deal_id!",
            n.message,
            c.name AS customer_name,
            u.telegram_id
        FROM notifications n
        JOIN deals d ON d.id = n.deal_id AND d.deleted_at IS NULL
        JOIN customers c ON c.id = d.customer_id
        JOIN users u ON u.id = n.user_id
        WHERE n.notification_type = 'activity_deadline_reminder'
          AND n.is_done = 0
          AND u.telegram_activity_notifications = TRUE
          AND (n.actor_name IS NULL OR n.actor_name != ?)
          AND n.due_at <= UTC_TIMESTAMP()
          AND EXISTS (
            SELECT 1 FROM deal_activities da
            WHERE da.deal_id = n.deal_id
              AND da.deleted_at IS NULL
              AND da.is_completed = 0
              AND LEFT(da.name, 255) = n.message
          )
        "#,
        TELEGRAM_SENT_MARKER
    )
    .fetch_all(pool)
    .await
}

pub async fn mark_deadline_reminder_telegram_sent(
    pool: &MySqlPool,
    notification_id: u64,
) -> Result<MySqlQueryResult, sqlx::Error> {
    sqlx::query!(
        r#"
        UPDATE notifications
        SET actor_name = ?
        WHERE id = ?
          AND notification_type = 'activity_deadline_reminder'
        "#,
        TELEGRAM_SENT_MARKER,
        notification_id
    )
    .execute(pool)
    .await
}
