//! Email alerts for lead problems nobody would otherwise notice.

use common::amazon::email::send_message;
use lambda_http::tracing;
use std::fmt::Display;

pub const LEAD_ALERT_EMAIL_ENV: &str = "LEAD_ALERT_EMAIL";

pub fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn alert_recipient() -> Option<String> {
    std::env::var(LEAD_ALERT_EMAIL_ENV)
        .ok()
        .map(|email| email.trim().to_string())
        .filter(|email| !email.is_empty())
}

/// Emails `LEAD_ALERT_EMAIL`. Never fails the caller: problems are only logged.
pub async fn send_lead_alert(subject: &str, html_body: &str) {
    tracing::warn!(subject, "Lead alert");
    let Some(recipient) = alert_recipient() else {
        tracing::error!(
            subject,
            "{LEAD_ALERT_EMAIL_ENV} is not set; lead alert not emailed"
        );
        return;
    };
    if let Err(e) = send_message(&[&recipient], subject, html_body).await {
        tracing::error!(?e, subject, "Failed to send lead alert email");
    }
}

/// For a lead that was saved but that Telegram did not deliver to the people
/// who should act on it.
pub async fn alert_lead_not_notified(company_id: i32, problem: &str, lead: &(impl Display + Sync)) {
    let subject = format!("Lead not delivered in Telegram (company #{company_id})");
    let body = format!(
        "<p><b>{}</b></p>\
<p>The lead is saved in the CRM, but this notification did not reach Telegram. \
Please make sure the lead is seen and assigned.</p>\
<pre style=\"font-family:inherit;white-space:pre-wrap\">{}</pre>",
        escape_html(problem),
        escape_html(&lead.to_string())
    );
    send_lead_alert(&subject, &body).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_escape_html() {
        assert_eq!(
            escape_html(r#"<b a="1">Tom & Jerry</b>"#),
            "&lt;b a=&quot;1&quot;&gt;Tom &amp; Jerry&lt;/b&gt;"
        );
    }
}
