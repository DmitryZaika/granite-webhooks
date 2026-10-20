//! Per-provider table and column names. Every name spliced into SQL in this
//! module comes from here, never from input.

use crate::auth::session::SessionUser;
pub use crate::routes::stats::schemas::SmsProvider;
use sqlx::mysql::MySqlArguments;
use sqlx::{Arguments, MySql};

impl SmsProvider {
    /// Every provider, for the admin inbox that merges them.
    pub const ALL: [Self; 2] = [Self::Cloudtalk, Self::Ringcentral];

    pub const fn sms_table(self) -> &'static str {
        match self {
            Self::Cloudtalk => "cloudtalk_sms",
            Self::Ringcentral => "ringcentral_sms",
        }
    }

    pub const fn reads_table(self) -> &'static str {
        match self {
            Self::Cloudtalk => "cloudtalk_sms_thread_reads",
            Self::Ringcentral => "ringcentral_sms_thread_reads",
        }
    }

    pub const fn contacts_table(self) -> &'static str {
        match self {
            Self::Cloudtalk => "cloudtalk_contacts",
            Self::Ringcentral => "ringcentral_contacts",
        }
    }

    /// The attachments table and its column referencing the SMS row.
    pub const fn attachments(self) -> (&'static str, &'static str) {
        match self {
            Self::Cloudtalk => ("cloudtalk_sms_attachments", "cloudtalk_sms_id"),
            Self::Ringcentral => ("ringcentral_sms_attachments", "ringcentral_sms_id"),
        }
    }

    /// The provider's own message id column.
    pub const fn id_column(self) -> &'static str {
        match self {
            Self::Cloudtalk => "cloudtalk_id",
            Self::Ringcentral => "ringcentral_id",
        }
    }

    /// The `users` column holding the agent (`CloudTalk`) or extension id.
    pub const fn agent_column(self) -> &'static str {
        match self {
            Self::Cloudtalk => "cloudtalk_agent_id",
            Self::Ringcentral => "ringcentral_extension_id",
        }
    }

    /// The `users` column holding the user's own provider number.
    pub const fn phone_column(self) -> &'static str {
        match self {
            Self::Cloudtalk => "cloudtalk_phone_number",
            Self::Ringcentral => "ringcentral_phone_number",
        }
    }

    /// `CloudTalk` pulls late webhook inserts back between their provider-id
    /// neighbours; `RingCentral` orders by insert time.
    pub const fn has_conversation_time(self) -> bool {
        matches!(self, Self::Cloudtalk)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cloudtalk => "cloudtalk",
            Self::Ringcentral => "ringcentral",
        }
    }

    pub const fn shared(self) -> common::sms::derived::SmsProvider {
        match self {
            Self::Cloudtalk => common::sms::derived::SmsProvider::Cloudtalk,
            Self::Ringcentral => common::sms::derived::SmsProvider::Ringcentral,
        }
    }

    fn raw_agent(self, user: &SessionUser) -> Option<&str> {
        match self {
            Self::Cloudtalk => user.cloudtalk_agent_id.as_deref(),
            Self::Ringcentral => user.ringcentral_extension_id.as_deref(),
        }
    }

    /// The user's trimmed agent id, if set.
    pub fn agent_of(self, user: &SessionUser) -> Option<String> {
        self.raw_agent(user)
            .map(str::trim)
            .filter(|agent| !agent.is_empty())
            .map(str::to_string)
    }

    /// Whether the user may text through this provider (has an agent id).
    pub fn can_send(self, user: &SessionUser) -> bool {
        self.raw_agent(user).is_some_and(|agent| !agent.is_empty())
    }

    /// Last ten digits of the user's own provider number, if it has ten.
    pub fn phone_of(self, user: &SessionUser) -> Option<String> {
        let raw = match self {
            Self::Cloudtalk => user.cloudtalk_phone_number.as_deref(),
            Self::Ringcentral => user.ringcentral_phone_number.as_deref(),
        }?;
        let digits = phone10(raw);
        (digits.len() >= 10).then_some(digits)
    }
}

/// The digits of `phone`, keeping the last ten (`canonicalPhone10`).
pub fn phone10(phone: &str) -> String {
    let digits: String = phone.chars().filter(char::is_ascii_digit).collect();
    let skip = digits.len().saturating_sub(10);
    digits[skip..].to_string()
}

/// A thread key from the URL: 5 to 15 digits.
pub fn valid_thread_phone(phone: &str) -> bool {
    (5..=15).contains(&phone.len()) && phone.chars().all(|c| c.is_ascii_digit())
}

/// The phone with and without the country code 1 (`phoneVariants`).
pub fn phone_variants(digits: &str) -> Vec<String> {
    let digits: String = digits.chars().filter(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return Vec::new();
    }
    let mut variants = vec![digits.clone()];
    if digits.len() == 11 && digits.starts_with('1') {
        variants.push(digits[1..].to_string());
    }
    if digits.len() == 10 {
        variants.push(format!("1{digits}"));
    }
    variants
}

/// The onboarding tour's sample customer: never a real thread.
pub const SAMPLE_PHONES: [&str; 2] = ["5550101999", "5550101888"];

/// A bound value of SQL assembled from parts.
#[derive(Debug, Clone)]
pub enum Param {
    Int(i64),
    Text(String),
}

impl From<i32> for Param {
    fn from(value: i32) -> Self {
        Self::Int(value.into())
    }
}

impl From<i64> for Param {
    fn from(value: i64) -> Self {
        Self::Int(value)
    }
}

impl From<&str> for Param {
    fn from(value: &str) -> Self {
        Self::Text(value.to_string())
    }
}

impl From<String> for Param {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl From<&String> for Param {
    fn from(value: &String) -> Self {
        Self::Text(value.clone())
    }
}

/// `?, ?, ...` for `count` values.
pub fn marks(count: usize) -> String {
    vec!["?"; count].join(",")
}

/// The arguments for a statement, in order.
pub fn arguments(params: &[Param]) -> MySqlArguments {
    let mut args = MySqlArguments::default();
    for param in params {
        // Encoding an integer or a string into the buffer cannot fail.
        let added = match param {
            Param::Int(value) => args.add(*value),
            Param::Text(value) => args.add(value.clone()),
        };
        debug_assert!(added.is_ok());
    }
    args
}

/// Runs `sql` with `params` and decodes every row as `T`.
pub async fn fetch_all<T>(
    pool: &sqlx::MySqlPool,
    sql: String,
    params: &[Param],
) -> Result<Vec<T>, sqlx::Error>
where
    T: for<'r> sqlx::FromRow<'r, sqlx::mysql::MySqlRow> + Send + Unpin,
{
    sqlx::query_as_with::<MySql, T, _>(sqlx::AssertSqlSafe(sql), arguments(params))
        .fetch_all(pool)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phones() {
        assert_eq!(phone10("+1 (317) 555-0100"), "3175550100");
        assert_eq!(phone10("12345"), "12345");
        assert_eq!(phone_variants("3175550100"), ["3175550100", "13175550100"]);
        assert_eq!(phone_variants("13175550100"), ["13175550100", "3175550100"]);
        assert!(valid_thread_phone("3175550100"));
        assert!(!valid_thread_phone("317-555"));
    }
}
