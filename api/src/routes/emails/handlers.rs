use super::admin_list::{AdminMailbox, admin_counts, admin_page};
use super::list::{folder_counts, list_page, suggestions};
use super::schemas::{
    CountsQuery, EmailFolder, FolderCounts, ListQuery, MailboxPage, Purged, SuggestQuery,
    SuggestScope, SuggestionRow, ThreadDetail, ThreadIds, ThreadQuery, UnreadCount,
};
use super::sql::{
    Arg, Mailbox, Sql, company_scope, own_threads, received_message, thread_in_inbox,
    unread_incoming,
};
use super::thread::{readable, thread_detail};
use crate::auth::EmployeeUser;
use crate::auth::session::SessionUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::{JsonBody, MultiQuery};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use sqlx::MySqlPool;

/// At most this many threads per delete or purge, so a buggy client cannot
/// wipe a folder in one request.
const MAX_THREADS: usize = 50;

pub fn mailbox(user: &SessionUser) -> Mailbox {
    Mailbox {
        user_id: user.id,
        company_id: user.company_id,
        email: user.email.trim().to_lowercase(),
    }
}

/// List the caller's email threads
///
/// One page (25 threads) of a folder: `inbox` (mail addressed to the caller,
/// ranked by the latest message to them), `sent`, `trash` (ranked by
/// deletion) or `drafts`. Every message of each listed thread comes back,
/// with names, attachment flags and the recipient's last open. `search`
/// matches subject, text, addresses, and the addresses of customers whose
/// name matches; in the inbox it matches any message of the thread. Open a
/// thread with `GET /v1/emails/threads/{thread_id}`.
///
/// `scope=admin` (admins only) lists every employee's mail of the company
/// instead, optionally only one user's (`sales_rep`); drafts read as the
/// inbox there.
#[crate::openapi::path(
    get,
    path = "/v1/emails/threads",
    operation_id = "list_email_threads",
    tag = "emails",
    params(ListQuery),
    responses(
        (status = 200, description = "A page of the folder", body = MailboxPage),
        (status = 400, description = "Unknown folder or malformed page", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Not an employee, or the admin scope without being an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    MultiQuery(query): MultiQuery<ListQuery>,
) -> ApiResult<Json<MailboxPage>> {
    if query.scope == Some(SuggestScope::Admin) {
        if !(user.is_admin || user.is_superuser) {
            return Err(ApiError::Forbidden);
        }
        let folder = match query.folder.unwrap_or_default() {
            EmailFolder::Drafts => EmailFolder::Inbox,
            folder => folder,
        };
        let mailbox = AdminMailbox {
            company_id: user.company_id,
            folder,
            sales_rep: query.sales_rep,
        };
        let page = query.page.filter(|p| *p >= 1).unwrap_or(1);
        let search = query.search.as_deref().unwrap_or_default();
        return Ok(Json(admin_page(&state.pool, &mailbox, page, search).await?));
    }
    Ok(Json(list_page(&state.pool, &mailbox(&user), &query).await?))
}

/// Count the caller's email folders
///
/// Unread inbox threads (customer mail to the caller or on their deals, and
/// coworkers' mail to them), threads in the trash and drafts. `scope=admin`
/// (admins only) counts the whole company's unread inbox and trash
/// (`sales_rep`: one user's), with no drafts.
#[crate::openapi::path(
    get,
    path = "/v1/emails/folder-counts",
    operation_id = "count_email_folders",
    tag = "emails",
    params(CountsQuery),
    responses(
        (status = 200, description = "Counts", body = FolderCounts),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Not an employee, or the admin scope without being an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn counts(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    MultiQuery(query): MultiQuery<CountsQuery>,
) -> ApiResult<Json<FolderCounts>> {
    if query.scope == Some(SuggestScope::Admin) {
        if !(user.is_admin || user.is_superuser) {
            return Err(ApiError::Forbidden);
        }
        return Ok(Json(
            admin_counts(&state.pool, user.company_id, query.sales_rep).await?,
        ));
    }
    Ok(Json(folder_counts(&state.pool, &mailbox(&user)).await?))
}

/// Get the inbox badge
///
/// Unread inbox threads and the newest unread message id (to notice new
/// mail between polls). Always 0 when the company has no inbound email.
#[crate::openapi::path(
    get,
    path = "/v1/emails/unread",
    operation_id = "get_unread_email_count",
    tag = "emails",
    responses(
        (status = 200, description = "The badge", body = UnreadCount),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn unread(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<UnreadCount>> {
    if !has_inbound_email(&state.pool, user.company_id).await? {
        return Ok(Json(UnreadCount {
            count: 0,
            latest_inbound_id: None,
        }));
    }
    let unread = unread_inbox(&mailbox(&user));
    let (count, latest): (i64, Option<i64>) = Sql::new(
        format!(
            "SELECT COUNT(DISTINCT e.thread_id), CAST(MAX(e.id) AS SIGNED) {}",
            unread.sql
        ),
        unread.args,
    )
    .query_as()
    .fetch_one(&state.pool)
    .await?;
    Ok(Json(UnreadCount {
        count,
        latest_inbound_id: latest,
    }))
}

/// The company receives email: it has an inbound email domain.
pub(super) async fn has_inbound_email(pool: &MySqlPool, company_id: i32) -> sqlx::Result<bool> {
    let domain: Option<Option<String>> =
        sqlx::query_scalar("SELECT domain FROM company WHERE id = ?")
            .bind(company_id)
            .fetch_optional(pool)
            .await?;
    Ok(domain.flatten().is_some_and(|d| !d.trim().is_empty()))
}

/// `FROM ... WHERE ...` of the caller's unread inbox messages (`e`), as the
/// badge counts them; the company's inbound email domain is checked apart.
/// `POST /v1/emails/inbound-push` pushes a new email to the users it matches.
pub(super) fn unread_inbox(mailbox: &Mailbox) -> Sql {
    let scope = company_scope("e", mailbox.company_id);
    let incoming = unread_incoming("e", mailbox);
    let in_inbox = thread_in_inbox("e", mailbox);
    let mut args: Vec<Arg> = vec![mailbox.company_id.into()];
    args.extend(scope.args);
    args.extend(incoming.args);
    args.extend(in_inbox.args);
    Sql::new(
        format!(
            "FROM emails e
             LEFT JOIN deals d ON d.id = COALESCE(
               e.deal_id,
               (SELECT MAX(td.deal_id) FROM emails td
                WHERE td.thread_id = e.thread_id AND td.deleted_at IS NULL AND td.deal_id IS NOT NULL
                  AND (td.company_id = ? OR td.company_id IS NULL))
             ) AND d.deleted_at IS NULL
             WHERE e.deleted_at IS NULL AND e.is_draft = 0 AND e.thread_id IS NOT NULL
               AND e.employee_read_at IS NULL AND {} AND {} AND {}",
            scope.sql, incoming.sql, in_inbox.sql
        ),
        args,
    )
}

/// Get an email conversation
///
/// The thread's messages oldest first (on a deal: that deal's messages and
/// unassigned ones), attachments with links, header participants, who it is
/// with, and the caller's signature and sending address. `folder=trash`
/// shows the deleted messages. With `deal_id` the conversation is opened on
/// that deal: only its (and unassigned) messages, the deal's customer, and an
/// empty conversation for a thread id that has no mail yet (composing a new
/// one). Any thread of the caller's company can be read. Opening it does not
/// mark it read: call
/// `POST /v1/emails/threads/{thread_id}/read`.
#[crate::openapi::path(
    get,
    path = "/v1/emails/threads/{thread_id}",
    operation_id = "get_email_thread",
    tag = "emails",
    params(("thread_id" = String, Path, description = "Conversation id"), ThreadQuery),
    responses(
        (status = 200, description = "The conversation", body = ThreadDetail),
        (status = 400, description = "Unknown folder", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such thread (or deal) in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get_thread(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(thread_id): Path<String>,
    MultiQuery(query): MultiQuery<ThreadQuery>,
) -> ApiResult<Json<ThreadDetail>> {
    let trash = query.folder == Some(EmailFolder::Trash);
    thread_detail(
        &state.pool,
        &state.storage,
        &mailbox(&user),
        &thread_id,
        trash,
        query.deal_id,
    )
    .await?
    .map(Json)
    .ok_or(ApiError::NotFound)
}

/// Mark an open conversation read
///
/// Stamps every unread live message of the thread that the caller did not
/// send (what opening the conversation used to do). Any thread of the
/// caller's company.
#[crate::openapi::path(
    post,
    path = "/v1/emails/threads/{thread_id}/read",
    operation_id = "read_email_thread",
    tag = "emails",
    params(("thread_id" = String, Path, description = "Conversation id")),
    responses(
        (status = 204, description = "Marked"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such thread in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn read_thread(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(thread_id): Path<String>,
) -> ApiResult<StatusCode> {
    if !readable(&state.pool, &thread_id, user.company_id).await? {
        return Err(ApiError::NotFound);
    }
    let received = received_message(user.id);
    let mut args: Vec<Arg> = vec![thread_id.into()];
    args.extend(received.args);
    Sql::new(
        format!(
            "UPDATE emails SET employee_read_at = NOW()
             WHERE deleted_at IS NULL AND thread_id = ? AND employee_read_at IS NULL AND {}",
            received.sql
        ),
        args,
    )
    .query()
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

fn clean_ids(body: ThreadIds, max: Option<usize>) -> ApiResult<Vec<String>> {
    let ids: Vec<String> = body
        .thread_ids
        .into_iter()
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
        .collect();
    if ids.is_empty() {
        return Err(ApiError::BadRequest("Invalid threadIds".into()));
    }
    if let Some(max) = max
        && ids.len() > max
    {
        return Err(ApiError::BadRequest(format!(
            "Cannot delete more than {max} conversations at once."
        )));
    }
    Ok(ids)
}

/// `UPDATE emails SET <set> WHERE thread_id IN (ids) AND <extra> AND own threads`.
async fn update_threads(
    state: &AppState,
    user: &SessionUser,
    ids: &[String],
    set: &str,
    extra: Option<Sql>,
) -> ApiResult<()> {
    let owned = own_threads("thread_id", &mailbox(user));
    let marks = vec!["?"; ids.len()].join(",");
    let mut args: Vec<Arg> = ids.iter().map(|id| Arg::from(id.clone())).collect();
    let extra = extra.unwrap_or_default();
    args.extend(extra.args);
    args.extend(owned.args);
    Sql::new(
        format!(
            "UPDATE emails SET {set} WHERE thread_id IN ({marks}){} AND {}",
            extra.sql, owned.sql
        ),
        args,
    )
    .query()
    .execute(&state.pool)
    .await?;
    Ok(())
}

macro_rules! thread_write {
    ($name:ident, $path:literal, $op:literal, $summary:literal, $doc:literal, $max:expr, $set:literal, $extra:expr) => {
        #[doc = $summary]
        #[doc = ""]
        #[doc = $doc]
        #[crate::openapi::path(
            post,
            path = $path,
            operation_id = $op,
            tag = "emails",
            request_body = ThreadIds,
            responses(
                (status = 204, description = "Done"),
                (status = 400, description = "No thread ids, or too many", body = ErrorBody),
                (status = 401, description = "No valid session", body = ErrorBody),
                (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
                (status = 500, description = "Database error", body = ErrorBody),
            ),
            security(("session_cookie" = []))
        )]
        pub async fn $name(
            State(state): State<AppState>,
            EmployeeUser(user): EmployeeUser,
            JsonBody(body): JsonBody<ThreadIds>,
        ) -> ApiResult<StatusCode> {
            let ids = clean_ids(body, $max)?;
            let extra_of: fn(&SessionUser) -> Option<Sql> = $extra;
            let extra = extra_of(&user);
            update_threads(&state, &user, &ids, $set, extra).await?;
            Ok(StatusCode::NO_CONTENT)
        }
    };
}

thread_write!(
    mark_read,
    "/v1/emails/threads/mark-read",
    "mark_email_threads_read",
    "Mark conversations read",
    "Stamps unread live messages the caller did not send, in the caller's own threads (threads they are on or of their deals).",
    None,
    "employee_read_at = NOW()",
    |user: &SessionUser| {
        let received = received_message(user.id);
        Some(Sql::new(
            format!(
                " AND deleted_at IS NULL AND employee_read_at IS NULL AND {}",
                received.sql
            ),
            received.args,
        ))
    }
);

thread_write!(
    mark_unread,
    "/v1/emails/threads/mark-unread",
    "mark_email_threads_unread",
    "Mark conversations unread",
    "Clears the read stamp of every message of the caller's own threads.",
    None,
    "employee_read_at = NULL",
    |_: &SessionUser| None
);

thread_write!(
    delete_threads,
    "/v1/emails/threads/delete",
    "delete_email_threads",
    "Move conversations to the trash",
    "Soft-deletes every message of the caller's own threads, at most 50 threads.",
    Some(MAX_THREADS),
    "deleted_at = NOW()",
    |_: &SessionUser| None
);

thread_write!(
    restore_threads,
    "/v1/emails/threads/restore",
    "restore_email_threads",
    "Restore conversations from the trash",
    "Undeletes every message of the caller's own threads.",
    None,
    "deleted_at = NULL",
    |_: &SessionUser| None
);

/// Erase conversations from the trash
///
/// Permanently deletes the trashed messages of the caller's own threads
/// (at most 50 threads) and their open tracking. Live messages are kept.
#[crate::openapi::path(
    post,
    path = "/v1/emails/threads/purge",
    operation_id = "purge_email_threads",
    tag = "emails",
    request_body = ThreadIds,
    responses(
        (status = 200, description = "Erased", body = Purged),
        (status = 400, description = "No thread ids, or too many", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn purge(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(body): JsonBody<ThreadIds>,
) -> ApiResult<Json<Purged>> {
    let ids = clean_ids(body, Some(MAX_THREADS))?;
    let owned = own_threads("e.thread_id", &mailbox(&user));
    let marks = vec!["?"; ids.len()].join(",");
    let mut args: Vec<Arg> = ids.iter().map(|id| Arg::from(id.clone())).collect();
    args.extend(owned.args);
    let mut tx = state.pool.begin().await?;
    let rows: Vec<(i32, Option<String>)> = Sql::new(
        format!(
            "SELECT e.id, e.message_id FROM emails e
             WHERE e.thread_id IN ({marks}) AND e.deleted_at IS NOT NULL AND {}",
            owned.sql
        ),
        args,
    )
    .query_as()
    .fetch_all(&mut *tx)
    .await?;
    if rows.is_empty() {
        tx.commit().await?;
        return Ok(Json(Purged { purged: 0 }));
    }
    let message_ids: Vec<Arg> = rows
        .iter()
        .filter_map(|(_, m)| m.clone().filter(|m| !m.trim().is_empty()))
        .map(Arg::from)
        .collect();
    if !message_ids.is_empty() {
        let marks = vec!["?"; message_ids.len()].join(",");
        Sql::new(
            format!("DELETE FROM email_reads WHERE message_id IN ({marks})"),
            message_ids,
        )
        .query()
        .execute(&mut *tx)
        .await?;
    }
    let email_ids: Vec<Arg> = rows.iter().map(|(id, _)| Arg::from(*id)).collect();
    let marks = vec!["?"; email_ids.len()].join(",");
    Sql::new(
        format!("DELETE FROM emails WHERE id IN ({marks}) AND deleted_at IS NOT NULL"),
        email_ids,
    )
    .query()
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(Purged { purged: rows.len() }))
}

/// Suggest conversations while typing a search
///
/// The newest matching message of up to 8 conversations of a folder
/// (needs 2 or more characters). `scope=admin` searches the whole company's
/// mail (admins only; `sales_rep` narrows it to one user's mail).
#[crate::openapi::path(
    get,
    path = "/v1/emails/search-suggestions",
    operation_id = "suggest_email_threads",
    tag = "emails",
    params(SuggestQuery),
    responses(
        (status = 200, description = "Up to 8 conversations, newest first", body = Vec<SuggestionRow>),
        (status = 400, description = "Missing term, unknown folder or scope", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Not an employee, or the admin scope without being an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn suggest(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    MultiQuery(query): MultiQuery<SuggestQuery>,
) -> ApiResult<Json<Vec<SuggestionRow>>> {
    if query.scope == Some(SuggestScope::Admin) && !(user.is_admin || user.is_superuser) {
        return Err(ApiError::Forbidden);
    }
    Ok(Json(
        suggestions(&state.pool, &mailbox(&user), &query).await?,
    ))
}
