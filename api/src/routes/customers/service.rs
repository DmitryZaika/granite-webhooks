//! Writes shared by the customer routes: addresses, sources, the
//! reassignment audit trail and the open deal of an assigned customer.

use crate::error::ApiError;
use crate::routes::deals::service::{enter_stage, sync_drip};
use chrono::{DateTime, Utc};
use lambda_http::tracing;
use sqlx::{MySql, MySqlConnection, MySqlPool, Transaction};
use std::collections::HashSet;

/// `customers_history.reassigned_to` of a deal deletion.
const DEAL_DELETED_PREFIX: &str = "deal_deleted:";
/// Who the audit trail credits with a lead's first assignment.
const SALES_MANAGER_AUDIT_NAME: &str = "sales manager";
/// Sources every company has, besides its own (`customer_sources`).
pub const BUILTIN_SOURCES: &[&str] = &[
    "check-in",
    "leads",
    "call-in",
    "other",
    "check-list",
    "user-input",
];
/// `positions.id` of a sales rep.
pub const SALES_REP_POSITION: i32 = 1;

/// Trimmed addresses without blanks or (case-insensitive) repeats, in order.
pub fn normalize_emails(emails: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    emails
        .iter()
        .map(|email| email.trim().to_string())
        .filter(|email| !email.is_empty() && seen.insert(email.to_lowercase()))
        .collect()
}

/// `syncCustomerEmails`: the customer's addresses become exactly `emails`,
/// the first one primary. An address of another live customer of the
/// company is refused (`Conflict`).
pub async fn sync_emails(
    tx: &mut Transaction<'_, MySql>,
    customer_id: i32,
    emails: &[String],
    created_by: i32,
) -> Result<(), ApiError> {
    let wanted = normalize_emails(emails);
    let existing: Vec<(i32, String)> = sqlx::query_as(
        "SELECT id, email FROM customers_emails WHERE customer_id = ? ORDER BY created_at, id",
    )
    .bind(customer_id)
    .fetch_all(&mut **tx)
    .await?;
    let insert: Vec<&String> = wanted
        .iter()
        .filter(|email| !existing.iter().any(|(_, e)| e.eq_ignore_ascii_case(email)))
        .collect();
    for email in insert {
        if let Some(owner) = other_owner(tx, customer_id, email).await? {
            return Err(ApiError::Conflict(format!(
                "{email} is already saved on {owner}"
            )));
        }
    }
    // Keep the wanted order: the first wanted address is the primary one.
    let mut ordered = Vec::with_capacity(wanted.len());
    for email in &wanted {
        if let Some((id, _)) = existing.iter().find(|(_, e)| e.eq_ignore_ascii_case(email)) {
            ordered.push(*id);
            continue;
        }
        let id = sqlx::query(
            "INSERT INTO customers_emails (customer_id, email, created_by) VALUES (?, ?, ?)",
        )
        .bind(customer_id)
        .bind(email)
        .bind(created_by)
        .execute(&mut **tx)
        .await?
        .last_insert_id();
        ordered.push(i32::try_from(id).unwrap_or(i32::MAX));
    }
    sqlx::query("UPDATE customers SET email_id = ? WHERE id = ?")
        .bind(ordered.first())
        .bind(customer_id)
        .execute(&mut **tx)
        .await?;
    if ordered.is_empty() {
        sqlx::query("DELETE FROM customers_emails WHERE customer_id = ?")
            .bind(customer_id)
            .execute(&mut **tx)
            .await?;
    } else {
        let marks = vec!["?"; ordered.len()].join(", ");
        let mut delete = sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM customers_emails WHERE customer_id = ? AND id NOT IN ({marks})"
        )))
        .bind(customer_id);
        for id in &ordered {
            delete = delete.bind(id);
        }
        delete.execute(&mut **tx).await?;
    }
    Ok(())
}

/// Name of another live customer of the company with this address.
async fn other_owner(
    conn: &mut MySqlConnection,
    customer_id: i32,
    email: &str,
) -> Result<Option<String>, sqlx::Error> {
    let owner: Option<Option<String>> = sqlx::query_scalar(
        "SELECT other.name FROM customers target
         JOIN customers other ON other.company_id = target.company_id
          AND other.id <> target.id AND other.deleted_at IS NULL
         JOIN customers_emails ce ON ce.customer_id = other.id
         WHERE target.id = ? AND LOWER(TRIM(ce.email)) = LOWER(TRIM(?))
         LIMIT 1",
    )
    .bind(customer_id)
    .bind(email)
    .fetch_optional(conn)
    .await?;
    Ok(owner.map(|name| {
        name.map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "another customer".to_string())
    }))
}

/// `isAllowedCustomerSource`: a built-in source, one of the company's, or
/// the customer's current one.
pub async fn source_allowed(
    pool: &MySqlPool,
    company_id: i32,
    source: &str,
    current: Option<&str>,
) -> Result<bool, sqlx::Error> {
    let source = source.trim();
    if source.is_empty() {
        return Ok(false);
    }
    if current.is_some_and(|current| current.trim().eq_ignore_ascii_case(source))
        || BUILTIN_SOURCES.contains(&source)
    {
        return Ok(true);
    }
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM customer_sources
         WHERE company_id = ? AND deleted_at IS NULL AND LOWER(source_key) = LOWER(?) LIMIT 1",
    )
    .bind(company_id)
    .bind(source)
    .fetch_optional(pool)
    .await?;
    Ok(found.is_some())
}

/// Live user's trimmed name.
pub async fn user_name(
    pool: &MySqlPool,
    user_id: Option<i32>,
) -> Result<Option<String>, sqlx::Error> {
    let Some(user_id) = user_id else {
        return Ok(None);
    };
    let name: Option<Option<String>> =
        sqlx::query_scalar("SELECT name FROM users WHERE id = ? AND is_deleted = 0 LIMIT 1")
            .bind(user_id)
            .fetch_optional(pool)
            .await?;
    Ok(name
        .flatten()
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty()))
}

/// `auditDisplayName`: the actor's name as the audit trail stores it.
pub fn audit_name(name: Option<&str>) -> Option<String> {
    name.map(str::trim)
        .filter(|n| !n.is_empty())
        .map(|n| n.chars().take(100).collect())
}

/// `recordCustomerReassignment`: one line of the audit trail (only with an actor).
pub async fn record_reassignment(
    pool: &MySqlPool,
    customer_id: i32,
    by: Option<&str>,
    to: Option<&str>,
    at: Option<DateTime<Utc>>,
) -> Result<(), sqlx::Error> {
    let Some(by) = by.map(str::trim).filter(|b| !b.is_empty()) else {
        return Ok(());
    };
    let to = to.map(str::trim).filter(|t| !t.is_empty());
    match at {
        Some(at) => {
            sqlx::query(
                "INSERT INTO customers_history (customer_id, reassigned_by, reassigned_to, updated_at)
                 VALUES (?, ?, ?, ?)",
            )
            .bind(customer_id)
            .bind(by)
            .bind(to)
            .bind(at)
            .execute(pool)
            .await?;
        }
        None => {
            sqlx::query(
                "INSERT INTO customers_history (customer_id, reassigned_by, reassigned_to) VALUES (?, ?, ?)",
            )
            .bind(customer_id)
            .bind(by)
            .bind(to)
            .execute(pool)
            .await?;
        }
    }
    Ok(())
}

/// Source, creator and first assignment date of a customer.
type LeadRow = (Option<String>, Option<String>, Option<DateTime<Utc>>);

/// `recordLeadManagerAssignmentBeforeReassign`: before a lead (no creator)
/// with an empty trail is first reassigned, record that the sales manager
/// had assigned it to `previous_rep`.
pub async fn record_lead_manager_assignment(
    pool: &MySqlPool,
    customer_id: i32,
    previous_rep: Option<i32>,
) -> Result<(), sqlx::Error> {
    let Some(previous_rep) = previous_rep else {
        return Ok(());
    };
    let lead: Option<LeadRow> = sqlx::query_as(
        "SELECT source, created_by, assigned_date FROM customers WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(customer_id)
    .fetch_optional(pool)
    .await?;
    let Some((source, created_by, assigned_date)) = lead else {
        return Ok(());
    };
    let is_lead = source
        .as_deref()
        .map(str::trim)
        .is_some_and(|s| s.eq_ignore_ascii_case("leads"))
        && created_by
            .as_deref()
            .map(str::trim)
            .is_none_or(str::is_empty);
    if !is_lead {
        return Ok(());
    }
    let trail: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM customers_history
         WHERE customer_id = ? AND (reassigned_to IS NULL OR reassigned_to NOT LIKE ?)",
    )
    .bind(customer_id)
    .bind(format!("{DEAL_DELETED_PREFIX}%"))
    .fetch_one(pool)
    .await?;
    if trail > 0 {
        return Ok(());
    }
    let Some(to) = user_name(pool, Some(previous_rep)).await? else {
        return Ok(());
    };
    let at = match assigned_date {
        Some(at) => Some(at),
        None => sqlx::query_scalar(
            "SELECT created_at FROM deals WHERE customer_id = ? AND user_id = ? AND deleted_at IS NULL
             ORDER BY created_at, id LIMIT 1",
        )
        .bind(customer_id)
        .bind(previous_rep)
        .fetch_optional(pool)
        .await?
        .flatten(),
    };
    record_reassignment(
        pool,
        customer_id,
        Some(SALES_MANAGER_AUDIT_NAME),
        Some(&to),
        at,
    )
    .await
}

/// `ensureOpenDealForCustomer`: the customer's deals move to `sales_rep`;
/// when none is open, a deal starts in the company's default pipeline
/// (first stage) with its drips. Returns the open deal.
pub async fn ensure_open_deal(
    pool: &MySqlPool,
    customer_id: i32,
    company_id: i32,
    sales_rep: i32,
) -> Result<i64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT id FROM customers WHERE id = ? AND deleted_at IS NULL FOR UPDATE")
        .bind(customer_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE deals SET user_id = ?
         WHERE customer_id = ? AND deleted_at IS NULL AND (user_id IS NULL OR user_id <> ?)",
    )
    .bind(sales_rep)
    .bind(customer_id)
    .bind(sales_rep)
    .execute(&mut *tx)
    .await?;
    let open: Option<i64> = sqlx::query_scalar(
        "SELECT CAST(id AS SIGNED) FROM deals
         WHERE customer_id = ? AND deleted_at IS NULL AND is_won IS NULL ORDER BY id LIMIT 1",
    )
    .bind(customer_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(open) = open {
        tx.commit().await?;
        return Ok(open);
    }
    let stage: Option<(i32, Option<String>)> = sqlx::query_as(
        "SELECT dl.id, dl.name FROM groups_list g
         JOIN deals_list dl ON dl.group_id = g.id AND dl.deleted_at IS NULL
         WHERE g.deleted_at IS NULL AND ((g.company_id = ? AND g.is_default = 1) OR g.id = 1)
         ORDER BY g.id = 1, dl.position LIMIT 1",
    )
    .bind(company_id)
    .fetch_optional(&mut *tx)
    .await?;
    let (list_id, status) = stage.map_or_else(
        || (1, "Not Contacted Yet".to_string()),
        |(id, name)| (id, name.unwrap_or_else(|| "Not Contacted Yet".to_string())),
    );
    let next: i64 = sqlx::query_scalar(
        "SELECT CAST(COALESCE(MAX(position), 0) + 1 AS SIGNED) FROM deals WHERE list_id = ? AND deleted_at IS NULL",
    )
    .bind(list_id)
    .fetch_one(&mut *tx)
    .await?;
    let deal_id = sqlx::query(
        "INSERT INTO deals (customer_id, status, list_id, position, user_id, created_by)
         VALUES (?, ?, ?, ?, ?, NULL)",
    )
    .bind(customer_id)
    .bind(status)
    .bind(list_id)
    .bind(next)
    .bind(sales_rep)
    .execute(&mut *tx)
    .await?
    .last_insert_id();
    let deal_id = i64::try_from(deal_id).unwrap_or(i64::MAX);
    enter_stage(&mut tx, deal_id, list_id).await?;
    tx.commit().await?;
    if let Err(err) = sync_drip(pool, deal_id, list_id, true).await {
        tracing::warn!(?err, deal_id, "drips of the new deal not scheduled");
    }
    Ok(deal_id)
}

/// Reminders for a newly assigned sales rep: text the customer within a
/// day, check on them within three (each once while undone).
pub async fn remind_new_rep(
    pool: &MySqlPool,
    sales_rep: i32,
    customer_id: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO notifications (user_id, customer_id, message, due_at)
         SELECT ?, id, CONCAT('Please text ', name), created_date + INTERVAL 24 HOUR
         FROM customers WHERE id = ? AND deleted_at IS NULL
           AND NOT EXISTS (SELECT 1 FROM notifications n
                           WHERE n.user_id = ? AND n.customer_id = ? AND n.is_done = 0)",
    )
    .bind(sales_rep)
    .bind(customer_id)
    .bind(sales_rep)
    .bind(customer_id)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO notifications (user_id, customer_id, message, due_at)
         SELECT ?, id, CONCAT('Please check the customer ', name), created_date + INTERVAL 72 HOUR
         FROM customers WHERE id = ? AND deleted_at IS NULL
           AND NOT EXISTS (SELECT 1 FROM notifications n
                           WHERE n.user_id = ? AND n.customer_id = ? AND n.is_done = 0
                             AND n.message LIKE 'Please check%')",
    )
    .bind(sales_rep)
    .bind(customer_id)
    .bind(sales_rep)
    .bind(customer_id)
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emails_are_trimmed_and_unique() {
        assert_eq!(
            normalize_emails(&[
                " A@x.com".into(),
                "a@X.com".into(),
                String::new(),
                "b@x.com".into()
            ]),
            ["A@x.com", "b@x.com"]
        );
    }

    #[test]
    fn audit_names() {
        assert_eq!(audit_name(Some("  Rita ")), Some("Rita".into()));
        assert_eq!(audit_name(Some(" ")), None);
        assert_eq!(audit_name(Some(&"x".repeat(120))).unwrap().len(), 100);
    }
}
