//! The Granite Manager sales mailbox (`sales@granitemanager.com`,
//! `sales@granite-manager.com`) for super admins, across every company.
//!
//! - `GET /v1/superuser/sales-emails` → a page of inbox or sent threads
//! - `GET /v1/superuser/sales-emails/{thread_id}` → one conversation
//!
//! Replaces the Remix `admin.superuser-dashboard.emails` loaders and
//! `superuserSalesMailbox.server.ts`.

use super::schemas::{ThreadCustomer, ThreadDetail};
use super::sql::{Arg, Mailbox, Sql, list_search, normalized};
use super::thread::{thread_contents, thread_customer};
use crate::auth::CurrentUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::MultiQuery;
use crate::openapi::{IntoParams, ToSchema};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Threads per page.
const PAGE_SIZE: i64 = 50;
/// The mailbox's addresses, as SQL.
const ADDRESSES: &str = "('sales@granitemanager.com', 'sales@granite-manager.com')";
/// The address the page shows.
pub const DISPLAY_ADDRESS: &str = "sales@granitemanager.com";

/// `e` was sent from the mailbox (its sender or a `from` header).
fn sent_from(alias: &str) -> String {
    format!(
        "({} IN {ADDRESSES} OR EXISTS (SELECT 1 FROM email_participants ep_from
            WHERE ep_from.email_id = {alias}.id AND ep_from.type = 'from' AND ep_from.email IN {ADDRESSES}))",
        normalized(&format!("{alias}.sender_email"))
    )
}

/// `e` was sent to the mailbox (its receiver or a `to`/`cc` header).
fn sent_to(alias: &str) -> String {
    format!(
        "({} IN {ADDRESSES} OR EXISTS (SELECT 1 FROM email_participants ep_to
            WHERE ep_to.email_id = {alias}.id AND ep_to.type IN ('to', 'cc') AND ep_to.email IN {ADDRESSES}))",
        normalized(&format!("{alias}.receiver_email"))
    )
}

/// A folder of the mailbox.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum SalesFolder {
    /// Mail to the mailbox not sent from it.
    #[default]
    Inbox,
    /// Mail sent from it.
    Sent,
}

/// Query of `GET /v1/superuser/sales-emails`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SalesQuery {
    /// `inbox` (default) or `sent`.
    #[serde(default)]
    #[param(inline)]
    pub folder: SalesFolder,
    /// Page, from 1.
    pub page: Option<i64>,
    /// Matches subject, text and addresses.
    #[serde(default)]
    pub search: String,
}

/// The latest message of a thread in the folder.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct SalesEmail {
    /// The message.
    pub id: i64,
    /// Its thread.
    pub thread_id: String,
    /// Subject.
    pub subject: Option<String>,
    /// Text.
    pub body: Option<String>,
    /// When (UTC, ISO 8601).
    pub sent_at: Option<String>,
    /// From.
    pub sender_email: Option<String>,
    /// To.
    pub receiver_email: Option<String>,
    /// The CRM user who sent it.
    pub sender_user_id: Option<i64>,
    /// It has attachments.
    pub has_attachments: bool,
    /// Any message of the thread has attachments.
    pub thread_has_attachments: bool,
    /// The sender's user or customer name.
    pub sender_name: Option<String>,
    /// The receiver's user or customer name.
    pub receiver_name: Option<String>,
}

/// `GET /v1/superuser/sales-emails`.
#[derive(Debug, Serialize, ToSchema)]
pub struct SalesPage {
    /// Threads, latest message first.
    pub emails: Vec<SalesEmail>,
    /// Threads in the folder (and search).
    pub total_count: i64,
    /// The page.
    pub current_page: i64,
    /// Threads per page.
    pub page_size: i64,
    /// The mailbox's address.
    pub mailbox: String,
}

const fn super_only(user: &crate::auth::session::SessionUser) -> ApiResult<()> {
    if user.is_superuser {
        Ok(())
    } else {
        Err(ApiError::Forbidden)
    }
}

/// List the sales mailbox
///
/// A page (50 threads) of the sales mailbox's inbox (mail to it, not from it)
/// or sent folder, across every company, ranked by each thread's latest
/// message in the folder. Super admins only.
#[crate::openapi::path(
    get,
    path = "/v1/superuser/sales-emails",
    operation_id = "list_sales_mailbox",
    tag = "superuser",
    params(SalesQuery),
    responses(
        (status = 200, description = "A page of threads", body = SalesPage),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a super admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    MultiQuery(query): MultiQuery<SalesQuery>,
) -> ApiResult<Json<SalesPage>> {
    super_only(&user)?;
    let pool = &state.pool;
    let folder = match query.folder {
        SalesFolder::Inbox => format!("({} AND NOT {})", sent_to("e"), sent_from("e")),
        SalesFolder::Sent => sent_from("e"),
    };
    let search = list_search(&query.search, None);
    let matching = format!(
        "e.deleted_at IS NULL AND e.is_draft = 0 AND e.thread_id IS NOT NULL AND {folder}{}",
        search.sql
    );
    let total_count: i64 = Sql::new(
        format!("SELECT COUNT(DISTINCT e.thread_id) FROM emails e WHERE {matching}"),
        search.args.clone(),
    )
    .query_scalar()
    .fetch_one(pool)
    .await?;
    let page = query.page.filter(|page| *page >= 1).unwrap_or(1);
    let mut args = search.args.clone();
    args.push(Arg::from(PAGE_SIZE));
    args.push(Arg::from((page - 1) * PAGE_SIZE));
    let threads: Vec<String> = Sql::new(
        format!(
            "SELECT e.thread_id FROM emails e WHERE {matching}
              GROUP BY e.thread_id ORDER BY MAX(e.sent_at) DESC, e.thread_id LIMIT ? OFFSET ?"
        ),
        args,
    )
    .query_scalar()
    .fetch_all(pool)
    .await?;
    if threads.is_empty() {
        return Ok(Json(SalesPage {
            emails: Vec::new(),
            total_count,
            current_page: page,
            page_size: PAGE_SIZE,
            mailbox: DISPLAY_ADDRESS.into(),
        }));
    }
    let customer_email = normalized("ce.email");
    let name_of = |column: &str, users: &str| {
        format!(
            "COALESCE((SELECT u.name FROM users u WHERE {users} LIMIT 1),
                      (SELECT c.name FROM customers c JOIN customers_emails ce ON ce.customer_id = c.id
                        WHERE c.deleted_at IS NULL AND {customer_email} = {} LIMIT 1))",
            normalized(column)
        )
    };
    let placeholders = vec!["?"; threads.len()].join(", ");
    let mut args = search.args;
    args.extend(threads.iter().map(|thread| Arg::from(thread.as_str())));
    let rows: Vec<SalesEmail> = Sql::new(
        format!(
            "SELECT CAST(e.id AS SIGNED) AS id, e.thread_id, e.subject, e.body,
                    DATE_FORMAT(e.sent_at, '%Y-%m-%dT%H:%i:%sZ') AS sent_at,
                    e.sender_email, e.receiver_email, CAST(e.sender_user_id AS SIGNED) AS sender_user_id,
                    EXISTS (SELECT 1 FROM email_attachments ea WHERE ea.email_id = e.id) AS has_attachments,
                    EXISTS (SELECT 1 FROM email_attachments ea2 JOIN emails e3 ON ea2.email_id = e3.id
                             WHERE e3.thread_id = e.thread_id) AS thread_has_attachments,
                    {} AS sender_name, {} AS receiver_name
               FROM emails e
              WHERE {matching} AND e.thread_id IN ({placeholders})
              ORDER BY e.sent_at DESC, e.id DESC",
            name_of("e.sender_email", "u.id = e.sender_user_id"),
            name_of(
                "e.receiver_email",
                &format!(
                    "u.is_deleted = 0 AND {} = {}",
                    normalized("u.email"),
                    normalized("e.receiver_email")
                )
            ),
        ),
        args,
    )
    .query_as()
    .fetch_all(pool)
    .await?;
    // The latest message of each thread, newest thread first.
    let mut seen = HashSet::new();
    let emails = rows
        .into_iter()
        .filter(|row| seen.insert(row.thread_id.clone()))
        .collect();
    Ok(Json(SalesPage {
        emails,
        total_count,
        current_page: page,
        page_size: PAGE_SIZE,
        mailbox: DISPLAY_ADDRESS.into(),
    }))
}

/// Open a sales mailbox conversation
///
/// Every message of a thread that has mail to or from the sales mailbox,
/// with attachments and addresses, and who it is with. Read only: opening
/// it marks nothing read. Super admins only.
#[crate::openapi::path(
    get,
    path = "/v1/superuser/sales-emails/{thread_id}",
    operation_id = "get_sales_mailbox_thread",
    tag = "superuser",
    params(("thread_id" = String, Path, description = "Conversation id")),
    responses(
        (status = 200, description = "The conversation", body = ThreadDetail),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a super admin", body = ErrorBody),
        (status = 404, description = "No sales mailbox mail in the thread", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn thread(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(thread_id): Path<String>,
) -> ApiResult<Json<ThreadDetail>> {
    super_only(&user)?;
    let pool = &state.pool;
    let allowed: Option<i32> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT 1 FROM emails e
          WHERE e.thread_id = ? AND e.deleted_at IS NULL AND e.is_draft = 0
            AND ({} OR {}) LIMIT 1",
        sent_from("e"),
        sent_to("e")
    )))
    .bind(&thread_id)
    .fetch_optional(pool)
    .await?;
    if allowed.is_none() {
        return Err(ApiError::NotFound);
    }
    // Seen from the mailbox: the other party is the customer. The thread's
    // company is the one of its mail, for the customer match.
    let company_id: Option<i32> = sqlx::query_scalar(
        "SELECT company_id FROM emails WHERE thread_id = ? AND company_id IS NOT NULL
          ORDER BY sent_at DESC LIMIT 1",
    )
    .bind(&thread_id)
    .fetch_optional(pool)
    .await?;
    let mailbox = Mailbox {
        user_id: user.id,
        company_id: company_id.unwrap_or(0),
        email: DISPLAY_ADDRESS.into(),
    };
    let (deal_id, customer) = thread_customer(pool, &thread_id, &mailbox).await?;
    let (messages, participants) =
        thread_contents(pool, &state.storage, &thread_id, false, None).await?;
    Ok(Json(ThreadDetail {
        thread_id: thread_id.clone(),
        deal_id,
        subject: messages.first().and_then(|m| m.subject.clone()),
        customer: ThreadCustomer {
            name: if customer.name.trim().is_empty() {
                DISPLAY_ADDRESS.into()
            } else {
                customer.name
            },
            ..customer
        },
        current_user_signature: None,
        sending_email: DISPLAY_ADDRESS.into(),
        messages,
        participants,
        customer_addresses: Vec::new(),
    }))
}
