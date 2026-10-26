//! What the inbound email intake (webhooks-email's `amazonses::parse_email`)
//! makes of a message, as stored by `crud::email`.

pub struct UploadedAttachment {
    pub content_type: String,
    pub content_subtype: Option<String>,
    pub filename: String,
    pub url: String,
}

/// One address from a `To:` or `Cc:` header, with its display name kept so the
/// CRM can render "Jane Doe" rather than the bare address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedRecipient {
    /// Lowercased, trimmed, bare address — the form stored and compared against.
    pub address: String,
    pub display_name: Option<String>,
}

#[derive(Clone)]
pub struct ParsedEmail {
    pub subject: Option<String>,
    pub body: String,
    pub html_body: Option<String>,
    /// Full text before quote-stripping. Set when `In-Reply-To` is present so
    /// a reply with no CRM parent can still show the quoted original.
    pub body_with_quote: Option<String>,
    pub html_body_with_quote: Option<String>,
    pub sender_email: String,
    /// `From:` display name (Gmail's sender column), when the header has one.
    pub sender_display_name: Option<String>,
    /// First `To:` address. Retained verbatim so existing callers and the
    /// `emails.receiver_email` column keep their current meaning. When `To:`
    /// carries no address (`undisclosed-recipients:;`), see
    /// `resolve_receiver_email` (webhooks-email) for where it comes from instead.
    pub receiver_email: String,
    /// Every `To:` address, in header order.
    pub to_recipients: Vec<ParsedRecipient>,
    /// Every `Cc:` address, in header order.
    pub cc_recipients: Vec<ParsedRecipient>,
    /// Every `Bcc:` address. Normally absent on inbound mail — a BCC'd
    /// recipient's copy does not carry the header — but stored when present.
    pub bcc_recipients: Vec<ParsedRecipient>,
    pub forward_to_email: Option<String>,
    /// Address SES accepted this message for, from the `for` clause of the
    /// `Received:` header SES prepends. Set only when the topmost `Received`
    /// was written by an SES inbound host; lower ones can be forged by the
    /// sender. Normalized like every stored address.
    pub envelope_recipient: Option<String>,
    pub in_reply_to: Option<String>,
    /// `References:` chain, oldest first. Used as a threading fallback when
    /// `In-Reply-To` does not match anything we issued.
    pub references: Vec<String>,
    pub message_id: String,
}

impl ParsedEmail {
    /// Keep the quoted original when this reply does not attach to a CRM thread.
    pub fn for_unknown_parent(&self) -> Self {
        let mut restored = self.clone();
        if let Some(body) = &self.body_with_quote {
            restored.body = body.clone();
        }
        if let Some(html_body) = &self.html_body_with_quote {
            restored.html_body = Some(html_body.clone());
        }
        restored
    }
}

/// The single normalization used for every stored or compared address:
/// unwrap `Name <addr>`, drop angle brackets, trim, lowercase.
pub fn normalize_address(raw: &str) -> String {
    let trimmed = raw.trim();
    let inner = match (trimmed.rfind('<'), trimmed.rfind('>')) {
        (Some(open), Some(close)) if close > open => &trimmed[open + 1..close],
        _ => trimmed,
    };
    inner.trim().to_lowercase()
}
