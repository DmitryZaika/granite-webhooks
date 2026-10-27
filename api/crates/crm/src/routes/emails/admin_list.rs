//! The admin mailbox: every employee's mail of the company, optionally
//! narrowed to one user (`sales_rep`). Port of the Remix `admin.emails`
//! loader.

use super::list::PAGE_SIZE;
use super::names::fill_display_names;
use super::schemas::{EmailFolder, FolderCounts, MailboxPage, MailboxRow};
use super::sql::{Arg, Sql, client_read_at, list_search};
use sqlx::MySqlPool;
use std::fmt::Write;

/// What an admin looks at: the company, one folder, maybe one user.
pub struct AdminMailbox {
    pub company_id: i32,
    pub folder: EmailFolder,
    pub sales_rep: Option<i32>,
}

/// The messages of the folder, `e`/`s`/`r` being the email, its sending
/// user and its receiving user. Drafts are never listed for admins (the
/// folder reads as the inbox).
fn folder_where(mailbox: &AdminMailbox, e: &str, s: &str, r: &str) -> Sql {
    let deleted = if mailbox.folder == EmailFolder::Trash {
        "IS NOT NULL"
    } else {
        "IS NULL"
    };
    let mut args: Vec<Arg> = vec![mailbox.company_id.into(), mailbox.company_id.into()];
    let mut sql = format!(
        "{e}.deleted_at {deleted} AND {e}.is_draft = 0 AND ({s}.company_id = ? OR {r}.company_id = ?)"
    );
    if let Some(rep) = mailbox.sales_rep {
        let _ = write!(sql, " AND ({e}.sender_user_id = ? OR {r}.id = ?)");
        args.extend([rep.into(), rep.into()]);
    }
    if mailbox.folder == EmailFolder::Sent {
        let _ = write!(sql, " AND {e}.sender_user_id IS NOT NULL");
        if let Some(rep) = mailbox.sales_rep {
            let _ = write!(sql, " AND {e}.sender_user_id = ?");
            args.push(rep.into());
        }
    } else {
        let _ = write!(
            sql,
            " AND ({e}.sender_user_id IS NULL OR {r}.id IS NOT NULL)"
        );
    }
    Sql::new(sql, args)
}

fn joined(sql: Sql, more: &Sql) -> Sql {
    let mut args = sql.args;
    args.extend(more.args.clone());
    Sql::new(format!("{}{}", sql.sql, more.sql), args)
}

/// Any message of the thread `{on}` matches the search.
fn thread_hit(search: &Sql, on: &str) -> Sql {
    if search.sql.is_empty() {
        return Sql::default();
    }
    Sql::new(
        format!(
            " AND EXISTS (SELECT 1 FROM emails hit WHERE hit.thread_id = {on} \
             AND hit.deleted_at IS NULL AND hit.is_draft = 0 {})",
            search.sql.replace("e.", "hit.")
        ),
        search.args.clone(),
    )
}

const USERS: &str = "LEFT JOIN users s ON e.sender_user_id = s.id
                     LEFT JOIN users r ON e.receiver_email = r.email";

/// One page of the folder, newest thread first.
pub async fn admin_page(
    pool: &MySqlPool,
    mailbox: &AdminMailbox,
    page: i64,
    search: &str,
) -> Result<MailboxPage, sqlx::Error> {
    let company = mailbox.company_id;
    let search = list_search(search.trim(), None);
    let inbox = mailbox.folder == EmailFolder::Inbox;
    let outer = folder_where(mailbox, "e", "s", "r");
    let inner = folder_where(mailbox, "e2", "s2", "r2");
    // The inbox matches threads with any matching message; the other
    // folders match the messages themselves.
    let (matching, picked) = if inbox {
        (
            joined(outer.clone(), &thread_hit(&search, "e.thread_id")),
            joined(inner, &thread_hit(&search, "e2.thread_id")),
        )
    } else {
        let inner_search = Sql::new(
            search
                .sql
                .replace("e.", "e2.")
                .replace("s.", "s2.")
                .replace("r.", "r2."),
            search.args.clone(),
        );
        (joined(outer.clone(), &search), joined(inner, &inner_search))
    };
    let offset = (page - 1) * PAGE_SIZE;
    let pick = format!(
        "SELECT e2.thread_id, MAX(e2.sent_at) AS mt FROM emails e2
           LEFT JOIN users s2 ON e2.sender_user_id = s2.id
           LEFT JOIN users r2 ON e2.receiver_email = r2.email
          WHERE {}
          GROUP BY e2.thread_id ORDER BY mt DESC LIMIT {PAGE_SIZE} OFFSET {offset}",
        picked.sql
    );
    let name = |column: &str| {
        format!(
            "(SELECT c.name FROM customers c JOIN customers_emails ce ON ce.customer_id = c.id \
             WHERE ce.email = {column} AND c.company_id = {company} ORDER BY ce.id LIMIT 1)"
        )
    };
    let columns = format!(
        "e.id, e.thread_id, e.subject, e.body, e.sent_at, e.sender_email, e.receiver_email,
         e.sender_user_id, e.employee_read_at, e.is_draft <> 0 AS is_draft,
         {read} AS client_read_at,
         (SELECT COUNT(*) FROM email_attachments ea WHERE ea.email_id = e.id) > 0 AS has_attachments,
         EXISTS (SELECT 1 FROM email_attachments ea2 JOIN emails e3 ON ea2.email_id = e3.id
                 WHERE e3.thread_id = e.thread_id) AS thread_has_attachments,
         COALESCE(s.name, {sender}) AS sender_name,
         COALESCE(r.name, {receiver}) AS receiver_name",
        read = client_read_at(),
        sender = name("e.sender_email"),
        receiver = name("e.receiver_email"),
    );
    let list = if inbox {
        let mut args = picked.args.clone();
        args.push(company.into());
        Sql::new(
            format!(
                "SELECT {columns}, picked.mt AS inbox_sort_at FROM emails e {USERS}
                 INNER JOIN (SELECT thread_id, mt FROM ({pick}) picked_page) picked
                   ON picked.thread_id = e.thread_id
                 WHERE e.deleted_at IS NULL AND e.is_draft = 0
                   AND (e.company_id = ? OR e.company_id IS NULL)
                 ORDER BY picked.mt DESC, e.sent_at DESC, e.id DESC"
            ),
            args,
        )
    } else {
        let message_match = joined(outer, &search);
        let mut args = message_match.args;
        args.extend(picked.args.clone());
        Sql::new(
            format!(
                "SELECT {columns}, NULL AS inbox_sort_at FROM emails e {USERS}
                 WHERE {} AND e.thread_id IN (SELECT thread_id FROM ({pick}) t)
                 ORDER BY e.sent_at DESC, e.id DESC",
                message_match.sql
            ),
            args,
        )
    };
    let mut emails: Vec<MailboxRow> = list.query_as().fetch_all(pool).await?;
    fill_display_names(pool, &mut emails).await?;
    let total: i64 = Sql::new(
        format!(
            "SELECT COUNT(DISTINCT e.thread_id) FROM emails e {USERS} WHERE {}",
            matching.sql
        ),
        matching.args,
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

/// Unread inbox threads and trash threads of the company (or one user).
pub async fn admin_counts(
    pool: &MySqlPool,
    company_id: i32,
    sales_rep: Option<i32>,
) -> Result<FolderCounts, sqlx::Error> {
    let count = |folder: EmailFolder, extra: &str| {
        let condition = folder_where(
            &AdminMailbox {
                company_id,
                folder,
                sales_rep,
            },
            "e",
            "s",
            "r",
        );
        Sql::new(
            format!(
                "SELECT COUNT(DISTINCT e.thread_id) FROM emails e {USERS} WHERE {}{extra}",
                condition.sql
            ),
            condition.args,
        )
    };
    let inbox_unread: i64 = count(
        EmailFolder::Inbox,
        " AND e.thread_id IS NOT NULL AND e.employee_read_at IS NULL",
    )
    .query_scalar()
    .fetch_one(pool)
    .await?;
    let trash: i64 = count(EmailFolder::Trash, "")
        .query_scalar()
        .fetch_one(pool)
        .await?;
    Ok(FolderCounts {
        inbox_unread,
        trash,
        drafts: 0,
    })
}
