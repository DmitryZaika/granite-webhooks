//! The email history of a deal (deal page activity panel and History tab).
//! Ports `dealEmailHistory.server.ts`, `crud/emails.ts` and
//! `backfillScheduledEmailsToHistory.server.ts`.

use super::sql::{Arg, CLIENT_OPEN_MIN_DELAY, Sql, normalize_address};
use super::thread::from_email;
use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::serde_helpers::js_date;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use chrono::{DateTime, NaiveDateTime, Utc};
use serde::Serialize;
use sqlx::{FromRow, MySqlPool};
use std::collections::{HashMap, HashSet};
use utoipa::ToSchema;

/// A message of a deal's or customer's email history.
#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct HistoryEmail {
    /// Message id.
    pub id: i32,
    /// Deal the message was sent on, if any.
    pub deal_id: Option<i64>,
    /// Conversation id.
    pub thread_id: String,
    /// Subject.
    pub subject: Option<String>,
    /// Plain text body.
    pub body: String,
    /// When it was sent.
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub sent_at: Option<DateTime<Utc>>,
    /// Real opens by the recipient (not proxies firing right after the send).
    pub read_count: i64,
    /// The message has attachments.
    pub has_attachments: bool,
    /// Sending user; null for mail from the customer.
    pub sender_user_id: Option<i32>,
    /// From header.
    pub sender_email: Option<String>,
    /// Sending user's name.
    pub sender_name: Option<String>,
    /// When an employee opened it.
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub employee_read_at: Option<DateTime<Utc>>,
    /// Some message of the conversation has attachments.
    #[sqlx(skip)]
    pub thread_has_attachments: bool,
}

/// Email history of a deal.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DealEmails {
    /// What the deal page lists: the deal's conversations and those with the
    /// customer; when the customer has only this deal, also any mail to or
    /// from their addresses. Newest first.
    pub emails: Vec<HistoryEmail>,
    /// Mail with the customer's addresses, when some of it is outside `emails`.
    pub customer_emails: Vec<HistoryEmail>,
    /// The deal's and the customer's conversations only (the History tab).
    pub thread_emails: Vec<HistoryEmail>,
}

const SELECT: &str =
    "SELECT e.id, CAST(e.deal_id AS SIGNED) AS deal_id, e.thread_id, e.subject, e.body,
    e.sent_at, COUNT(er.message_id) AS read_count,
    (SELECT COUNT(*) FROM email_attachments ea WHERE ea.email_id = e.id) > 0 AS has_attachments,
    e.sender_user_id, e.sender_email, u_sender.name AS sender_name, e.employee_read_at
  FROM emails e
  LEFT JOIN users u_sender ON u_sender.id = e.sender_user_id";

/// Messages the customer is a participant of. Only those messages, not their
/// whole threads: a thread that also holds another customer's mail (a reused
/// thread id, a mis-threaded reply) would otherwise show all of it.
pub(crate) const CUSTOMER_PARTICIPANT_EMAILS: &str =
    "e.id IN (SELECT ep.email_id FROM email_participants ep WHERE ep.customer_id = ?)";

/// The company check: received mail by the company it arrived for, sent mail
/// by the sender's company. Matching a customer's address alone would also
/// find what they wrote to other companies.
pub(crate) fn history_sql(filter: &str, args: Vec<Arg>) -> Sql {
    Sql::new(
        format!(
            "{SELECT}
  LEFT JOIN email_reads er ON e.message_id = er.message_id AND {CLIENT_OPEN_MIN_DELAY}
  WHERE e.deleted_at IS NULL AND e.thread_id IS NOT NULL
    AND IF(e.sender_user_id IS NULL, e.company_id, u_sender.company_id) = ?
    AND {filter}
  GROUP BY e.id
  ORDER BY e.sent_at DESC, e.id DESC"
        ),
        args,
    )
}

async fn deal_thread_emails(
    pool: &MySqlPool,
    company_id: i32,
    deal_id: i64,
    customer_id: i32,
) -> Result<Vec<HistoryEmail>, sqlx::Error> {
    let by_deal: Vec<HistoryEmail> = history_sql(
        "e.thread_id IN (SELECT DISTINCT e2.thread_id FROM emails e2
           WHERE e2.deal_id = ? AND e2.thread_id IS NOT NULL AND e2.deleted_at IS NULL)",
        vec![company_id.into(), deal_id.into()],
    )
    .query_as()
    .fetch_all(pool)
    .await?;
    let by_customer: Vec<HistoryEmail> = history_sql(
        CUSTOMER_PARTICIPANT_EMAILS,
        vec![company_id.into(), customer_id.into()],
    )
    .query_as()
    .fetch_all(pool)
    .await?;
    Ok(merge(by_deal, by_customer))
}

pub(crate) async fn address_emails(
    pool: &MySqlPool,
    company_id: i32,
    addresses: &[String],
) -> Result<Vec<HistoryEmail>, sqlx::Error> {
    if addresses.is_empty() {
        return Ok(Vec::new());
    }
    let marks = vec!["?"; addresses.len()].join(", ");
    let mut args: Vec<Arg> = vec![company_id.into()];
    for _ in 0..4 {
        args.extend(addresses.iter().map(|a| Arg::from(a.clone())));
    }
    history_sql(
        &format!(
            "(LOWER(TRIM(SUBSTRING_INDEX(SUBSTRING_INDEX(e.sender_email, '<', -1), '>', 1))) IN ({marks})
        OR (e.sender_email NOT LIKE '%<%' AND LOWER(TRIM(e.sender_email)) IN ({marks}))
        OR LOWER(TRIM(SUBSTRING_INDEX(SUBSTRING_INDEX(e.receiver_email, '<', -1), '>', 1))) IN ({marks})
        OR (e.receiver_email NOT LIKE '%<%' AND LOWER(TRIM(e.receiver_email)) IN ({marks})))"
        ),
        args,
    )
    .query_as()
    .fetch_all(pool)
    .await
}

/// `mergeEmailHistoryById`: first list wins, then newest first.
pub(crate) fn merge(first: Vec<HistoryEmail>, extra: Vec<HistoryEmail>) -> Vec<HistoryEmail> {
    let mut seen: HashSet<i32> = first.iter().map(|e| e.id).collect();
    let mut merged = first;
    for email in extra {
        if seen.insert(email.id) {
            merged.push(email);
        }
    }
    merged.sort_by_key(|a| std::cmp::Reverse(a.sent_at));
    merged
}

pub(crate) fn with_thread_attachments(mut emails: Vec<HistoryEmail>) -> Vec<HistoryEmail> {
    let threads: HashSet<String> = emails
        .iter()
        .filter(|e| e.has_attachments)
        .map(|e| e.thread_id.clone())
        .collect();
    for email in &mut emails {
        email.thread_has_attachments = threads.contains(&email.thread_id);
    }
    emails
}

#[derive(FromRow)]
struct MissingSend {
    id: i32,
    deal_id: i64,
    user_id: i32,
    company_id: i32,
    sent_at: Option<NaiveDateTime>,
    message_id: Option<String>,
    template_subject: String,
    template_body: String,
    recipient_email: Option<String>,
    sender_email: Option<String>,
    sender_name: Option<String>,
    email_name: Option<String>,
    customer_name: Option<String>,
    company_name: Option<String>,
    company_domain: Option<String>,
}

const RECIPIENT: &str = "COALESCE(NULLIF(TRIM(ce.email), ''),
    (SELECT NULLIF(TRIM(ce2.email), '') FROM customers_emails ce2
     WHERE ce2.customer_id = se.customer_id AND TRIM(IFNULL(ce2.email, '')) <> ''
     ORDER BY ce2.id LIMIT 1))";

fn first_name(full: Option<&str>) -> String {
    full.unwrap_or_default()
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string()
}

fn subdomain(domain: Option<&str>) -> String {
    let domain = domain.unwrap_or_default().trim();
    let bare = domain
        .strip_prefix("https://")
        .or_else(|| domain.strip_prefix("http://"))
        .unwrap_or(domain);
    bare.split('.').next().unwrap_or_default().to_lowercase()
}

/// `{{ customer.first_name }}` and friends, as the drip sender filled them.
fn fill_template(row: &MissingSend) -> String {
    let mut vars: HashMap<&str, String> = HashMap::new();
    let customer = row.customer_name.as_deref().unwrap_or_default().trim();
    if !customer.is_empty() {
        vars.insert("customer.name", customer.to_string());
    }
    let customer_first = first_name(row.customer_name.as_deref());
    if !customer_first.is_empty() {
        vars.insert("customer.first_name", customer_first);
    }
    let sender = row.sender_name.as_deref().unwrap_or_default().trim();
    if !sender.is_empty() {
        vars.insert("user.name", sender.to_string());
    }
    let sender_first = first_name(row.sender_name.as_deref());
    if !sender_first.is_empty() {
        vars.insert("user.first_name", sender_first);
    }
    let sender_email = row.sender_email.as_deref().unwrap_or_default().trim();
    if !sender_email.is_empty() {
        vars.insert("user.email", sender_email.to_string());
    }
    let company = row.company_name.as_deref().unwrap_or_default().trim();
    if !company.is_empty() {
        vars.insert("company.name", company.to_string());
    }
    let slug = subdomain(row.company_domain.as_deref());
    let inventory = if slug.is_empty() {
        format!(
            "https://granite-manager.com/customer/{}/stones",
            row.company_id
        )
    } else {
        vars.insert("company.subdomain", slug.clone());
        vars.insert("company.id", row.company_id.to_string());
        format!(
            "https://{slug}.granite-manager.com/customer/{}/stones",
            row.company_id
        )
    };
    vars.insert("customer.inventory_link", inventory);

    let template = &row.template_body;
    let mut out = String::with_capacity(template.len());
    let mut rest = template.as_str();
    while let Some(open) = rest.find("{{") {
        out.push_str(&rest[..open]);
        let after = &rest[open + 2..];
        let Some(close) = after.find("}}") else {
            out.push_str(&rest[open..]);
            rest = "";
            break;
        };
        let key = after[..close].trim();
        let valid = !key.is_empty()
            && key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.');
        match vars.get(key).filter(|v| valid && !v.is_empty()) {
            Some(value) => out.push_str(value),
            None => out.push_str(&rest[open..open + 2 + close + 2]),
        }
        rest = &after[close + 2..];
    }
    out.push_str(rest);
    out
}

/// The message id to store: the SES id the customer replies to, at most
/// 500 characters (`historyMessageId`).
fn history_message_id(existing: Option<&str>) -> String {
    let trimmed = existing
        .unwrap_or_default()
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>');
    if trimmed.is_empty() {
        return uuid::Uuid::new_v4().to_string();
    }
    if trimmed.chars().count() <= 500 {
        return trimmed.to_string();
    }
    match trimmed.find('@') {
        Some(at) if at > 0 && at <= 500 => trimmed[..at].to_string(),
        _ => trimmed.chars().take(500).collect(),
    }
}

/// Records sent list drips of the customer (of one deal, when given) that
/// never made it into `emails`, so the history shows them
/// (`ensureSentScheduledEmailsInHistory`). Best effort.
pub(crate) async fn record_missing_sends(pool: &MySqlPool, deal_id: Option<i64>, customer_id: i32) {
    let missing: Result<Vec<MissingSend>, sqlx::Error> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT se.id, CAST(se.deal_id AS SIGNED) AS deal_id, se.user_id, se.company_id, se.sent_at,
                se.message_id, et.template_subject, et.template_body, {RECIPIENT} AS recipient_email,
                u.email AS sender_email, u.name AS sender_name, u.email_name, c.name AS customer_name,
                co.name AS company_name, co.domain AS company_domain
         FROM scheduled_emails se
         JOIN email_templates et ON et.id = se.template_id
         JOIN customers c ON c.id = se.customer_id
         LEFT JOIN customers_emails ce ON ce.id = c.email_id
         LEFT JOIN users u ON u.id = se.user_id
         LEFT JOIN company co ON co.id = se.company_id
         WHERE se.status = 'sent' AND se.deal_id IS NOT NULL AND se.user_id IS NOT NULL
           AND TRIM(IFNULL({RECIPIENT}, '')) <> ''
           AND (se.message_id IS NULL OR se.message_id = ''
                OR NOT EXISTS (SELECT 1 FROM emails e WHERE e.deleted_at IS NULL AND e.message_id = se.message_id))
           AND NOT EXISTS (SELECT 1 FROM emails e
                WHERE e.deleted_at IS NULL AND e.deal_id = se.deal_id AND e.sender_user_id = se.user_id
                  AND e.subject = et.template_subject AND e.sent_at IS NOT NULL AND se.sent_at IS NOT NULL
                  AND ABS(TIMESTAMPDIFF(SECOND, e.sent_at, se.sent_at)) <= 120)
           AND se.customer_id = ? AND (? IS NULL OR se.deal_id = ?)
         ORDER BY se.id"
    )))
    .bind(customer_id)
    .bind(deal_id)
    .bind(deal_id)
    .fetch_all(pool)
    .await;
    let Ok(missing) = missing else {
        return;
    };
    for row in missing {
        let _ = record_send(pool, &row).await;
    }
}

async fn record_send(pool: &MySqlPool, row: &MissingSend) -> Result<(), sqlx::Error> {
    let recipient = normalize_address(row.recipient_email.as_deref());
    if recipient.is_empty() {
        return Ok(());
    }
    let sender = normalize_address(Some(&from_email(
        row.company_domain.as_deref(),
        row.sender_email.as_deref().unwrap_or_default(),
    )));
    let html = fill_template(row);
    let message_id = history_message_id(row.message_id.as_deref());
    let mut tx = pool.begin().await?;
    let email_id = sqlx::query(
        "INSERT INTO emails (sender_user_id, subject, body, html_body, message_id, sender_email,
           receiver_email, thread_id, deal_id, company_id, sent_at, is_automated)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1)",
    )
    .bind(row.user_id)
    .bind(&row.template_subject)
    .bind(&html)
    .bind(&html)
    .bind(&message_id)
    .bind(&sender)
    .bind(&recipient)
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(row.deal_id)
    .bind(row.company_id)
    .bind(row.sent_at)
    .execute(&mut *tx)
    .await?
    .last_insert_id();
    let user_id: Option<i32> = sqlx::query_scalar(
        "SELECT id FROM users WHERE is_deleted = 0 AND company_id = ? AND LOWER(TRIM(email)) = ? LIMIT 1",
    )
    .bind(row.company_id)
    .bind(&sender)
    .fetch_optional(&mut *tx)
    .await?;
    let customer_id: Option<i32> = sqlx::query_scalar(
        "SELECT MIN(c.id) FROM customers c JOIN customers_emails ce ON ce.customer_id = c.id
         WHERE c.company_id = ? AND c.deleted_at IS NULL
           AND LOWER(TRIM(SUBSTRING_INDEX(SUBSTRING_INDEX(ce.email, '<', -1), '>', 1))) = ?",
    )
    .bind(row.company_id)
    .bind(&recipient)
    .fetch_one(&mut *tx)
    .await?;
    let from_name = row
        .email_name
        .clone()
        .or_else(|| row.sender_name.clone())
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty());
    let to_name = row
        .customer_name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty());
    sqlx::query(
        "INSERT INTO email_participants (email_id, type, email, display_name, user_id, customer_id, position)
         VALUES (?, 'from', ?, ?, ?, NULL, 0), (?, 'to', ?, ?, NULL, ?, 0)",
    )
    .bind(email_id)
    .bind(&sender)
    .bind(from_name)
    .bind(user_id)
    .bind(email_id)
    .bind(&recipient)
    .bind(to_name)
    .bind(customer_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE scheduled_emails SET message_id = ? WHERE id = ?")
        .bind(&message_id)
        .bind(row.id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await
}

/// Get a deal's email history
///
/// The deal's conversations and every conversation with its customer, plus
/// (when the customer has no other deal) any mail to or from the customer's
/// addresses; `customer_emails` lists the address mail when some of it is
/// not in `emails`. Messages carry real open counts. Before reading, sent
/// list drips missing from the history are recorded (so the page always
/// shows them). Open a conversation with `GET /v1/emails/threads/{thread_id}`.
#[utoipa::path(
    get,
    path = "/v1/deals/{id}/emails",
    operation_id = "list_deal_emails",
    tag = "emails",
    params(("id" = i64, Path, description = "Deal id")),
    responses(
        (status = 200, description = "The history", body = DealEmails),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such live deal in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn deal_emails(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<DealEmails>> {
    let deal: Option<(i32, i64)> = sqlx::query_as(
        "SELECT d.customer_id,
                (SELECT COUNT(*) FROM deals d2 WHERE d2.customer_id = d.customer_id AND d2.deleted_at IS NULL)
         FROM deals d JOIN customers c ON d.customer_id = c.id
         WHERE d.id = ? AND d.deleted_at IS NULL AND c.company_id = ?",
    )
    .bind(id)
    .bind(user.company_id)
    .fetch_optional(&state.pool)
    .await?;
    let (customer_id, customer_deals) = deal.ok_or(ApiError::NotFound)?;

    let addresses: Vec<String> = sqlx::query_scalar(
        "SELECT ce.email FROM customers_emails ce JOIN customers c ON c.id = ce.customer_id
         WHERE ce.customer_id = ? ORDER BY ce.id = c.email_id DESC, ce.created_at, ce.id",
    )
    .bind(customer_id)
    .fetch_all(&state.pool)
    .await?;
    let addresses: Vec<String> = addresses
        .into_iter()
        .map(|a| a.trim().to_lowercase())
        .filter(|a| !a.is_empty())
        .collect();

    record_missing_sends(&state.pool, Some(id), customer_id).await;

    let thread_emails = with_thread_attachments(
        deal_thread_emails(&state.pool, user.company_id, id, customer_id).await?,
    );
    let by_address =
        with_thread_attachments(address_emails(&state.pool, user.company_id, &addresses).await?);
    let use_addresses = !addresses.is_empty() && customer_deals == 1;
    let emails = merge(
        thread_emails.clone(),
        if use_addresses {
            by_address.clone()
        } else {
            Vec::new()
        },
    );
    let listed: HashSet<i32> = emails.iter().map(|e| e.id).collect();
    let customer_emails = if by_address.iter().any(|e| !listed.contains(&e.id)) {
        by_address
    } else {
        Vec::new()
    };
    Ok(Json(DealEmails {
        emails,
        customer_emails,
        thread_emails,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(body: &str) -> MissingSend {
        MissingSend {
            id: 1,
            deal_id: 1,
            user_id: 1,
            company_id: 7,
            sent_at: None,
            message_id: None,
            template_subject: String::new(),
            template_body: body.to_string(),
            recipient_email: None,
            sender_email: Some("rep@acme.test".into()),
            sender_name: Some("Rita Rep".into()),
            email_name: None,
            customer_name: Some("Alice Lead".into()),
            company_name: Some("Acme".into()),
            company_domain: Some("https://acme.granite-manager.com".into()),
        }
    }

    #[test]
    fn templates_fill_known_variables_only() {
        assert_eq!(
            fill_template(&row(
                "Hi {{ customer.first_name }}, {{user.name}} of {{company.name}}: {{ customer.inventory_link }} {{unknown}}"
            )),
            "Hi Alice, Rita Rep of Acme: https://acme.granite-manager.com/customer/7/stones {{unknown}}"
        );
    }

    #[test]
    fn message_ids_are_kept_short() {
        assert_eq!(history_message_id(Some("<abc@ses>")), "abc@ses");
        assert_eq!(history_message_id(Some(&"x".repeat(600))).len(), 500);
        assert_eq!(history_message_id(None).len(), 36);
    }
}
