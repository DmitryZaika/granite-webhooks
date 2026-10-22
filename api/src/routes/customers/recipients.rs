//! Suggestions for an email recipient field: customers' addresses matching
//! what was typed (with typo tolerance) and, optionally, coworkers.
//!
//! Replaces `api.customers.email-recipients`.

use crate::auth::EmployeeUser;
use crate::error::{ApiResult, ErrorBody};
use crate::extract::MultiQuery;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, MySqlPool};
use std::collections::HashSet;
use utoipa::{IntoParams, ToSchema};

/// Query of `GET /v1/customers/email-recipients`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct RecipientsQuery {
    /// What was typed (name, company or address).
    #[serde(default)]
    pub term: String,
    /// Also suggest the company's users.
    #[serde(default)]
    pub include_users: bool,
}

/// A suggested address.
#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct Recipient {
    /// Customer (or user) id.
    pub id: i32,
    /// Name.
    pub name: String,
    /// Address.
    pub email: String,
    /// Company of a business customer.
    pub company_name: Option<String>,
}

/// Result of `GET /v1/customers/email-recipients`.
#[derive(Debug, Serialize, ToSchema)]
pub struct Recipients {
    /// Customers' addresses, best match first (at most 15).
    pub customers: Vec<Recipient>,
    /// Coworkers (with `include_users`).
    pub users: Vec<Recipient>,
}

/// Edit distance of two strings (by characters).
fn levenshtein(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut previous = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let current = row[j + 1];
            row[j + 1] = if ca == *cb {
                previous
            } else {
                1 + previous.min(row[j]).min(row[j + 1])
            };
            previous = current;
        }
    }
    row[b.len()]
}

fn local_part(email: &str) -> &str {
    match email.find('@') {
        Some(at) if at > 0 => &email[..at],
        _ => email,
    }
}

/// `matchesEmailFuzzy`: the address contains the term, or is one typo away
/// (in the whole address, the local part, or a prefix of it).
pub fn matches_email_fuzzy(email: &str, term: &str) -> bool {
    let email = email.trim().to_lowercase();
    let term = term.trim().to_lowercase();
    if term.is_empty() {
        return false;
    }
    if email.contains(&term) || levenshtein(&email, &term) <= 1 {
        return true;
    }
    let local = local_part(&email);
    match term.find('@') {
        None => {
            if local.starts_with(&term) || term.starts_with(local) || levenshtein(local, &term) <= 1
            {
                return true;
            }
            let term_len = term.chars().count();
            if local.chars().count() + 1 >= term_len {
                let head: String = local.chars().take(term_len).collect();
                return levenshtein(&head, &term) <= 1;
            }
            false
        }
        Some(at) => {
            let Some(email_at) = email.find('@').filter(|i| *i > 0) else {
                return false;
            };
            if email[email_at + 1..] != term[at + 1..] {
                return false;
            }
            levenshtein(local, &term[..at]) <= 1
        }
    }
}

fn rank(recipient: &Recipient, term: &str) -> u8 {
    let term = term.to_lowercase();
    let email = recipient.email.to_lowercase();
    if email.starts_with(&term) || local_part(&email).starts_with(&term) {
        0
    } else if matches_email_fuzzy(&recipient.email, &term) {
        1
    } else if recipient.name.to_lowercase().contains(&term) {
        2
    } else if recipient
        .company_name
        .as_deref()
        .is_some_and(|c| c.to_lowercase().contains(&term))
    {
        3
    } else {
        4
    }
}

fn dedupe(list: Vec<Recipient>) -> Vec<Recipient> {
    let mut seen = HashSet::new();
    list.into_iter()
        .filter(|r| seen.insert(r.email.clone()))
        .collect()
}

const CUSTOMER_ADDRESSES: &str =
    "SELECT c.id, COALESCE(c.name, '') AS name, ce.email, c.company_name
     FROM customers c JOIN customers_emails ce ON ce.customer_id = c.id
     WHERE c.company_id = ? AND c.deleted_at IS NULL AND ce.email IS NOT NULL AND ce.email != ''";

async fn users(
    pool: &MySqlPool,
    company_id: i32,
    term: &str,
) -> Result<Vec<Recipient>, sqlx::Error> {
    let like = format!("%{term}%");
    sqlx::query_as(
        "SELECT DISTINCT u.id, COALESCE(u.name, '') AS name, u.email, NULL AS company_name
         FROM users u
         LEFT JOIN users_positions up ON up.user_id = u.id AND up.company_id = ?
         WHERE u.is_deleted = 0 AND u.email IS NOT NULL AND TRIM(u.email) != ''
           AND (u.company_id = ? OR up.user_id IS NOT NULL)
           AND (? = '' OR u.name LIKE ? OR u.email LIKE ?)
         ORDER BY name LIMIT 50",
    )
    .bind(company_id)
    .bind(company_id)
    .bind(term)
    .bind(&like)
    .bind(&like)
    .fetch_all(pool)
    .await
}

/// Suggest email recipients
///
/// Addresses of the company's live customers matching `term` by name,
/// company or address, tolerating one typo in the address; with a domain
/// typed, every address of that domain. Best matches first, at most 15.
/// With `include_users`, also the company's users (empty `term`: all).
#[utoipa::path(
    get,
    path = "/v1/customers/email-recipients",
    operation_id = "suggest_email_recipients",
    tag = "customers",
    params(RecipientsQuery),
    responses(
        (status = 200, description = "Suggestions", body = Recipients),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn recipients(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    MultiQuery(query): MultiQuery<RecipientsQuery>,
) -> ApiResult<Json<Recipients>> {
    let pool = &state.pool;
    let term = query.term.trim().to_string();
    let users = if query.include_users {
        dedupe(users(pool, user.company_id, &term).await?)
    } else {
        Vec::new()
    };
    if term.is_empty() {
        return Ok(Json(Recipients {
            customers: Vec::new(),
            users,
        }));
    }
    let like = format!("%{term}%");
    let prefix = format!("{term}%");
    let mut customers: Vec<Recipient> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{CUSTOMER_ADDRESSES}
           AND (c.name LIKE ? OR ce.email LIKE ? OR (c.company_name IS NOT NULL AND c.company_name LIKE ?))
         ORDER BY CASE WHEN ce.email LIKE ? THEN 0 WHEN c.name LIKE ? THEN 1
                       WHEN c.company_name LIKE ? THEN 2 ELSE 3 END, c.name
         LIMIT 15"
    )))
    .bind(user.company_id)
    .bind(&like)
    .bind(&like)
    .bind(&like)
    .bind(&prefix)
    .bind(&prefix)
    .bind(&prefix)
    .fetch_all(pool)
    .await?;
    let lowered = term.to_lowercase();
    let extra: Vec<Recipient> = match term.find('@') {
        Some(at) if at > 0 && at + 1 < term.len() => {
            sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "{CUSTOMER_ADDRESSES} AND LOWER(ce.email) LIKE ? LIMIT 50"
            )))
            .bind(user.company_id)
            .bind(format!("%@{}", &lowered[at + 1..]))
            .fetch_all(pool)
            .await?
        }
        None if term.chars().count() >= 3 => {
            let count = lowered.chars().count();
            let keep = if count > 3 { count - 1 } else { 3 };
            let head: String = lowered.chars().take(keep).collect();
            sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "{CUSTOMER_ADDRESSES} AND LOWER(SUBSTRING_INDEX(ce.email, '@', 1)) LIKE ? LIMIT 50"
            )))
            .bind(user.company_id)
            .bind(format!("{head}%"))
            .fetch_all(pool)
            .await?
        }
        _ => Vec::new(),
    };
    customers.extend(extra);
    let mut customers: Vec<Recipient> = dedupe(customers)
        .into_iter()
        .filter(|c| {
            c.name.to_lowercase().contains(&lowered)
                || c.email.to_lowercase().contains(&lowered)
                || c.company_name
                    .as_deref()
                    .is_some_and(|n| n.to_lowercase().contains(&lowered))
                || matches_email_fuzzy(&c.email, &term)
        })
        .collect();
    customers.sort_by(|a, b| {
        rank(a, &term)
            .cmp(&rank(b, &term))
            .then_with(|| a.name.cmp(&b.name))
    });
    customers.truncate(15);
    Ok(Json(Recipients { customers, users }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_addresses() {
        assert!(matches_email_fuzzy("alice@example.com", "alice"));
        assert!(matches_email_fuzzy("alice@example.com", "alce"));
        assert!(matches_email_fuzzy(
            "alice@example.com",
            "alise@example.com"
        ));
        assert!(!matches_email_fuzzy("alice@example.com", "alise@other.com"));
        assert!(!matches_email_fuzzy("alice@example.com", "bob"));
        assert_eq!(levenshtein("kitten", "sitting"), 3);
    }
}
