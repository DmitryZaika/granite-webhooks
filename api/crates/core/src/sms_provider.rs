//! The SMS providers a company texts through, and their table and column
//! names. Every name spliced into SQL comes from here, never from input.

use crate::auth::session::SessionUser;
use crate::openapi::ToSchema;
use serde::{Deserialize, Serialize};

/// The SMS provider a company texts through: `cloudtalk`, `ringcentral`, or
/// `telnyx` (Granite Phone: texts from the company's Telnyx numbers).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum SmsProvider {
    Cloudtalk,
    Ringcentral,
    Telnyx,
}

impl SmsProvider {
    /// Every provider, for the admin inbox that merges them.
    pub const ALL: [Self; 3] = [Self::Cloudtalk, Self::Ringcentral, Self::Telnyx];

    pub const fn sms_table(self) -> &'static str {
        match self {
            Self::Cloudtalk => "cloudtalk_sms",
            Self::Ringcentral => "ringcentral_sms",
            Self::Telnyx => "telnyx_sms",
        }
    }

    pub const fn reads_table(self) -> &'static str {
        match self {
            Self::Cloudtalk => "cloudtalk_sms_thread_reads",
            Self::Ringcentral => "ringcentral_sms_thread_reads",
            Self::Telnyx => "telnyx_sms_thread_reads",
        }
    }

    /// The provider's synced contacts; Telnyx has none (customers are
    /// matched by their own phones).
    pub const fn contacts_table(self) -> Option<&'static str> {
        match self {
            Self::Cloudtalk => Some("cloudtalk_contacts"),
            Self::Ringcentral => Some("ringcentral_contacts"),
            Self::Telnyx => None,
        }
    }

    /// The attachments table and its column referencing the SMS row.
    pub const fn attachments(self) -> (&'static str, &'static str) {
        match self {
            Self::Cloudtalk => ("cloudtalk_sms_attachments", "cloudtalk_sms_id"),
            Self::Ringcentral => ("ringcentral_sms_attachments", "ringcentral_sms_id"),
            Self::Telnyx => ("telnyx_sms_attachments", "telnyx_sms_id"),
        }
    }

    /// The provider's own message id column (Telnyx's is a UUID, as text).
    pub const fn id_column(self) -> &'static str {
        match self {
            Self::Cloudtalk => "cloudtalk_id",
            Self::Ringcentral => "ringcentral_id",
            Self::Telnyx => "telnyx_id",
        }
    }

    /// The provider's message ids are numbers (Telnyx's are UUIDs).
    pub const fn numeric_ids(self) -> bool {
        !matches!(self, Self::Telnyx)
    }

    /// Breaks ties between rows of the same second: the provider's id, or
    /// ours when the provider's carries no order.
    pub const fn order_column(self) -> &'static str {
        match self {
            Self::Telnyx => "id",
            other => other.id_column(),
        }
    }

    /// The `users` column holding the agent (`CloudTalk`) or extension id;
    /// none for Telnyx, whose users text from the lines they may call from
    /// (`crate::phone::texting`).
    pub const fn agent_column(self) -> Option<&'static str> {
        match self {
            Self::Cloudtalk => Some("cloudtalk_agent_id"),
            Self::Ringcentral => Some("ringcentral_extension_id"),
            Self::Telnyx => None,
        }
    }

    /// The `users` column holding the user's own provider number; none for
    /// Telnyx (its numbers are `telnyx_numbers`).
    pub const fn phone_column(self) -> Option<&'static str> {
        match self {
            Self::Cloudtalk => Some("cloudtalk_phone_number"),
            Self::Ringcentral => Some("ringcentral_phone_number"),
            Self::Telnyx => None,
        }
    }

    /// `CloudTalk` pulls late webhook inserts back between their provider-id
    /// neighbours; the others order by insert time.
    pub const fn has_conversation_time(self) -> bool {
        matches!(self, Self::Cloudtalk)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cloudtalk => "cloudtalk",
            Self::Ringcentral => "ringcentral",
            Self::Telnyx => "telnyx",
        }
    }

    /// The provider stored as `name` (`delayed_sms.provider`).
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|provider| provider.as_str() == name)
    }

    /// The provider of the shared insert and upkeep code (`common::sms`);
    /// Telnyx rows are written by `crate::phone::texting`.
    pub const fn shared(self) -> Option<common::sms::derived::SmsProvider> {
        match self {
            Self::Cloudtalk => Some(common::sms::derived::SmsProvider::Cloudtalk),
            Self::Ringcentral => Some(common::sms::derived::SmsProvider::Ringcentral),
            Self::Telnyx => None,
        }
    }

    fn raw_agent(self, user: &SessionUser) -> Option<&str> {
        match self {
            Self::Cloudtalk => user.cloudtalk_agent_id.as_deref(),
            Self::Ringcentral => user.ringcentral_extension_id.as_deref(),
            Self::Telnyx => None,
        }
    }

    /// The user's trimmed agent id, if set. Always `None` for Telnyx, where
    /// texting depends on the user's lines (`crate::phone::texting::seat`).
    pub fn agent_of(self, user: &SessionUser) -> Option<String> {
        self.raw_agent(user)
            .map(str::trim)
            .filter(|agent| !agent.is_empty())
            .map(str::to_string)
    }

    /// Whether the user may text through this provider (has an agent id);
    /// see [`Self::agent_of`] for Telnyx.
    pub fn can_send(self, user: &SessionUser) -> bool {
        self.raw_agent(user).is_some_and(|agent| !agent.is_empty())
    }

    /// Last ten digits of the user's own provider number, if it has ten.
    pub fn phone_of(self, user: &SessionUser) -> Option<String> {
        let raw = match self {
            Self::Cloudtalk => user.cloudtalk_phone_number.as_deref(),
            Self::Ringcentral => user.ringcentral_phone_number.as_deref(),
            Self::Telnyx => None,
        }?;
        let digits = phone10(raw);
        (digits.len() >= 10).then_some(digits)
    }
}

/// The `idempotency_key` of a text copied to the provider a rep texts
/// through now (`crate::routes::sms::moves`): it names the original row,
/// `cloudtalk:<id>`, `ringcentral:<id>` or `telnyx:<id>`.
pub const COPY_KEY: &str = "^(cloudtalk|ringcentral|telnyx):[0-9]+$";

/// SQL: the row is not such a copy. Views over more than one provider skip
/// copies; the original stays in its own table. `alias` may be empty.
pub fn not_a_copy(alias: &str) -> String {
    let dot = if alias.is_empty() { "" } else { "." };
    format!("COALESCE({alias}{dot}idempotency_key, '') NOT REGEXP '{COPY_KEY}'")
}

/// The digits of `phone`, keeping the last ten (`canonicalPhone10`).
pub fn phone10(phone: &str) -> String {
    let digits: String = phone.chars().filter(char::is_ascii_digit).collect();
    let skip = digits.len().saturating_sub(10);
    digits[skip..].to_string()
}
