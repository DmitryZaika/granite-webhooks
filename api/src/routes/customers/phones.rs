//! Customers from the SMS inbox: who a phone belongs to.
//!
//! - `GET  /v1/customers/sms-contacts`  → customers and suppliers to text
//! - `POST /v1/customers/{id}/phone`    → link a texting phone to a customer
//! - `POST /v1/customers/from-phone`    → a new customer for an unknown phone
//! - `GET  /v1/customers/by-phone`      → who an incoming call is from
//!
//! Replace the Remix `api.customers.{sms-search,link-phone,create-from-phone}`
//! and `api.{cloudtalk,ringcentral}.resolveCaller`.

use super::fuzzy::find_by_name;
use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::{JsonBody, MultiQuery};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

/// Query of `GET /v1/customers/sms-contacts`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ContactsQuery {
    /// A name (typos allowed) or phone digits; at least two characters.
    #[serde(default)]
    pub term: String,
}

/// Someone to text.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct SmsContact {
    /// Customer or supplier id.
    pub id: i32,
    /// Their name.
    pub name: String,
    /// The phone as saved (may be empty).
    pub phone: String,
}

/// `GET /v1/customers/sms-contacts`.
#[derive(Debug, Serialize, ToSchema)]
pub struct SmsContacts {
    /// Up to 10 customers, best match first.
    pub customers: Vec<SmsContact>,
    /// Up to 10 suppliers by name or phone.
    pub suppliers: Vec<SmsContact>,
}

/// Body of the phone routes.
#[derive(Debug, Deserialize, ToSchema)]
pub struct PhoneInput {
    /// 10 to 15 digits.
    pub phone_digits: String,
}

/// Body of `POST /v1/customers/from-phone`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct NewPhoneCustomer {
    /// 10 to 15 digits.
    pub phone_digits: String,
    /// The customer's name (at most 200 characters are kept).
    pub name: String,
}

/// Query of `GET /v1/customers/by-phone`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ByPhoneQuery {
    /// A phone in any format.
    #[serde(default)]
    pub phone: String,
}

/// `GET /v1/customers/by-phone`: the customer, if any.
#[derive(Debug, Serialize, ToSchema)]
pub struct PhoneOwner {
    /// The customer with this phone (or second phone); null when none.
    pub customer_id: Option<i32>,
    /// Their name.
    pub customer_name: Option<String>,
}

/// The customer a phone now belongs to.
#[derive(Debug, Serialize, ToSchema)]
pub struct PhoneCustomer {
    /// The customer.
    pub customer_id: i32,
    /// Their name.
    pub customer_name: String,
}

/// `317-555-0100` for 10 digits (or 11 starting with 1); `None` otherwise.
fn storage_format(digits: &str) -> Option<String> {
    if !(10..=15).contains(&digits.len()) || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let local = match digits.len() {
        10 => digits,
        11 if digits.starts_with('1') => &digits[1..],
        _ => return None,
    };
    Some(format!("{}-{}-{}", &local[..3], &local[3..6], &local[6..]))
}

fn checked_phone(digits: &str) -> ApiResult<String> {
    storage_format(digits).ok_or_else(|| ApiError::BadRequest("invalid_phone".into()))
}

/// Find people to text
///
/// Customers whose name matches `term` (typos allowed) or whose phone
/// contains its digits, and suppliers by name or phone.
#[utoipa::path(
    get,
    path = "/v1/customers/sms-contacts",
    operation_id = "find_sms_contacts",
    tag = "customers",
    params(ContactsQuery),
    responses(
        (status = 200, description = "Matches (none for a term under two characters)", body = SmsContacts),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn sms_contacts(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    MultiQuery(query): MultiQuery<ContactsQuery>,
) -> ApiResult<Json<SmsContacts>> {
    let pool = &state.pool;
    let term: String = query.term.trim().chars().take(100).collect();
    if term.chars().count() < 2 {
        return Ok(Json(SmsContacts {
            customers: Vec::new(),
            suppliers: Vec::new(),
        }));
    }
    let digits: String = term.chars().filter(char::is_ascii_digit).collect();
    let mut customers: Vec<SmsContact> = find_by_name(pool, user.company_id, &term, 15)
        .await?
        .into_iter()
        .map(|c| SmsContact {
            id: c.id,
            name: c.name.unwrap_or_default(),
            phone: c.phone.unwrap_or_default(),
        })
        .collect();
    if !digits.is_empty() {
        let by_phone: Vec<SmsContact> = sqlx::query_as(
            "SELECT id, COALESCE(name, '') AS name, COALESCE(phone, '') AS phone FROM customers
              WHERE company_id = ? AND deleted_at IS NULL
                AND REPLACE(REPLACE(REPLACE(REPLACE(phone, ' ', ''), '(', ''), ')', ''), '-', '') LIKE ?
              ORDER BY name LIMIT 10",
        )
        .bind(user.company_id)
        .bind(format!("%{digits}%"))
        .fetch_all(pool)
        .await?;
        for contact in by_phone {
            if !customers.iter().any(|c| c.id == contact.id) {
                customers.push(contact);
            }
        }
    }
    customers.truncate(10);
    let escaped: String = term
        .chars()
        .flat_map(|c| {
            if matches!(c, '\\' | '%' | '_') {
                vec!['\\', c]
            } else {
                vec![c]
            }
        })
        .collect();
    let suppliers = sqlx::query_as(
        "SELECT id, supplier_name AS name, COALESCE(phone, '') AS phone FROM suppliers
          WHERE company_id = ? AND supplier_name IS NOT NULL AND TRIM(supplier_name) <> ''
            AND (supplier_name LIKE ?
                 OR (? <> '' AND RIGHT(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(COALESCE(phone, ''), ' ', ''), '(', ''), ')', ''), '-', ''), '+', ''), 10) LIKE ?))
          ORDER BY supplier_name LIMIT 10",
    )
    .bind(user.company_id)
    .bind(format!("%{escaped}%"))
    .bind(&digits)
    .bind(format!("%{digits}%"))
    .fetch_all(pool)
    .await?;
    Ok(Json(SmsContacts {
        customers,
        suppliers,
    }))
}

/// Find who a phone belongs to
///
/// The company's live customer whose phone or second phone is this number
/// (with or without the country code), for an incoming call.
#[utoipa::path(
    get,
    path = "/v1/customers/by-phone",
    operation_id = "find_customer_by_phone",
    tag = "customers",
    params(ByPhoneQuery),
    responses(
        (status = 200, description = "The customer, or nulls", body = PhoneOwner),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn by_phone(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    MultiQuery(query): MultiQuery<ByPhoneQuery>,
) -> ApiResult<Json<PhoneOwner>> {
    let digits: String = query.phone.chars().filter(char::is_ascii_digit).collect();
    let last10: String = digits
        .chars()
        .skip(digits.len().saturating_sub(10))
        .collect();
    if last10.is_empty() {
        return Ok(Json(PhoneOwner {
            customer_id: None,
            customer_name: None,
        }));
    }
    let with_code = if last10.len() == 10 {
        format!("1{last10}")
    } else {
        last10.clone()
    };
    let clean = |column: &str| {
        format!(
            "REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(COALESCE({column}, ''), '-', ''), ' ', ''), '(', ''), ')', ''), '+', ''), '.', '')"
        )
    };
    let found: Option<(i32, Option<String>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT id, name FROM customers
          WHERE company_id = ? AND deleted_at IS NULL
            AND ({phone} IN (?, ?) OR {phone_2} IN (?, ?))
          LIMIT 1",
        phone = clean("phone"),
        phone_2 = clean("phone_2"),
    )))
    .bind(user.company_id)
    .bind(&last10)
    .bind(&with_code)
    .bind(&last10)
    .bind(&with_code)
    .fetch_optional(&state.pool)
    .await?;
    Ok(Json(PhoneOwner {
        customer_id: found.as_ref().map(|found| found.0),
        customer_name: found.and_then(|found| found.1),
    }))
}

/// Link a phone to a customer
///
/// Saves the phone as the customer's phone, or as the second phone when the
/// first is taken, and pushes the customer to the telephony address books
/// so the conversation shows their name.
#[utoipa::path(
    post,
    path = "/v1/customers/{id}/phone",
    operation_id = "link_customer_phone",
    tag = "customers",
    params(("id" = i32, Path, description = "Customer id")),
    request_body = PhoneInput,
    responses(
        (status = 200, description = "Linked", body = PhoneCustomer),
        (status = 400, description = "Not a phone number (`invalid_phone`)", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such customer in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn link_phone(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<PhoneInput>,
) -> ApiResult<Json<PhoneCustomer>> {
    let phone = checked_phone(&input.phone_digits)?;
    let pool = &state.pool;
    let (name, current): (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT name, phone FROM customers WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(user.company_id)
    .fetch_optional(pool)
    .await?
    .ok_or(ApiError::NotFound)?;
    let column = if current.is_some_and(|phone| !phone.is_empty()) {
        "phone_2"
    } else {
        "phone"
    };
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE customers SET {column} = ? WHERE id = ? AND company_id = ?"
    )))
    .bind(&phone)
    .bind(id)
    .bind(user.company_id)
    .execute(pool)
    .await?;
    state.webhooks.sync_contacts(user.company_id, id).await;
    Ok(Json(PhoneCustomer {
        customer_id: id,
        customer_name: name.unwrap_or_default(),
    }))
}

/// Create a customer for a phone
///
/// A new customer with just a name and the phone, for an unknown number in
/// the SMS inbox.
#[utoipa::path(
    post,
    path = "/v1/customers/from-phone",
    operation_id = "create_customer_from_phone",
    tag = "customers",
    request_body = NewPhoneCustomer,
    responses(
        (status = 201, description = "Created", body = PhoneCustomer),
        (status = 400, description = "`invalid_phone` or `empty_name`", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn from_phone(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<NewPhoneCustomer>,
) -> ApiResult<(StatusCode, Json<PhoneCustomer>)> {
    let phone = checked_phone(&input.phone_digits)?;
    let name: String = input.name.trim().chars().take(200).collect();
    if name.is_empty() {
        return Err(ApiError::BadRequest("empty_name".into()));
    }
    let id = sqlx::query("INSERT INTO customers (company_id, name, phone) VALUES (?, ?, ?)")
        .bind(user.company_id)
        .bind(&name)
        .bind(&phone)
        .execute(&state.pool)
        .await?
        .last_insert_id();
    Ok((
        StatusCode::CREATED,
        Json(PhoneCustomer {
            customer_id: i32::try_from(id).unwrap_or_default(),
            customer_name: name,
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::storage_format;

    #[test]
    fn phones_are_stored_dashed() {
        assert_eq!(
            storage_format("3175550100").as_deref(),
            Some("317-555-0100")
        );
        assert_eq!(
            storage_format("13175550100").as_deref(),
            Some("317-555-0100")
        );
        assert_eq!(storage_format("443175550100"), None);
        assert_eq!(storage_format("317555"), None);
    }
}
