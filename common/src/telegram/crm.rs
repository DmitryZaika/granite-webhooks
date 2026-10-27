use std::sync::LazyLock;

use regex::Regex;

pub const TELEGRAM_SENT_MARKER: &str = "__telegram_sent__";

const ACTIVITY_ICON: &str = "📋";
const SMS_ICON: &str = "💬";
const PERSON_ICON: &str = "👤";
const EMAIL_ICON: &str = "✉️";
const EMAIL_TEXT_ICON: &str = "💬";
const TELEGRAM_LINE_CHARS: usize = 36;
const EMAIL_NAME_MAX_LINES: usize = 1;
const EMAIL_SUBJECT_MAX_LINES: usize = 1;
const EMAIL_BODY_MAX_LINES: usize = 3;

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

fn escape_telegram_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
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

fn wrap_preview_lines(value: &str, max_lines: usize) -> String {
    let collapsed = collapse_whitespace(value);
    if collapsed.is_empty() || max_lines == 0 {
        return String::new();
    }
    let words: Vec<&str> = collapsed.split_whitespace().collect();
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut leftover = false;
    let mut index = 0;
    while index < words.len() {
        if lines.len() == max_lines {
            leftover = true;
            break;
        }
        let word = words[index];
        let candidate = if current.is_empty() {
            word.to_string()
        } else {
            format!("{current} {word}")
        };
        if candidate.chars().count() <= TELEGRAM_LINE_CHARS {
            current = candidate;
            index += 1;
            continue;
        }
        if !current.is_empty() {
            lines.push(std::mem::take(&mut current));
            continue;
        }
        leftover = word.chars().count() > TELEGRAM_LINE_CHARS;
        lines.push(word.chars().take(TELEGRAM_LINE_CHARS).collect());
        index += 1;
    }
    if !current.is_empty() && lines.len() < max_lines {
        lines.push(current);
    } else if !current.is_empty() {
        leftover = true;
    }
    if leftover {
        if let Some(last) = lines.last_mut() {
            if last.chars().count() + 3 <= TELEGRAM_LINE_CHARS {
                last.push_str("...");
            } else {
                let budget = TELEGRAM_LINE_CHARS.saturating_sub(3);
                *last = last.chars().take(budget).collect::<String>() + "...";
            }
        }
    }
    lines.join("\n")
}

/// "On Wed, Oct 21, 2026 at 6:14 PM <dema@example.com> wrote:" — Gmail, Apple
/// Mail and Outlook mobile reply attribution. May be wrapped over several
/// lines or follow the reply on the same line. A digit (date/time) is required
/// so ordinary prose like "On second thought I wrote: ..." is kept.
static ON_WROTE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)\bOn\s.{0,300}?\d.{0,300}?\swrote:").unwrap());

/// Outlook desktop / other clients' quoted-history separators.
static QUOTE_SEPARATOR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?im)^\s*(?:-{2,}\s*Original Message\s*-{2,}|_{10,}|From:[^\n]*\n\s*(?:Sent|Date):)",
    )
    .unwrap()
});

/// Returns only the new text of a reply: cuts everything from the quoted
/// history ("On … wrote:", Outlook separators) and drops `>` quoted lines.
fn strip_quoted_reply(body: &str) -> String {
    let mut text = body;
    if let Some(found) = ON_WROTE_RE.find(text) {
        text = &text[..found.start()];
    }
    if let Some(found) = QUOTE_SEPARATOR_RE.find(text) {
        text = &text[..found.start()];
    }
    text.lines()
        .filter(|line| !line.trim_start().starts_with('>'))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

fn email_body_preview(body: Option<&str>) -> Option<String> {
    let Some(raw) = body else {
        return None;
    };
    let reply = strip_quoted_reply(raw);
    let preview = wrap_preview_lines(&reply, EMAIL_BODY_MAX_LINES);
    if preview.is_empty() {
        return None;
    }
    Some(preview)
}

pub fn format_email_notification(
    customer_name: Option<&str>,
    subject: Option<&str>,
    body: Option<&str>,
    deal_id: Option<u64>,
    thread_id: &str,
) -> CrmTelegramMessage {
    let customer = escape_telegram_html(&preview_lines(
        customer_name
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("Customer"),
        EMAIL_NAME_MAX_LINES,
    ));
    let subject_line = escape_telegram_html(&preview_lines(
        subject
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("New email"),
        EMAIL_SUBJECT_MAX_LINES,
    ));
    let button_url = match deal_id.and_then(|value| i32::try_from(value).ok()) {
        Some(deal_id) => deal_email_chat_url(deal_id, thread_id),
        None => emails_chat_url(thread_id),
    };
    let text = match email_body_preview(body) {
        Some(preview) => {
            let preview = escape_telegram_html(&preview);
            format!(
                "{PERSON_ICON} <i><b>{customer}</b></i>\n{EMAIL_ICON} <b>{subject_line}</b>\n{EMAIL_TEXT_ICON} <i>{preview}</i>"
            )
        }
        None => {
            format!("{PERSON_ICON} <i><b>{customer}</b></i>\n{EMAIL_ICON} <b>{subject_line}</b>")
        }
    };
    CrmTelegramMessage {
        text,
        button_label: "📬 Open Email",
        button_url,
    }
}

pub fn format_sms_notification(
    sender_phone: &str,
    message: &str,
    phone_digits: &str,
) -> CrmTelegramMessage {
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
            Some("Your countertop quote"),
            Some("Dear customer!\n\nHere is a quote for your counters."),
            Some(12),
            "thread-1",
        );
        assert_eq!(
            msg.text,
            "👤 <i><b>Jane</b></i>\n✉️ <b>Your countertop quote</b>\n💬 <i>Dear customer! Here is a quote for\nyour counters.</i>"
        );
        assert!(!msg.text.contains("https://"));
        assert_eq!(msg.button_label, "📬 Open Email");
        assert_eq!(msg.button_url, deal_email_chat_url(12, "thread-1"));
    }

    #[test]
    fn email_notification_wraps_body_to_three_lines() {
        let long_body = "word ".repeat(80);
        let msg = format_email_notification(
            Some("Jane"),
            Some("Quote"),
            Some(&long_body),
            None,
            "thread-2",
        );
        let prefix = "👤 <i><b>Jane</b></i>\n✉️ <b>Quote</b>\n💬 <i>";
        assert!(msg.text.starts_with(prefix));
        let preview = msg
            .text
            .strip_prefix(prefix)
            .and_then(|rest| rest.strip_suffix("</i>"))
            .expect("prefix");
        let body_lines: Vec<&str> = preview.lines().collect();
        assert_eq!(body_lines.len(), EMAIL_BODY_MAX_LINES);
        assert!(preview.ends_with("..."));
        for line in &body_lines {
            assert!(line.chars().count() <= TELEGRAM_LINE_CHARS);
        }
    }

    #[test]
    fn email_notification_keeps_name_and_subject_to_line_limits() {
        let long_name = "Alexandra Catherine Montgomery-Williams";
        let long_subject = "Granite Depot of Indianapolis - Your kitchen countertop quote and fabrication timeline";
        let msg = format_email_notification(
            Some(long_name),
            Some(long_subject),
            Some("Dear customer! Here is a quote."),
            None,
            "thread-3",
        );
        let lines: Vec<&str> = msg.text.lines().collect();
        assert_eq!(lines.len(), 3);
        let name = lines[0]
            .strip_prefix("👤 <i><b>")
            .and_then(|line| line.strip_suffix("</b></i>"))
            .expect("name");
        let subject = lines[1]
            .strip_prefix("✉️ <b>")
            .and_then(|line| line.strip_suffix("</b>"))
            .expect("subject");
        assert!(name.chars().count() <= TELEGRAM_LINE_CHARS);
        assert!(subject.chars().count() <= TELEGRAM_LINE_CHARS * EMAIL_SUBJECT_MAX_LINES);
        assert!(name.ends_with("..."));
        assert!(subject.ends_with("..."));
    }

    #[test]
    fn email_notification_hides_quoted_reply_history() {
        let msg = format_email_notification(
            Some("Craig Hogwood"),
            Some("Re: Your countertop quote"),
            Some(
                "Is that installation included?\n\nOn Wed, Oct 21, 2026 at 6:14 PM <dema@granitedepotindy.com>\nwrote:\n\n> Hi Craig, here is your quote.",
            ),
            None,
            "thread-4",
        );
        assert!(
            msg.text
                .ends_with("💬 <i>Is that installation included?</i>")
        );
        assert!(!msg.text.contains("wrote"));
    }

    #[test]
    fn strip_quoted_reply_handles_inline_attribution_and_outlook() {
        assert_eq!(
            strip_quoted_reply(
                "Sounds good. On Oct 21, 2026, at 6:14 PM, Dema <dema@example.com> wrote: old"
            ),
            "Sounds good."
        );
        assert_eq!(
            strip_quoted_reply("Thanks!\n\n-----Original Message-----\nFrom: Dema"),
            "Thanks!"
        );
        assert_eq!(
            strip_quoted_reply("Thanks!\nFrom: Dema <d@x.com>\nSent: Wednesday\nold"),
            "Thanks!"
        );
        assert_eq!(
            strip_quoted_reply("On second thought, I wrote: yes please."),
            "On second thought, I wrote: yes please."
        );
    }

    #[test]
    fn email_notification_omits_preview_when_body_is_only_quote() {
        let msg = format_email_notification(
            Some("Jane"),
            Some("Re: Quote"),
            Some("On Wed, Oct 21, 2026 at 6:14 PM <dema@x.com> wrote:\n> old"),
            None,
            "thread-5",
        );
        assert_eq!(msg.text, "👤 <i><b>Jane</b></i>\n✉️ <b>Re: Quote</b>");
    }

    #[test]
    fn sms_notification_hides_raw_url_behind_button() {
        let msg = format_sms_notification("+15551234567", "Hello", "15551234567");
        assert!(
            msg.text
                .starts_with("💬 New CloudTalk SMS from +15551234567")
        );
        assert!(msg.text.contains("Hello"));
        assert!(!msg.text.contains("/employee/cloudtalk"));
        assert_eq!(msg.button_label, "💬 Open SMS");
        assert_eq!(msg.button_url, cloudtalk_thread_url("15551234567"));
    }
}
