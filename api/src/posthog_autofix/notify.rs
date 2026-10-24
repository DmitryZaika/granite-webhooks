//! Sending the auto-fix emails. A failed email is logged and never fails the
//! job or the callback that triggered it.

use crate::webhooks_client::{OutgoingEmail, WebhooksClient};
use lambda_http::tracing;

const NO_REPLY: &str = "noreply@granite-manager.com";

/// A rendered email (see [`super::emails`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Email {
    pub subject: String,
    pub html: String,
}

/// Sends email to the developers.
#[allow(async_fn_in_trait)]
pub trait Mailer: Sync {
    async fn send(&self, to: &[String], email: &Email) -> Result<(), String>;
}

impl Mailer for WebhooksClient {
    async fn send(&self, to: &[String], email: &Email) -> Result<(), String> {
        let outgoing = OutgoingEmail {
            from_name: Some("Granite Manager".into()),
            from_address: NO_REPLY.into(),
            to: to.to_vec(),
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: email.subject.clone(),
            body: email.html.clone(),
            signature: None,
            reply_to: Vec::new(),
            in_reply_to: None,
            references: Vec::new(),
            attachments: Vec::new(),
            transactional: true,
        };
        self.send_email(&outgoing)
            .await
            .map(|_| ())
            .map_err(|err| format!("{err:?}"))
    }
}

/// Sends `email` to `recipients`; false (and a log line) when there is no
/// one to send to or the send failed.
pub async fn deliver(mailer: &impl Mailer, recipients: &[String], email: &Email) -> bool {
    if recipients.is_empty() {
        tracing::warn!(
            subject = email.subject,
            "POSTHOG_AUTOFIX_RECIPIENTS is empty; auto-fix email not sent"
        );
        return false;
    }
    match mailer.send(recipients, email).await {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(error, subject = email.subject, "auto-fix email failed");
            false
        }
    }
}
