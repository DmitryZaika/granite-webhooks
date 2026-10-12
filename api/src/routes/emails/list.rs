//! The mailbox: thread list per folder and the folder counts.

use super::names::fill_display_names;
use super::schemas::{
    EmailFolder, FolderCounts, ListQuery, MailboxPage, MailboxRow, SuggestQuery, SuggestScope,
    SuggestionRow,
};
use super::sql::{
    Arg, Mailbox, Sql, client_read_at, company_scope, customer_name_by_email, list_search,
    normalize_search_term, normalized, receiver_condition, sender_condition, unread_incoming,
};
use sqlx::MySqlPool;

pub const PAGE_SIZE: i64 = 25;

/// Addresses of the company's customers whose name contains `search` (at most 500).
async fn customer_addresses(
    pool: &MySqlPool,
    company_id: i32,
    search: &str,
) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT DISTINCT LOWER(TRIM(ce.email)) AS email
         FROM customers c JOIN customers_emails ce ON ce.customer_id = c.id
         WHERE c.company_id = ? AND c.deleted_at IS NULL
           AND ce.email IS NOT NULL AND ce.email <> '' AND c.name LIKE ?
         LIMIT 500",
    )
    .bind(company_id)
    .bind(format!("%{search}%"))
    .fetch_all(pool)
    .await
}

fn alias(sql: &str, to: &str) -> String {
    sql.replace("e.", &format!("{to}."))
}

/// One page of threads of a folder, newest first, every message of each.
#[allow(clippy::too_many_lines)] // One statement built step by step like the Remix loader.
pub async fn list_page(
    pool: &MySqlPool,
    mailbox: &Mailbox,
    query: &ListQuery,
) -> Result<MailboxPage, sqlx::Error> {
    let folder = query.folder.unwrap_or_default();
    let page = query.page.filter(|p| *p >= 1).unwrap_or(1);
    let search = query
        .search
        .as_deref()
        .unwrap_or_default()
        .trim()
        .to_string();

    let extra = if search.is_empty() {
        None
    } else {
        let addresses = customer_addresses(pool, mailbox.company_id, &search).await?;
        if addresses.is_empty() {
            None
        } else {
            let marks = vec!["?"; addresses.len()].join(",");
            let mut args: Vec<Arg> = addresses.iter().map(|a| Arg::from(a.clone())).collect();
            args.extend(addresses.iter().map(|a| Arg::from(a.clone())));
            Some(Sql::new(
                format!(
                    " OR {} IN ({marks}) OR {} IN ({marks})",
                    normalized("e.sender_email"),
                    normalized("e.receiver_email")
                ),
                args,
            ))
        }
    };
    let search_sql = list_search(&search, extra);

    let scope = company_scope("e", mailbox.company_id);
    let folder_condition = if folder == EmailFolder::Drafts {
        Sql::new(
            format!("{} AND e.sender_user_id = ? AND e.is_draft = 1", scope.sql),
            vec![mailbox.company_id.into(), mailbox.user_id.into()],
        )
    } else {
        {
            let address = if folder == EmailFolder::Sent {
                sender_condition("e", mailbox)
            } else {
                receiver_condition("e", mailbox)
            };
            let mut args = scope.args.clone();
            args.extend(address.args);
            Sql::new(
                format!("{} AND e.is_draft = 0 AND {}", scope.sql, address.sql),
                args,
            )
        }
    };
    let deleted = if folder == EmailFolder::Trash {
        "e.deleted_at IS NOT NULL"
    } else {
        "e.deleted_at IS NULL"
    };
    let folder_where = format!("{deleted} AND {}", folder_condition.sql);
    let folder_args = folder_condition.args;
    let is_inbox = folder == EmailFolder::Inbox;

    // Inbox search hits any message of the thread; the row previews the newest.
    let hit = alias(&search_sql.sql, "hit");
    let inbox_search_exists = if hit.is_empty() {
        String::new()
    } else {
        format!(
            " AND EXISTS (SELECT 1 FROM emails hit WHERE hit.thread_id = e.thread_id \
             AND hit.deleted_at IS NULL AND hit.is_draft = 0 {hit})"
        )
    };
    let where_thread_match = if is_inbox {
        format!("{folder_where}{inbox_search_exists}")
    } else {
        format!("{folder_where}{}", search_sql.sql)
    };
    let search_hit_on_thread = if hit.is_empty() {
        String::new()
    } else {
        format!(
            " AND EXISTS (SELECT 1 FROM emails hit WHERE hit.thread_id = e2.thread_id \
             AND hit.deleted_at IS NULL AND hit.is_draft = 0 {hit})"
        )
    };
    let subquery_where = if is_inbox {
        format!("{}{search_hit_on_thread}", alias(&folder_where, "e2"))
    } else {
        alias(&where_thread_match, "e2")
    };
    let mut thread_match_args = folder_args.clone();
    thread_match_args.extend(search_sql.args.clone());

    let offset = (page - 1) * PAGE_SIZE;
    let (thread_order, list_order) = if folder == EmailFolder::Trash {
        ("MAX(e2.deleted_at)", "e.deleted_at DESC, e.sent_at DESC")
    } else {
        ("MAX(e2.sent_at)", "e.sent_at DESC")
    };
    let thread_pick = format!(
        "SELECT e2.thread_id, {thread_order} AS mt FROM emails e2 WHERE {subquery_where}
         GROUP BY e2.thread_id ORDER BY mt DESC LIMIT {PAGE_SIZE} OFFSET {offset}"
    );
    let columns = format!(
        "e.id, e.thread_id, e.subject, e.body, e.sent_at, e.sender_email, e.receiver_email,
         e.sender_user_id, e.employee_read_at, e.is_draft <> 0 AS is_draft,
         {read} AS client_read_at,
         (SELECT COUNT(*) FROM email_attachments ea WHERE ea.email_id = e.id) > 0 AS has_attachments,
         EXISTS (SELECT 1 FROM email_attachments ea2 JOIN emails e3 ON ea2.email_id = e3.id
                 WHERE e3.thread_id = e.thread_id) AS thread_has_attachments,
         COALESCE(s.name, {sender_name}) AS sender_name,
         COALESCE(r.name, {receiver_name}) AS receiver_name",
        read = client_read_at(),
        sender_name = customer_name_by_email("e.sender_email"),
        receiver_name = customer_name_by_email("e.receiver_email"),
    );
    let list = if is_inbox {
        let mut args = thread_match_args.clone();
        args.extend(scope.args.clone());
        Sql::new(
            format!(
                "SELECT {columns}, picked.mt AS inbox_sort_at
                 FROM emails e
                 LEFT JOIN users s ON e.sender_user_id = s.id
                 LEFT JOIN users r ON e.receiver_email = r.email
                 INNER JOIN (SELECT thread_id, mt FROM ({thread_pick}) picked_page) picked
                   ON picked.thread_id = e.thread_id
                 WHERE {deleted} AND e.is_draft = 0 AND {scope}
                 ORDER BY picked.mt DESC, e.sent_at DESC, e.id DESC",
                scope = scope.sql
            ),
            args,
        )
    } else {
        let mut args = folder_args.clone();
        args.extend(thread_match_args.clone());
        Sql::new(
            format!(
                "SELECT {columns}, NULL AS inbox_sort_at
                 FROM emails e
                 LEFT JOIN users s ON e.sender_user_id = s.id
                 LEFT JOIN users r ON e.receiver_email = r.email
                 WHERE {folder_where}
                   AND e.thread_id IN (SELECT thread_id FROM ({thread_pick}) t)
                 ORDER BY {list_order}, e.id DESC"
            ),
            args,
        )
    };
    let mut emails: Vec<MailboxRow> = list.query_as().fetch_all(pool).await?;
    fill_display_names(pool, &mut emails).await?;

    let total: i64 = Sql::new(
        format!("SELECT COUNT(DISTINCT e.thread_id) FROM emails e WHERE {where_thread_match}"),
        thread_match_args,
    )
    .query_scalar()
    .fetch_one(pool)
    .await?;

    Ok(MailboxPage {
        emails,
        total_count: total,
        page,
        page_size: PAGE_SIZE,
    })
}

/// Inbox unread threads, trash threads and drafts of the caller.
pub async fn folder_counts(
    pool: &MySqlPool,
    mailbox: &Mailbox,
) -> Result<FolderCounts, sqlx::Error> {
    let scope = company_scope("e", mailbox.company_id);
    let unread = unread_incoming("e", mailbox);
    let mut args: Vec<Arg> = vec![mailbox.company_id.into()];
    args.extend(scope.args.clone());
    args.extend(unread.args);
    let inbox_unread: i64 = Sql::new(
        format!(
            "SELECT COUNT(DISTINCT e.thread_id) FROM emails e
             LEFT JOIN deals d ON d.id = COALESCE(
               e.deal_id,
               (SELECT MAX(td.deal_id) FROM emails td
                WHERE td.thread_id = e.thread_id AND td.deleted_at IS NULL AND td.is_draft = 0
                  AND td.deal_id IS NOT NULL AND (td.company_id = ? OR td.company_id IS NULL))
             ) AND d.deleted_at IS NULL
             WHERE e.deleted_at IS NULL AND e.is_draft = 0 AND e.thread_id IS NOT NULL
               AND e.employee_read_at IS NULL AND {scope} AND {unread}",
            scope = scope.sql,
            unread = unread.sql
        ),
        args,
    )
    .query_scalar()
    .fetch_one(pool)
    .await?;

    let receiver = receiver_condition("e", mailbox);
    let mut args = scope.args.clone();
    args.extend(receiver.args);
    let trash: i64 = Sql::new(
        format!(
            "SELECT COUNT(DISTINCT e.thread_id) FROM emails e
             WHERE e.deleted_at IS NOT NULL AND e.is_draft = 0 AND {} AND {}",
            scope.sql, receiver.sql
        ),
        args,
    )
    .query_scalar()
    .fetch_one(pool)
    .await?;

    let drafts: i64 = sqlx::query_scalar(
        "SELECT COUNT(DISTINCT e.thread_id) FROM emails e
         WHERE e.deleted_at IS NULL AND e.is_draft = 1 AND e.sender_user_id = ? AND e.company_id = ?",
    )
    .bind(mailbox.user_id)
    .bind(mailbox.company_id)
    .fetch_one(pool)
    .await?;

    Ok(FolderCounts {
        inbox_unread,
        trash,
        drafts,
    })
}

/// Suggestions per conversation, newest first.
const SUGGEST_LIMIT: usize = 8;

/// Typeahead for the mailbox search: the newest matching message of up to
/// 8 conversations, in the given folder (`searchEmailSuggestions`).
#[allow(clippy::too_many_lines)]
pub async fn suggestions(
    pool: &MySqlPool,
    mailbox: &Mailbox,
    query: &SuggestQuery,
) -> Result<Vec<SuggestionRow>, sqlx::Error> {
    let term = normalize_search_term(&query.term);
    if term.chars().count() < 2 {
        return Ok(Vec::new());
    }
    let scope_kind = query.scope.unwrap_or_default();
    let mut folder = query.folder.unwrap_or_default();
    if scope_kind == SuggestScope::Admin && folder == EmailFolder::Drafts {
        folder = EmailFolder::Inbox;
    }
    let extra = if scope_kind == SuggestScope::Employee {
        let addresses = customer_addresses(pool, mailbox.company_id, &term).await?;
        (!addresses.is_empty()).then(|| {
            let marks = vec!["?"; addresses.len()].join(",");
            let mut args: Vec<Arg> = addresses.iter().map(|a| Arg::from(a.clone())).collect();
            args.extend(addresses.iter().map(|a| Arg::from(a.clone())));
            Sql::new(
                format!(
                    " OR {} IN ({marks}) OR {} IN ({marks})",
                    normalized("e.sender_email"),
                    normalized("e.receiver_email")
                ),
                args,
            )
        })
    } else {
        None
    };
    let search = list_search(&term, extra);
    if search.sql.is_empty() {
        return Ok(Vec::new());
    }
    let fetch = SUGGEST_LIMIT * 4;
    let (where_sql, mut args, name_company) = if scope_kind == SuggestScope::Admin {
        let company = mailbox.company_id;
        let mut args: Vec<Arg> = vec![
            company.into(),
            company.into(),
            company.into(),
            company.into(),
        ];
        let rep = query.sales_rep.map_or(String::new(), |rep| {
            args.push(rep.into());
            args.push(rep.into());
            " AND (e.sender_user_id = ? OR r.id = ?)".to_string()
        });
        let deleted = if folder == EmailFolder::Trash {
            "IS NOT NULL"
        } else {
            "IS NULL"
        };
        let folder_sql = if folder == EmailFolder::Sent {
            query.sales_rep.map_or_else(
                || " AND e.sender_user_id IS NOT NULL".to_string(),
                |rep| {
                    args.push(rep.into());
                    " AND e.sender_user_id IS NOT NULL AND e.sender_user_id = ?".to_string()
                },
            )
        } else {
            " AND (e.sender_user_id IS NULL OR r.id IS NOT NULL)".to_string()
        };
        (
            format!(
                "e.deleted_at {deleted} AND e.is_draft = 0 AND (s.company_id = ? OR r.company_id = ?){rep}{folder_sql}"
            ),
            args,
            " AND c.company_id = ?",
        )
    } else {
        let scope = company_scope("e", mailbox.company_id);
        let (where_sql, args) = if folder == EmailFolder::Drafts {
            (
                format!(
                    "e.deleted_at IS NULL AND {} AND e.sender_user_id = ? AND e.is_draft = 1",
                    scope.sql
                ),
                vec![Arg::from(mailbox.company_id), Arg::from(mailbox.user_id)],
            )
        } else {
            let condition = if folder == EmailFolder::Sent {
                sender_condition("e", mailbox)
            } else {
                receiver_condition("e", mailbox)
            };
            let deleted = if folder == EmailFolder::Trash {
                "IS NOT NULL"
            } else {
                "IS NULL"
            };
            let mut args = scope.args;
            args.extend(condition.args);
            (
                format!(
                    "e.deleted_at {deleted} AND {} AND e.is_draft = 0 AND {}",
                    scope.sql, condition.sql
                ),
                args,
            )
        };
        (where_sql, args, "")
    };
    // Admin: the two company placeholders of the name lookups come first.
    let name = |column: &str| {
        format!(
            "(SELECT c.name FROM customers c JOIN customers_emails ce ON ce.customer_id = c.id \
             WHERE ce.email = {column}{name_company} LIMIT 1)"
        )
    };
    if scope_kind == SuggestScope::Employee {
        args.extend(search.args);
    } else {
        // [company, company] (names), then scope args after the first two.
        let names = args.drain(..2).collect::<Vec<_>>();
        let mut ordered = names;
        ordered.extend(args);
        ordered.extend(search.args);
        args = ordered;
    }
    let rows: Vec<SuggestionRow> = Sql::new(
        format!(
            "SELECT e.thread_id, e.subject, e.body, e.sent_at, e.sender_email, e.receiver_email,
                    COALESCE(s.name, {}) AS sender_name, COALESCE(r.name, {}) AS receiver_name
             FROM emails e
             LEFT JOIN users s ON e.sender_user_id = s.id
             LEFT JOIN users r ON e.receiver_email = r.email
             WHERE {where_sql}{} AND e.thread_id IS NOT NULL
             ORDER BY e.sent_at DESC, e.id DESC
             LIMIT {fetch}",
            name("e.sender_email"),
            name("e.receiver_email"),
            search.sql
        ),
        args,
    )
    .query_as()
    .fetch_all(pool)
    .await?;
    let mut seen = std::collections::HashSet::new();
    Ok(rows
        .into_iter()
        .filter(|row| seen.insert(row.thread_id.clone()))
        .take(SUGGEST_LIMIT)
        .collect())
}
