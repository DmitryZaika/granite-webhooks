//! Per-provider SQL. Every table and column name spliced into it comes from
//! [`SmsProvider`] (`crate::sms_provider`), never from input.

pub use crate::sms_provider::{SmsProvider, phone10};
use sqlx::mysql::MySqlArguments;
use sqlx::{Arguments, MySql};

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
