pub const TELEGRAM_SENT_MARKER: &str = "__telegram_sent__";

pub fn notification_type_title(notification_type: &str) -> &'static str {
    match notification_type {
        "activity_added" => "Added an Activity",
        "activity_edited" => "Edited an Activity",
        "activity_deleted" => "Deleted an Activity",
        "activity_deadline_reminder" => "Activity Reminder",
        "note_added" => "Added a Note",
        "note_edited" => "Edited a Note",
        "note_deleted" => "Deleted a Note",
        "comment_added" => "Added a Comment",
        "comment_deleted" => "Deleted a Comment",
        _ => "CRM Notification",
    }
}

pub fn deal_project_url(deal_id: i32) -> String {
    format!("https://granite-manager.com/employee/deals/edit/{deal_id}/project")
}

pub fn deal_email_chat_url(deal_id: i32, thread_id: &str) -> String {
    format!(
        "https://granite-manager.com/employee/deals/edit/{deal_id}/project/chat/{thread_id}"
    )
}

pub fn emails_chat_url(thread_id: &str) -> String {
    format!("https://granite-manager.com/employee/emails/chat/{thread_id}")
}

pub fn format_activity_notification(
    notification_type: &str,
    customer_name: Option<&str>,
    actor_name: Option<&str>,
    message: &str,
    deal_id: i32,
) -> String {
    let title = notification_type_title(notification_type);
    let customer = customer_name.unwrap_or("Deal");
    let actor_line = match actor_name {
        Some(name) if !name.is_empty() => format!("{name}: {message}"),
        _ => message.to_string(),
    };
    format!(
        "{title}\n\nCustomer: {customer}\n{actor_line}\n\n{}",
        deal_project_url(deal_id)
    )
}

pub fn format_email_notification(
    customer_name: Option<&str>,
    subject: Option<&str>,
    deal_id: Option<u64>,
    thread_id: &str,
) -> String {
    let customer = customer_name.unwrap_or("New email");
    let subject_line = subject.unwrap_or("New email");
    let url = match deal_id.and_then(|value| i32::try_from(value).ok()) {
        Some(deal_id) => deal_email_chat_url(deal_id, thread_id),
        None => emails_chat_url(thread_id),
    };
    format!("New email\n\nCustomer: {customer}\nSubject: {subject_line}\n\n{url}")
}
