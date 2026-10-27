//! The email code message.

use super::intent::CODE_MINUTES;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use crate::webhooks_client::{OutgoingEmail, SendFailure};
use lambda_http::tracing;

/// What the buyer sees when the code could not be sent.
pub const SEND_FAILED: &str = "We could not send the email. Try again in a minute.";

fn code_email_html(code: &str) -> String {
    format!(
        r#"<div style="font-family: Arial, sans-serif; max-width: 480px; margin: 0 auto;">
  <h2 style="color: #1e293b;">Your Granite Manager code</h2>
  <p style="color: #475569; line-height: 1.6;">Enter this code to confirm your email and finish creating your account.</p>
  <p style="font-size: 32px; font-weight: 700; letter-spacing: 6px; color: #1e293b; margin: 16px 0;">{code}</p>
  <p style="color: #475569; line-height: 1.6;">It works for {CODE_MINUTES} minutes.</p>
  <p style="color: #94a3b8; font-size: 13px; margin-top: 24px;">If you did not ask for this, you can safely ignore this email.</p>
</div>"#
    )
}

/// Emails the 6-digit `code` to `to`. The failure is logged without the
/// address or the code.
pub async fn send_code(state: &AppState, to: &str, code: &str) -> ApiResult<()> {
    let email = OutgoingEmail {
        from_name: Some("Granite Manager".into()),
        from_address: "noreply@granite-manager.com".into(),
        to: vec![to.to_string()],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: "Your Granite Manager code".into(),
        body: code_email_html(code),
        signature: None,
        reply_to: Vec::new(),
        in_reply_to: None,
        references: Vec::new(),
        attachments: Vec::new(),
        transactional: true,
    };
    match state.webhooks.send_email(&email).await {
        Ok(_) => Ok(()),
        Err(err) => {
            let kind = match err {
                SendFailure::Rejected(_) => "rejected",
                SendFailure::Failed(_) => "failed",
            };
            tracing::warn!(kind, "signup code email failed");
            Err(ApiError::Upstream(SEND_FAILED.into()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_email_has_the_code_and_no_em_dash() {
        let html = code_email_html("042913");
        assert!(html.contains("042913"));
        assert!(html.contains("It works for 15 minutes."));
        assert!(!html.contains('\u{2014}'));
    }
}
