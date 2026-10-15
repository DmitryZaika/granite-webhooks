//! The event's history log (`event_history`). Port of
//! `app/lib/eventHistory.server.ts`.

use lambda_http::tracing;
use sqlx::MySqlPool;

/// One history line.
pub struct History<'a> {
    event_id: i32,
    company_id: i32,
    action: &'static str,
    created_by: Option<&'a str>,
    message: Option<&'a str>,
}

impl<'a> History<'a> {
    pub const fn new(
        event_id: i32,
        company_id: i32,
        action: &'static str,
        created_by: Option<&'a str>,
    ) -> Self {
        Self {
            event_id,
            company_id,
            action,
            created_by,
            message: None,
        }
    }

    /// Instead of the action's default text.
    #[must_use]
    pub const fn message(mut self, message: &'a str) -> Self {
        self.message = Some(message);
        self
    }
}

fn default_message(action: &str) -> &'static str {
    match action {
        "created" => "Event created",
        "date_updated" => "Date updated",
        "label_updated" => "Label updated",
        "location_updated" => "Location updated",
        "attachment_added" => "Attachment added",
        "attachment_removed" => "Attachment removed",
        "comment_added" => "Comment added",
        _ => "Event updated",
    }
}

/// Adds a line; a failure is only logged (history never blocks a save).
pub async fn record(pool: &MySqlPool, line: &History<'_>) {
    let result = sqlx::query(
        "INSERT INTO event_history (event_id, company_id, action, message, created_by) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(line.event_id)
    .bind(line.company_id)
    .bind(line.action)
    .bind(line.message.unwrap_or_else(|| default_message(line.action)))
    .bind(line.created_by)
    .execute(pool)
    .await;
    if let Err(err) = result {
        tracing::warn!(error = %err, event_id = line.event_id, "event history not saved");
    }
}
