//! A customer who wrote, called or talked to the company stops their running
//! flows (`sms_flow_enrollments`).

use super::provider::SmsProvider;
use sqlx::MySqlPool;

/// The ten-digit form of a phone (`canonicalPhone10`).
pub fn canonical_phone10(raw: &str) -> String {
    let digits: String = raw.chars().filter(char::is_ascii_digit).collect();
    if digits.len() > 10 {
        digits[digits.len() - 10..].to_string()
    } else {
        digits
    }
}

/// The customer wrote or talked to the company: their running flows stop
/// (by this phone, the customer and the customer's other phone). How many.
pub async fn stop_after_customer_conversation(
    pool: &MySqlPool,
    provider: SmsProvider,
    company_id: i32,
    phone: &str,
) -> Result<u64, sqlx::Error> {
    let digits = canonical_phone10(phone);
    if digits.is_empty() {
        return Ok(0);
    }
    let mut affected = stop_phone_runs(pool, company_id, &digits).await?;
    let Some(customer) =
        super::thread::customer_by_phone(pool, provider, company_id, &digits).await?
    else {
        return Ok(affected);
    };
    affected += stop_customer_runs(pool, company_id, customer.id, &digits).await?;
    Ok(affected)
}

/// The customer of a Granite Phone call talked to the company (or called
/// it): their running flows stop, like [`stop_after_customer_conversation`]
/// (by the call's number, the customer and the customer's other phone).
/// The call names its customer, so no SMS provider is needed: a company
/// that texts through none stops its email flows too. How many.
pub async fn stop_after_call(
    pool: &MySqlPool,
    company_id: i32,
    phone: &str,
    customer_id: i32,
) -> Result<u64, sqlx::Error> {
    let digits = canonical_phone10(phone);
    let mut affected = if digits.is_empty() {
        0
    } else {
        stop_phone_runs(pool, company_id, &digits).await?
    };
    affected += stop_customer_runs(pool, company_id, customer_id, &digits).await?;
    Ok(affected)
}

/// Stops the running flows of the phone `digits` (last 10): how many.
async fn stop_phone_runs(
    pool: &MySqlPool,
    company_id: i32,
    digits: &str,
) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query(
        "UPDATE sms_flow_enrollments SET status = 'stopped_by_reply', updated_at = UTC_TIMESTAMP()
          WHERE company_id = ? AND customer_phone_digits = ? AND status IN ('active', 'paused')",
    )
    .bind(company_id)
    .bind(digits)
    .execute(pool)
    .await?
    .rows_affected())
}

/// Stops the running flows of the customer and of their phones other than
/// `digits` (stopped already): how many.
async fn stop_customer_runs(
    pool: &MySqlPool,
    company_id: i32,
    customer_id: i32,
    digits: &str,
) -> Result<u64, sqlx::Error> {
    let mut affected = sqlx::query(
        "UPDATE sms_flow_enrollments SET status = 'stopped_by_reply', updated_at = UTC_TIMESTAMP()
          WHERE company_id = ? AND customer_id = ? AND status IN ('active', 'paused')",
    )
    .bind(company_id)
    .bind(customer_id)
    .execute(pool)
    .await?
    .rows_affected();
    let phones: Option<(Option<String>, Option<String>)> =
        sqlx::query_as("SELECT phone, phone_2 FROM customers WHERE id = ? AND company_id = ? AND deleted_at IS NULL")
            .bind(customer_id)
            .bind(company_id)
            .fetch_optional(pool)
            .await?;
    if let Some((phone, phone_2)) = phones {
        for raw in <[_; 2]>::from((phone, phone_2)).into_iter().flatten() {
            let other = canonical_phone10(&raw);
            if !other.is_empty() && other != digits {
                affected += stop_phone_runs(pool, company_id, &other).await?;
            }
        }
    }
    Ok(affected)
}
