pub const TELEGRAM_SENT_MARKER: &str = "__telegram_sent__";

const EMAIL_ICON: &str = "✉️";
const ACTIVITY_ICON: &str = "📋";
const SMS_ICON: &str = "💬";

pub fn notification_type_title(notification_type: &str) -> &'static str {
    match notification_type {
        "activity_added" => "Added an Activity",
        "activity_edited" => "Edited an Activity",
        "activity_deleted" => "Deleted an Activity",
        "activity_deadline_reminder" => "Activity Reminder",
        "estimate_appointment_reminder" => "In-Home Estimate Reminder",
        "installation_appointment_reminder" => "Installation Reminder",
        "template_appointment_reminder" => "Template Appointment Reminder",
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
    format!("https://granite-manager.com/employee/deals/edit/{deal_id}/project/chat/{thread_id}")
}

pub fn emails_chat_url(thread_id: &str) -> String {
    format!("https://granite-manager.com/employee/emails/chat/{thread_id}")
}

pub fn cloudtalk_thread_url(phone_digits: &str) -> String {
    format!("https://granite-manager.com/employee/cloudtalk/thread/{phone_digits}")
}

/// Text + optional URL button shown under the Telegram message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrmTelegramMessage {
    pub text: String,
    pub button_label: &'static str,
    pub button_url: String,
}

pub fn format_activity_notification(
    notification_type: &str,
    customer_name: Option<&str>,
    actor_name: Option<&str>,
    message: &str,
    deal_id: i32,
) -> CrmTelegramMessage {
    let title = notification_type_title(notification_type);
    let customer = customer_name.unwrap_or("Deal");
    let actor_line = match actor_name {
        Some(name) if !name.is_empty() => format!("{name}: {message}"),
        _ => message.to_string(),
    };
    CrmTelegramMessage {
        text: format!("{ACTIVITY_ICON} {title}\n\n👤 {customer}\n{actor_line}"),
        button_label: "📂 Open Deal",
        button_url: deal_project_url(deal_id),
    }
}

pub fn format_email_notification(
    customer_name: Option<&str>,
    subject: Option<&str>,
    deal_id: Option<u64>,
    thread_id: &str,
) -> CrmTelegramMessage {
    let customer = customer_name.unwrap_or("Customer");
    let subject_line = subject.unwrap_or("New email");
    let button_url = match deal_id.and_then(|value| i32::try_from(value).ok()) {
        Some(deal_id) => deal_email_chat_url(deal_id, thread_id),
        None => emails_chat_url(thread_id),
    };
    CrmTelegramMessage {
        text: format!("{EMAIL_ICON} New email\n\n👤 {customer}\n✉️ {subject_line}"),
        button_label: "📬 Open Email",
        button_url,
    }
}

pub fn format_sms_notification(sender_phone: &str, message: &str, phone_digits: &str) -> CrmTelegramMessage {
    CrmTelegramMessage {
        text: format!("{SMS_ICON} New CloudTalk SMS from {sender_phone}\n\n{message}"),
        button_label: "💬 Open SMS",
        button_url: cloudtalk_thread_url(phone_digits),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_notification_hides_raw_url_behind_button() {
        let msg = format_activity_notification(
            "activity_deadline_reminder",
            Some("Jane"),
            None,
            "Call back",
            12,
        );
        assert!(msg.text.starts_with("📋 Activity Reminder"));
        assert!(msg.text.contains("👤 Jane"));
        assert!(msg.text.contains("Call back"));
        assert!(!msg.text.contains("https://"));
        assert_eq!(msg.button_label, "📂 Open Deal");
        assert_eq!(msg.button_url, deal_project_url(12));
    }

    #[test]
    fn email_notification_hides_raw_url_behind_button() {
        let msg = format_email_notification(Some("Jane"), Some("Quote"), Some(12), "thread-1");
        assert!(msg.text.starts_with("✉️ New email"));
        assert!(msg.text.contains("👤 Jane"));
        assert!(msg.text.contains("✉️ Quote"));
        assert!(!msg.text.contains("https://"));
        assert_eq!(msg.button_label, "📬 Open Email");
        assert_eq!(msg.button_url, deal_email_chat_url(12, "thread-1"));
    }

    #[test]
    fn sms_notification_hides_raw_url_behind_button() {
        let msg = format_sms_notification("+15551234567", "Hello", "15551234567");
        assert!(msg.text.starts_with("💬 New CloudTalk SMS from +15551234567"));
        assert!(msg.text.contains("Hello"));
        assert!(!msg.text.contains("/employee/cloudtalk"));
        assert_eq!(msg.button_label, "💬 Open SMS");
        assert_eq!(msg.button_url, cloudtalk_thread_url("15551234567"));
    }
}
