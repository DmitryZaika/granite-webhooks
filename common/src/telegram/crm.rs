pub const TELEGRAM_SENT_MARKER: &str = "__telegram_sent__";

const ACTIVITY_ICON: &str = "📋";
const SMS_ICON: &str = "💬";
const PERSON_ICON: &str = "👤";
const EMAIL_ICON: &str = "✉️";
const EMAIL_TEXT_ICON: &str = "💬";
const TELEGRAM_LINE_CHARS: usize = 36;
const EMAIL_NAME_MAX_LINES: usize = 1;
const EMAIL_SUBJECT_MAX_LINES: usize = 2;
const EMAIL_BODY_MAX_LINES: usize = 4;
const EMAIL_BODY_MAX_WORDS: usize = 20;

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

fn collapse_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_to_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let budget = max_chars.saturating_sub(3);
    let mut truncated = String::new();
    for word in value.split_whitespace() {
        let candidate = if truncated.is_empty() {
            word.to_string()
        } else {
            format!("{truncated} {word}")
        };
        if candidate.chars().count() > budget {
            break;
        }
        truncated = candidate;
    }
    if truncated.is_empty() {
        truncated = value.chars().take(budget).collect();
    }
    truncated.push_str("...");
    truncated
}

fn preview_lines(value: &str, max_lines: usize) -> String {
    truncate_to_chars(&collapse_whitespace(value), TELEGRAM_LINE_CHARS * max_lines)
}

fn email_body_preview(body: Option<&str>) -> Option<String> {
    let Some(raw) = body else {
        return None;
    };
    let collapsed = collapse_whitespace(raw);
    if collapsed.is_empty() {
        return None;
    }
    let words: Vec<&str> = collapsed.split_whitespace().collect();
    let (limited, word_truncated) = if words.len() > EMAIL_BODY_MAX_WORDS {
        (words[..EMAIL_BODY_MAX_WORDS].join(" "), true)
    } else {
        (collapsed, false)
    };
    let line_budget = TELEGRAM_LINE_CHARS * EMAIL_BODY_MAX_LINES;
    if limited.chars().count() <= line_budget {
        if word_truncated {
            return Some(format!("{limited}..."));
        }
        return Some(limited);
    }
    Some(truncate_to_chars(&limited, line_budget))
}

pub fn format_email_notification(
    customer_name: Option<&str>,
    subject: Option<&str>,
    body: Option<&str>,
    deal_id: Option<u64>,
    thread_id: &str,
) -> CrmTelegramMessage {
    let customer = preview_lines(
        customer_name
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("Customer"),
        EMAIL_NAME_MAX_LINES,
    );
    let subject_line = preview_lines(
        subject
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("New email"),
        EMAIL_SUBJECT_MAX_LINES,
    );
    let button_url = match deal_id.and_then(|value| i32::try_from(value).ok()) {
        Some(deal_id) => deal_email_chat_url(deal_id, thread_id),
        None => emails_chat_url(thread_id),
    };
    let text = match email_body_preview(body) {
        Some(preview) => {
            format!("{PERSON_ICON} {customer}\n{EMAIL_ICON} {subject_line}\n{EMAIL_TEXT_ICON} {preview}")
        }
        None => format!("{PERSON_ICON} {customer}\n{EMAIL_ICON} {subject_line}"),
    };
    CrmTelegramMessage {
        text,
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
    fn email_notification_puts_customer_subject_then_body_preview() {
        let msg = format_email_notification(
            Some("Jane"),
            Some("Granite Depot - Your countertop quote"),
            Some("Dear customer!\n\nHere is a quote for your counters."),
            Some(12),
            "thread-1",
        );
        assert_eq!(
            msg.text,
            "👤 Jane\n✉️ Granite Depot - Your countertop quote\n💬 Dear customer! Here is a quote for your counters."
        );
        assert!(!msg.text.contains("https://"));
        assert_eq!(msg.button_label, "📬 Open Email");
        assert_eq!(msg.button_url, deal_email_chat_url(12, "thread-1"));
    }

    #[test]
    fn email_notification_truncates_long_body_preview() {
        let long_body = "word ".repeat(80);
        let msg = format_email_notification(
            Some("Jane"),
            Some("Quote"),
            Some(&long_body),
            None,
            "thread-2",
        );
        assert!(msg.text.starts_with("👤 Jane\n✉️ Quote\n💬 "));
        let preview = msg
            .text
            .strip_prefix("👤 Jane\n✉️ Quote\n💬 ")
            .expect("prefix");
        let words: Vec<&str> = preview
            .trim_end_matches('.')
            .split_whitespace()
            .collect();
        assert!(preview.ends_with("..."));
        assert_eq!(words.len(), EMAIL_BODY_MAX_WORDS);
        assert!(preview.chars().count() <= TELEGRAM_LINE_CHARS * EMAIL_BODY_MAX_LINES);
    }

    #[test]
    fn email_notification_keeps_name_and_subject_to_line_limits() {
        let long_name = "Alexandra Catherine Montgomery-Williams";
        let long_subject =
            "Granite Depot of Indianapolis - Your kitchen countertop quote and fabrication timeline";
        let msg = format_email_notification(
            Some(long_name),
            Some(long_subject),
            Some("Dear customer! Here is a quote."),
            None,
            "thread-3",
        );
        let lines: Vec<&str> = msg.text.lines().collect();
        assert_eq!(lines.len(), 3);
        let name = lines[0].strip_prefix("👤 ").expect("name icon");
        let subject = lines[1].strip_prefix("✉️ ").expect("subject icon");
        assert!(name.chars().count() <= TELEGRAM_LINE_CHARS);
        assert!(subject.chars().count() <= TELEGRAM_LINE_CHARS * EMAIL_SUBJECT_MAX_LINES);
        assert!(name.ends_with("..."));
        assert!(subject.ends_with("..."));
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
