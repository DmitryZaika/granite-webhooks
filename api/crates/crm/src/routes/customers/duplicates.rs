//! Customers who already have a phone or email address (`api.customers.duplicate-check`).

use super::service::normalize_emails;
use crate::auth::EmployeeUser;
use crate::error::{ApiResult, ErrorBody};
use crate::extract::MultiQuery;
use crate::openapi::{IntoParams, ToSchema};
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, MySql, QueryBuilder};

/// Query of `GET /v1/customers/duplicates`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct DuplicatesQuery {
    /// Phone (its last 10 digits are compared).
    pub phone: Option<String>,
    /// Second phone.
    pub phone_2: Option<String>,
    /// Email addresses (repeat the parameter), compared case-insensitively.
    #[serde(default)]
    pub email: Vec<String>,
}

/// A live customer with one of the given contacts.
#[derive(Debug, Serialize, FromRow, ToSchema)]
pub struct DuplicateCustomer {
    /// Customer id.
    pub id: i32,
    /// Name.
    pub name: Option<String>,
    /// Phone.
    pub phone: Option<String>,
    /// Primary email address.
    pub email: Option<String>,
    /// Assigned sales rep's name.
    pub sales_rep_name: Option<String>,
}

/// The last 10 digits of a phone with at least 10.
pub fn phone_last10(phone: Option<&str>) -> Option<String> {
    let digits: String = phone?.chars().filter(char::is_ascii_digit).collect();
    (digits.len() >= 10).then(|| digits[digits.len() - 10..].to_string())
}

const LAST10: &str = "RIGHT(REGEXP_REPLACE(COALESCE({col}, ''), '[^0-9]', ''), 10)";

/// Find customers with a contact
///
/// Up to 5 live customers of the company that already have one of the
/// phones (last 10 digits) or addresses, for the "already exists" warning
/// of the add form. Empty without any contact.
#[crate::openapi::path(
    get,
    path = "/v1/customers/duplicates",
    operation_id = "find_duplicate_customers",
    tag = "customers",
    params(DuplicatesQuery),
    responses(
        (status = 200, description = "Matching customers", body = Vec<DuplicateCustomer>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn duplicates(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    MultiQuery(query): MultiQuery<DuplicatesQuery>,
) -> ApiResult<Json<Vec<DuplicateCustomer>>> {
    let phones: Vec<String> = [query.phone.as_deref(), query.phone_2.as_deref()]
        .into_iter()
        .filter_map(phone_last10)
        .collect();
    let emails = normalize_emails(&query.email);
    if phones.is_empty() && emails.is_empty() {
        return Ok(Json(Vec::new()));
    }
    let mut sql = QueryBuilder::<MySql>::new(
        "SELECT c.id, c.name, c.phone, pe.email, u.name AS sales_rep_name
         FROM customers c
         LEFT JOIN customers_emails pe ON pe.id = c.email_id AND pe.customer_id = c.id
         LEFT JOIN users u ON u.id = c.sales_rep AND u.is_deleted = 0
         WHERE c.deleted_at IS NULL AND c.company_id = ",
    );
    sql.push_bind(user.company_id).push(" AND (");
    let mut first = true;
    for phone in phones {
        if !first {
            sql.push(" OR ");
        }
        first = false;
        sql.push(LAST10.replace("{col}", "c.phone"))
            .push(" = ")
            .push_bind(phone.clone())
            .push(" OR ")
            .push(LAST10.replace("{col}", "c.phone_2"))
            .push(" = ")
            .push_bind(phone);
    }
    for email in emails {
        if !first {
            sql.push(" OR ");
        }
        first = false;
        sql.push(
            "EXISTS (SELECT 1 FROM customers_emails m WHERE m.customer_id = c.id AND LOWER(m.email) = LOWER(",
        )
        .push_bind(email)
        .push("))");
    }
    sql.push(") ORDER BY c.id LIMIT 5");
    Ok(Json(
        sql.build_query_as::<DuplicateCustomer>()
            .fetch_all(&state.pool)
            .await?,
    ))
}

#[cfg(test)]
mod tests {
    use super::phone_last10;

    #[test]
    fn last_ten_digits() {
        assert_eq!(
            phone_last10(Some("+1 (317) 555-0100")),
            Some("3175550100".into())
        );
        assert_eq!(phone_last10(Some("555-0100")), None);
        assert_eq!(phone_last10(None), None);
    }
}
