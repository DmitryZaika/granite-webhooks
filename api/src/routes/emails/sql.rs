//! SQL fragments shared by the email routes. Ports of the Remix
//! `emailVisibility.server.ts`, `incomingMailboxSql.ts`,
//! `emailListSearch.server.ts` and `emailClientOpen.ts`; keep them in step.

use sqlx::mysql::MySqlArguments;
use sqlx::query::{Query, QueryAs, QueryScalar};
use sqlx::{FromRow, MySql};

/// A bound value of a hand-built statement.
#[derive(Debug, Clone)]
pub enum Arg {
    Int(i64),
    Text(String),
}

impl From<i32> for Arg {
    fn from(value: i32) -> Self {
        Self::Int(value.into())
    }
}

impl From<i64> for Arg {
    fn from(value: i64) -> Self {
        Self::Int(value)
    }
}

impl From<&str> for Arg {
    fn from(value: &str) -> Self {
        Self::Text(value.to_string())
    }
}

impl From<String> for Arg {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

/// SQL text with its `?` arguments, in order.
#[derive(Debug, Clone, Default)]
pub struct Sql {
    pub sql: String,
    pub args: Vec<Arg>,
}

impl Sql {
    pub fn new(sql: impl Into<String>, args: Vec<Arg>) -> Self {
        Self {
            sql: sql.into(),
            args,
        }
    }

    pub fn query_as<T>(&self) -> QueryAs<'static, MySql, T, MySqlArguments>
    where
        T: for<'r> FromRow<'r, sqlx::mysql::MySqlRow>,
    {
        let mut query = sqlx::query_as::<_, T>(sqlx::AssertSqlSafe(self.sql.clone()));
        for arg in &self.args {
            query = match arg {
                Arg::Int(value) => query.bind(*value),
                Arg::Text(value) => query.bind(value.clone()),
            };
        }
        query
    }

    pub fn query_scalar<T>(&self) -> QueryScalar<'static, MySql, T, MySqlArguments>
    where
        (T,): for<'r> FromRow<'r, sqlx::mysql::MySqlRow>,
    {
        let mut query = sqlx::query_scalar::<_, T>(sqlx::AssertSqlSafe(self.sql.clone()));
        for arg in &self.args {
            query = match arg {
                Arg::Int(value) => query.bind(*value),
                Arg::Text(value) => query.bind(value.clone()),
            };
        }
        query
    }

    pub fn query(&self) -> Query<'static, MySql, MySqlArguments> {
        let mut query = sqlx::query(sqlx::AssertSqlSafe(self.sql.clone()));
        for arg in &self.args {
            query = match arg {
                Arg::Int(value) => query.bind(*value),
                Arg::Text(value) => query.bind(value.clone()),
            };
        }
        query
    }
}

/// The address inside `Name <address>`, trimmed and lowercased (SQL).
pub fn normalized(column: &str) -> String {
    format!("LOWER(TRIM(SUBSTRING_INDEX(SUBSTRING_INDEX({column}, '<', -1), '>', 1)))")
}

/// `normalizeAddress`: the address inside `Name <address>`, lowercased.
pub fn normalize_address(raw: Option<&str>) -> String {
    let Some(raw) = raw else {
        return String::new();
    };
    let trimmed = raw.trim();
    let inner = match (trimmed.rfind('<'), trimmed.rfind('>')) {
        (Some(open), Some(close)) if close > open => &trimmed[open + 1..close],
        _ => trimmed,
    };
    inner.trim().to_lowercase()
}

/// The display name of `"Name" <address>`, if any (`displayNameFromAddressField`).
pub fn display_name_from_address(raw: Option<&str>) -> Option<String> {
    let trimmed = raw?.trim();
    if !trimmed.ends_with('>') {
        return None;
    }
    let open = trimmed.find('<')?;
    let name = trimmed[..open]
        .trim()
        .trim_start_matches('"')
        .trim_end_matches('"');
    (!name.is_empty()).then(|| name.to_string())
}

/// The caller as the email queries see them.
#[derive(Debug, Clone)]
pub struct Mailbox {
    pub user_id: i32,
    pub company_id: i32,
    /// Login email, trimmed and lowercased.
    pub email: String,
}

impl Mailbox {
    /// `%<address>`: matches `Name <address>` receiver/sender columns.
    pub fn like(&self) -> String {
        format!("%<{}>", self.email)
    }
}

/// Rows of the company, or legacy rows of no company (still gated by address).
pub fn company_scope(alias: &str, company_id: i32) -> Sql {
    Sql::new(
        format!("({alias}.company_id = ? OR {alias}.company_id IS NULL)"),
        vec![company_id.into()],
    )
}

/// The inbox addressee test (list, trash): the receiver columns or a to/cc
/// participant row of this user. `receiver_user_id` covers BCC'd copies, whose
/// headers never name the user.
pub fn receiver_condition(alias: &str, mailbox: &Mailbox) -> Sql {
    let receiver = normalized(&format!("{alias}.receiver_email"));
    Sql::new(
        format!(
            "(
      {alias}.receiver_user_id = ?
      OR {receiver} = ?
      OR ({alias}.receiver_email NOT LIKE '%<%' AND LOWER(TRIM({alias}.receiver_email)) = ?)
      OR {alias}.receiver_email LIKE ?
      OR {alias}.id IN (
        SELECT /*+ SUBQUERY(MATERIALIZATION) */ ep.email_id FROM email_participants ep
        WHERE ep.type IN ('to', 'cc') AND (ep.user_id = ? OR ep.email = ?)
      )
    )"
        ),
        vec![
            mailbox.user_id.into(),
            mailbox.email.clone().into(),
            mailbox.email.clone().into(),
            mailbox.like().into(),
            mailbox.user_id.into(),
            mailbox.email.clone().into(),
        ],
    )
}

/// The thread is in this user's Inbox list: one of its messages is addressed
/// to them. Unread counts need it, or mail on their deal sent to another
/// address counts without ever showing in their Inbox.
pub fn thread_in_inbox(alias: &str, mailbox: &Mailbox) -> Sql {
    let scope = company_scope("ib", mailbox.company_id);
    let receiver = receiver_condition("ib", mailbox);
    let mut args = scope.args;
    args.extend(receiver.args);
    Sql::new(
        format!(
            "EXISTS (SELECT 1 FROM emails ib WHERE ib.thread_id = {alias}.thread_id
      AND ib.deleted_at IS NULL AND ib.is_draft = 0 AND {} AND {})",
            scope.sql, receiver.sql
        ),
        args,
    )
}

/// Sent by this user (their id, or their address in the sender column).
pub fn sender_condition(alias: &str, mailbox: &Mailbox) -> Sql {
    let sender = normalized(&format!("{alias}.sender_email"));
    Sql::new(
        format!(
            "(
      {alias}.sender_user_id = ?
      OR {sender} = ?
      OR ({alias}.sender_email NOT LIKE '%<%' AND LOWER(TRIM({alias}.sender_email)) = ?)
      OR {alias}.sender_email LIKE ?
    )"
        ),
        vec![
            mailbox.user_id.into(),
            mailbox.email.clone().into(),
            mailbox.email.clone().into(),
            mailbox.like().into(),
        ],
    )
}

fn addressed_to_user(alias: &str, mailbox: &Mailbox) -> Sql {
    let receiver = normalized(&format!("{alias}.receiver_email"));
    Sql::new(
        format!(
            "(
    {alias}.receiver_user_id = ?
    OR {receiver} = ?
    OR ({alias}.receiver_email NOT LIKE '%<%' AND LOWER(TRIM({alias}.receiver_email)) = ?)
    OR {alias}.receiver_email LIKE ?
    OR {alias}.id IN (
      SELECT /*+ SUBQUERY(MATERIALIZATION) */ ep.email_id FROM email_participants ep
      WHERE ep.type IN ('to', 'cc') AND (ep.user_id = ? OR ep.email = ?)
    )
  )"
        ),
        vec![
            mailbox.user_id.into(),
            mailbox.email.clone().into(),
            mailbox.email.clone().into(),
            mailbox.like().into(),
            mailbox.user_id.into(),
            mailbox.email.clone().into(),
        ],
    )
}

/// Unread inbox mail (`unreadIncomingWhere`): a customer wrote (to this user
/// or on their deal), or a coworker addressed this user. Requires
/// `LEFT JOIN deals d`.
pub fn unread_incoming(alias: &str, mailbox: &Mailbox) -> Sql {
    let receiver = normalized(&format!("{alias}.receiver_email"));
    let addressed = addressed_to_user(alias, mailbox);
    let mut args: Vec<Arg> = vec![
        mailbox.user_id.into(),
        mailbox.user_id.into(),
        mailbox.email.clone().into(),
        mailbox.email.clone().into(),
        mailbox.like().into(),
        mailbox.user_id.into(),
        mailbox.email.clone().into(),
        mailbox.user_id.into(),
    ];
    args.extend(addressed.args);
    Sql::new(
        format!(
            "(
      (
        {alias}.sender_user_id IS NULL
        AND (
          {alias}.receiver_user_id = ?
          OR d.user_id = ?
          OR {receiver} = ?
          OR ({alias}.receiver_email NOT LIKE '%<%' AND LOWER(TRIM({alias}.receiver_email)) = ?)
          OR {alias}.receiver_email LIKE ?
          OR {alias}.id IN (
            SELECT /*+ SUBQUERY(MATERIALIZATION) */ ep.email_id FROM email_participants ep
            WHERE ep.type IN ('to', 'cc') AND (ep.user_id = ? OR ep.email = ?)
          )
        )
      )
      OR (
        {alias}.sender_user_id IS NOT NULL
        AND {alias}.sender_user_id <> ?
        AND {addressed}
      )
    )",
            addressed = addressed.sql
        ),
        args,
    )
}

/// Customer and coworker mail, never the viewer's own sends (marking read).
pub fn received_message(user_id: i32) -> Sql {
    Sql::new(
        "(sender_user_id IS NULL OR sender_user_id <> ?)",
        vec![user_id.into()],
    )
}

/// A message is connected to this user: sender, addressee, or a to/cc participant.
fn user_on_email(alias: &str, mailbox: &Mailbox) -> Sql {
    let sender = normalized(&format!("{alias}.sender_email"));
    let receiver = normalized(&format!("{alias}.receiver_email"));
    Sql::new(
        format!(
            "(
      {alias}.sender_user_id = ?
      OR {alias}.receiver_user_id = ?
      OR {sender} = ?
      OR {receiver} = ?
      OR EXISTS (
        SELECT 1 FROM email_participants ep
        WHERE ep.email_id = {alias}.id AND ep.type <> 'bcc' AND (ep.user_id = ? OR ep.email = ?)
      )
    )"
        ),
        vec![
            mailbox.user_id.into(),
            mailbox.user_id.into(),
            mailbox.email.clone().into(),
            mailbox.email.clone().into(),
            mailbox.user_id.into(),
            mailbox.email.clone().into(),
        ],
    )
}

/// `<column> IN (threads this user may change)`: threads of the company they
/// are on, or on one of their deals (`ownThreadsClause`). The derived table
/// lets an UPDATE of `emails` read `emails`.
pub fn own_threads(column: &str, mailbox: &Mailbox) -> Sql {
    let scope = company_scope("ev", mailbox.company_id);
    let membership = user_on_email("ev", mailbox);
    let mut args = scope.args;
    args.extend(membership.args);
    args.push(mailbox.user_id.into());
    Sql::new(
        format!(
            "{column} IN (SELECT owned.thread_id FROM (
          SELECT DISTINCT ev.thread_id
          FROM emails ev
          LEFT JOIN (
            SELECT thread_id, MAX(deal_id) AS deal_id FROM emails
            WHERE deleted_at IS NULL AND thread_id IS NOT NULL AND deal_id IS NOT NULL
            GROUP BY thread_id
          ) td ON td.thread_id = ev.thread_id
          LEFT JOIN deals d ON d.id = COALESCE(ev.deal_id, td.deal_id) AND d.deleted_at IS NULL
          WHERE ev.thread_id IS NOT NULL AND {scope} AND ({membership} OR d.user_id = ?)
        ) AS owned)",
            scope = scope.sql,
            membership = membership.sql,
        ),
        args,
    )
}

/// May this user read the thread (`canReadThread`): it belongs to their
/// company, or (legacy rows without a company) an address on it is one of
/// their company's users or customers.
pub fn can_read_thread(thread_id: &str, company_id: i32) -> Sql {
    let sender = normalized("e.sender_email");
    let receiver = normalized("e.receiver_email");
    let customer = normalized("ce.email");
    Sql::new(
        format!(
            "SELECT 1 FROM emails e
     WHERE e.thread_id = ?
       AND (
         e.company_id = ?
         OR (
           e.company_id IS NULL
           AND (
             EXISTS (SELECT 1 FROM users u WHERE u.company_id = ? AND u.is_deleted = 0
                       AND LOWER(TRIM(u.email)) IN ({sender}, {receiver}))
             OR EXISTS (SELECT 1 FROM customers c JOIN customers_emails ce ON ce.customer_id = c.id
                          WHERE c.company_id = ? AND c.deleted_at IS NULL
                            AND {customer} IN ({sender}, {receiver}))
             OR EXISTS (SELECT 1 FROM email_participants ep JOIN users u2 ON u2.id = ep.user_id
                          WHERE ep.email_id = e.id AND u2.company_id = ?)
           )
         )
       )
     LIMIT 1"
        ),
        vec![
            thread_id.into(),
            company_id.into(),
            company_id.into(),
            company_id.into(),
            company_id.into(),
        ],
    )
}

/// Opens that are not image proxies or scanners firing right after the send.
pub const CLIENT_OPEN_MIN_DELAY: &str = "er.read_at >= e.sent_at + INTERVAL 5 SECOND";

/// Latest real client open of message `e`.
pub fn client_read_at() -> String {
    format!(
        "(SELECT MAX(er.read_at) FROM email_reads er WHERE er.message_id = e.message_id AND {CLIENT_OPEN_MIN_DELAY})"
    )
}

/// The customer name of an address column of `e`.
pub fn customer_name_by_email(column: &str) -> String {
    format!(
        "(SELECT c.name FROM customers c JOIN customers_emails ce ON ce.customer_id = c.id \
         WHERE ce.email = {column} ORDER BY ce.id LIMIT 1)"
    )
}

/// `normalizeEmailSearchTerm`: drop punctuation stuck to words.
pub fn normalize_search_term(raw: &str) -> String {
    const EDGE: &[char] = &[
        '.', ',', ';', ':', '!', '?', '…', '\'', '"', '“', '”', '‘', '’', '(', ')', '[', ']', '{',
        '}', '<', '>',
    ];
    raw.split_whitespace()
        .map(|token| token.trim_matches(EDGE))
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn whitespace_normalized(column: &str) -> String {
    format!(
        "REPLACE(REPLACE(REPLACE(IFNULL({column}, ''), '\\r\\n', ' '), '\\n', ' '), '\\r', ' ')"
    )
}

/// ` AND (...)` matching subject, text, html and addresses (`buildEmailListSearch`).
/// `extra` is an ` OR ...` fragment. Empty when the term is.
pub fn list_search(search: &str, extra: Option<Sql>) -> Sql {
    let term = normalize_search_term(search);
    if term.is_empty() {
        return Sql::default();
    }
    let escaped = term
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    let like = format!("%{escaped}%");
    let esc = " ESCAPE '\\\\'";
    let conditions = [
        format!("e.subject LIKE ?{esc}"),
        format!("{} LIKE ?{esc}", whitespace_normalized("e.body")),
        format!("{} LIKE ?{esc}", whitespace_normalized("e.html_body")),
        format!("e.sender_email LIKE ?{esc}"),
        format!("e.receiver_email LIKE ?{esc}"),
    ];
    let mut args: Vec<Arg> = (0..5).map(|_| Arg::from(like.clone())).collect();
    let extra = extra.unwrap_or_default();
    args.extend(extra.args);
    Sql::new(
        format!(" AND ({}{})", conditions.join(" OR "), extra.sql),
        args,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses() {
        assert_eq!(normalize_address(Some(" Ann <ANN@x.com> ")), "ann@x.com");
        assert_eq!(normalize_address(Some("ann@x.com")), "ann@x.com");
        assert_eq!(normalize_address(None), "");
        assert_eq!(
            display_name_from_address(Some("\"Ann Lee\" <ann@x.com>")).as_deref(),
            Some("Ann Lee")
        );
        assert_eq!(display_name_from_address(Some("ann@x.com")), None);
        assert_eq!(display_name_from_address(Some("<ann@x.com>")), None);
    }

    #[test]
    fn search_terms() {
        assert_eq!(
            normalize_search_term(" James.  andrew.....  "),
            "James andrew"
        );
        assert_eq!(normalize_search_term("..."), "");
        let search = list_search("5%_off", None);
        assert!(matches!(&search.args[0], Arg::Text(t) if t == "%5\\%\\_off%"));
    }
}
