//! One conversation: who it is with, its messages, attachments and participants.

use super::schemas::{Attachment, Participant, ThreadCustomer, ThreadDetail, ThreadMessage};
use super::sql::{Arg, CLIENT_OPEN_MIN_DELAY, Mailbox, Sql, can_read_thread, normalize_address};
use crate::storage::Storage;
use sqlx::MySqlPool;

/// The address mail of this user leaves from (`fromEmail`): their own on
/// the company's mail domain, else the shared sending address.
pub use common::amazon::email::from_email;

pub async fn sending_address(
    pool: &MySqlPool,
    company_id: i32,
    user_email: &str,
) -> Result<String, sqlx::Error> {
    let domain: Option<Option<String>> =
        sqlx::query_scalar("SELECT domain FROM company WHERE id = ?")
            .bind(company_id)
            .fetch_optional(pool)
            .await?;
    Ok(from_email(domain.flatten().as_deref(), user_email))
}

pub async fn readable(
    pool: &MySqlPool,
    thread_id: &str,
    company_id: i32,
) -> Result<bool, sqlx::Error> {
    let found: Option<i32> = can_read_thread(thread_id, company_id)
        .query_scalar()
        .fetch_optional(pool)
        .await?;
    Ok(found.is_some())
}

/// The other side of the conversation: the latest outside sender, or the
/// address the user wrote to.
async fn counterparty(
    pool: &MySqlPool,
    thread_id: &str,
    user_email: &str,
) -> Result<String, sqlx::Error> {
    let rows: Vec<(Option<String>, Option<String>, Option<i32>)> = sqlx::query_as(
        "SELECT sender_email, receiver_email, sender_user_id FROM emails
         WHERE thread_id = ? ORDER BY sent_at DESC, id DESC LIMIT 20",
    )
    .bind(thread_id)
    .fetch_all(pool)
    .await?;
    let me = normalize_address(Some(user_email));
    for (sender, receiver, sender_user) in rows {
        let sender = normalize_address(sender.as_deref());
        let receiver = normalize_address(receiver.as_deref());
        let from_employee = sender_user.is_some() || sender == me;
        if !from_employee && !sender.is_empty() && sender != me {
            return Ok(sender);
        }
        if from_employee && !receiver.is_empty() && receiver != me {
            return Ok(receiver);
        }
        if from_employee && !sender.is_empty() && sender != me && receiver == me {
            return Ok(sender);
        }
    }
    Ok(String::new())
}

/// The customer's address in this thread, else their primary address.
async fn customer_thread_email(
    pool: &MySqlPool,
    customer_id: i32,
    counterparty: &str,
) -> Result<String, sqlx::Error> {
    if !counterparty.is_empty() {
        let matching: Option<String> = sqlx::query_scalar(
            "SELECT email FROM customers_emails
             WHERE customer_id = ? AND LOWER(TRIM(SUBSTRING_INDEX(SUBSTRING_INDEX(email, '<', -1), '>', 1))) = ?
             LIMIT 1",
        )
        .bind(customer_id)
        .bind(counterparty)
        .fetch_optional(pool)
        .await?;
        let matching = normalize_address(matching.as_deref());
        if !matching.is_empty() {
            return Ok(matching);
        }
    }
    let primary: Option<Option<String>> = sqlx::query_scalar(
        "SELECT pe.email FROM customers c
         LEFT JOIN customers_emails pe ON pe.id = c.email_id AND pe.customer_id = c.id
         WHERE c.id = ? LIMIT 1",
    )
    .bind(customer_id)
    .fetch_optional(pool)
    .await?;
    let primary = normalize_address(primary.flatten().as_deref());
    Ok(if primary.is_empty() {
        counterparty.to_string()
    } else {
        primary
    })
}

/// Who a thread is with (`resolveEmailThreadCustomer`): the deal's customer,
/// a customer by address, a name from a header, a coworker, the address.
#[allow(clippy::too_many_lines)] // The resolution order of the Remix helper, step by step.
pub async fn thread_customer(
    pool: &MySqlPool,
    thread_id: &str,
    mailbox: &Mailbox,
) -> Result<(Option<i64>, ThreadCustomer), sqlx::Error> {
    let deal_id: Option<i64> = sqlx::query_scalar(
        "SELECT CAST(deal_id AS SIGNED) FROM emails WHERE thread_id = ? AND deal_id IS NOT NULL
         ORDER BY sent_at DESC, id DESC LIMIT 1",
    )
    .bind(thread_id)
    .fetch_optional(pool)
    .await?;
    let other = counterparty(pool, thread_id, &mailbox.email).await?;

    if let Some(deal_id) = deal_id {
        let customer: Option<(i32, Option<String>)> = sqlx::query_as(
            "SELECT c.id, c.name FROM deals d JOIN customers c ON c.id = d.customer_id
             WHERE d.id = ? AND d.deleted_at IS NULL LIMIT 1",
        )
        .bind(deal_id)
        .fetch_optional(pool)
        .await?;
        let name = customer
            .as_ref()
            .and_then(|(_, name)| name.clone())
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| {
                if other.is_empty() {
                    "Customer".into()
                } else {
                    other.clone()
                }
            });
        let id = customer.map(|(id, _)| id);
        let email = match id {
            Some(id) => customer_thread_email(pool, id, &other).await?,
            None => other,
        };
        return Ok((Some(deal_id), ThreadCustomer { id, name, email }));
    }

    if !other.is_empty() {
        let matched: Option<(i32, Option<String>, String)> = sqlx::query_as(
            "SELECT c.id, c.name, ce.email FROM customers c JOIN customers_emails ce ON ce.customer_id = c.id
             WHERE c.company_id = ? AND c.deleted_at IS NULL
               AND LOWER(TRIM(SUBSTRING_INDEX(SUBSTRING_INDEX(ce.email, '<', -1), '>', 1))) = ?
             LIMIT 1",
        )
        .bind(mailbox.company_id)
        .bind(&other)
        .fetch_optional(pool)
        .await?;
        if let Some((id, name, email)) = matched {
            let email = normalize_address(Some(&email));
            return Ok((
                None,
                ThreadCustomer {
                    id: Some(id),
                    name: name
                        .filter(|n| !n.trim().is_empty())
                        .unwrap_or_else(|| other.clone()),
                    email: if email.is_empty() { other } else { email },
                },
            ));
        }
    }

    let header_name = if other.is_empty() {
        None
    } else {
        let names: Vec<Option<String>> = sqlx::query_scalar(
            "SELECT ep.display_name FROM email_participants ep JOIN emails e ON e.id = ep.email_id
             WHERE e.company_id = ? AND LOWER(TRIM(ep.email)) = ?
               AND ep.display_name IS NOT NULL AND TRIM(ep.display_name) <> ''
             ORDER BY (e.thread_id = ?) DESC, (ep.type = 'from') DESC, e.sent_at DESC, ep.email_id DESC
             LIMIT 8",
        )
        .bind(mailbox.company_id)
        .bind(&other)
        .bind(thread_id)
        .fetch_all(pool)
        .await?;
        names
            .into_iter()
            .flatten()
            .map(|n| n.trim().to_string())
            .find(|n| !n.is_empty() && n.to_lowercase() != other)
    };
    let coworker: Option<(Option<String>, String)> = if header_name.is_some() || other.is_empty() {
        None
    } else {
        sqlx::query_as(
            "SELECT name, email FROM users WHERE company_id = ? AND is_deleted = 0
             AND LOWER(TRIM(email)) = ? LIMIT 1",
        )
        .bind(mailbox.company_id)
        .bind(&other)
        .fetch_optional(pool)
        .await?
    };
    let coworker = coworker.map(|(name, email)| {
        let email = normalize_address(Some(&email));
        let name = name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty());
        (name.unwrap_or_else(|| email.clone()), email)
    });
    let name = header_name
        .or_else(|| coworker.as_ref().map(|(name, _)| name.clone()))
        .unwrap_or_else(|| {
            if other.is_empty() {
                "Customer".into()
            } else {
                other.clone()
            }
        });
    let email = coworker.map_or(other, |(_, email)| email);
    Ok((
        None,
        ThreadCustomer {
            id: None,
            name,
            email,
        },
    ))
}

fn mime(attachment: &Attachment) -> Option<String> {
    let kind = attachment.content_type.trim().to_lowercase();
    let sub = attachment
        .content_subtype
        .as_deref()
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    if !kind.is_empty() && !sub.is_empty() {
        Some(format!("{kind}/{sub}"))
    } else if kind.contains('/') {
        Some(kind)
    } else {
        None
    }
}

async fn user_signature(pool: &MySqlPool, user_id: i32) -> Result<Option<String>, sqlx::Error> {
    let signature: Option<Option<String>> =
        sqlx::query_scalar("SELECT email_signature FROM users WHERE id = ?")
            .bind(user_id)
            .fetch_optional(pool)
            .await?;
    Ok(signature.flatten().filter(|s| !s.is_empty()))
}

/// Every address of a customer, primary first (`loadCustomerEmails`).
async fn customer_addresses(
    pool: &MySqlPool,
    customer_id: i32,
) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT ce.email FROM customers_emails ce JOIN customers c ON c.id = ce.customer_id
         WHERE ce.customer_id = ? ORDER BY ce.id = c.email_id DESC, ce.created_at, ce.id",
    )
    .bind(customer_id)
    .fetch_all(pool)
    .await
}

async fn primary_address(pool: &MySqlPool, customer_id: i32) -> Result<String, sqlx::Error> {
    let primary: Option<Option<String>> = sqlx::query_scalar(
        "SELECT pe.email FROM customers c
         LEFT JOIN customers_emails pe ON pe.id = c.email_id AND pe.customer_id = c.id
         WHERE c.id = ?",
    )
    .bind(customer_id)
    .fetch_optional(pool)
    .await?;
    Ok(primary.flatten().unwrap_or_default())
}

/// Signed links and e-signature status of attachments.
pub async fn finish_attachments(
    pool: &MySqlPool,
    storage: &Storage,
    attachments: &mut [Attachment],
) -> Result<(), sqlx::Error> {
    for attachment in attachments.iter_mut() {
        let signed = storage
            .readable_url(&attachment.url, mime(attachment).as_deref())
            .await;
        if signed != attachment.url {
            attachment.signed_url = Some(signed);
        }
    }
    if attachments.is_empty() {
        return Ok(());
    }
    let ids: Vec<Arg> = attachments.iter().map(|a| Arg::from(a.id)).collect();
    let marks = vec!["?"; ids.len()].join(",");
    let statuses: Vec<(i32, String)> = Sql::new(
        format!(
            "SELECT email_attachment_id, CAST(status AS CHAR) FROM signature_requests
             WHERE email_attachment_id IN ({marks}) ORDER BY id"
        ),
        ids,
    )
    .query_as()
    .fetch_all(pool)
    .await?;
    for (attachment_id, status) in statuses {
        if let Some(attachment) = attachments.iter_mut().find(|a| a.id == attachment_id) {
            attachment.signature_status = Some(status);
        }
    }
    Ok(())
}

/// A thread's messages (oldest first, with attachments; on a deal only its
/// own and unassigned ones) and every non-BCC address, in header order.
pub async fn thread_contents(
    pool: &MySqlPool,
    storage: &Storage,
    thread_id: &str,
    trash: bool,
    deal_id: Option<i64>,
) -> Result<(Vec<ThreadMessage>, Vec<Participant>), sqlx::Error> {
    let deleted = if trash { "IS NOT NULL" } else { "IS NULL" };
    let mut args: Vec<Arg> = vec![thread_id.into()];
    let deal_filter = deal_id.map_or(String::new(), |deal_id| {
        args.push(deal_id.into());
        " AND (e.deal_id = ? OR e.deal_id IS NULL)".to_string()
    });
    let mut messages: Vec<ThreadMessage> = Sql::new(
        format!(
            "SELECT e.id, e.subject, e.body, e.html_body, e.sent_at, e.sender_email, e.receiver_email,
                    e.receiver_user_id, e.employee_read_at, e.sender_user_id, u.email_signature AS signature,
                    MAX(er.read_at) AS read_at
             FROM emails e
             LEFT JOIN email_reads er ON e.message_id = er.message_id AND {CLIENT_OPEN_MIN_DELAY}
             LEFT JOIN users u ON u.id = e.sender_user_id
             WHERE e.deleted_at {deleted} AND e.thread_id = ?{deal_filter}
             GROUP BY e.id
             ORDER BY e.sent_at, e.id"
        ),
        args.clone(),
    )
    .query_as()
    .fetch_all(pool)
    .await?;

    let deal_filter = deal_id.map_or(String::new(), |_| {
        " AND (deal_id = ? OR deal_id IS NULL)".to_string()
    });
    let mut attachments: Vec<Attachment> = Sql::new(
        format!(
            "SELECT id, email_id, content_type, content_subtype, filename, url, is_inline FROM email_attachments
             WHERE email_id IN (SELECT id FROM emails WHERE deleted_at {deleted} AND thread_id = ?{deal_filter})
             ORDER BY id"
        ),
        args,
    )
    .query_as()
    .fetch_all(pool)
    .await?;
    finish_attachments(pool, storage, &mut attachments).await?;
    for message in &mut messages {
        message.attachments = attachments
            .iter()
            .filter(|a| a.email_id == message.id)
            .cloned()
            .collect();
    }

    let participants: Vec<Participant> = sqlx::query_as(
        "SELECT ep.email_id, CAST(ep.type AS CHAR) AS type, ep.email, ep.display_name, ep.user_id,
                ep.customer_id, u.name AS user_name, c.name AS customer_name
         FROM email_participants ep
         JOIN emails e ON e.id = ep.email_id
         LEFT JOIN users u ON u.id = ep.user_id
         LEFT JOIN customers c ON c.id = ep.customer_id
         WHERE e.thread_id = ? AND ep.type <> 'bcc'
         ORDER BY ep.email_id, ep.position, ep.id",
    )
    .bind(thread_id)
    .fetch_all(pool)
    .await?;

    Ok((messages, participants))
}

/// Everything the conversation view shows. `None` when the caller's company
/// may not read it.
#[allow(clippy::too_many_lines)] // Mirrors the two Remix chat loaders (mailbox and deal).
pub async fn thread_detail(
    pool: &MySqlPool,
    storage: &Storage,
    mailbox: &Mailbox,
    thread_id: &str,
    trash: bool,
    on_deal: Option<i64>,
) -> Result<Option<ThreadDetail>, sqlx::Error> {
    let deal_customer = match on_deal {
        Some(deal_id) => {
            let row: Option<(i32, Option<String>)> = sqlx::query_as(
                "SELECT c.id, c.name FROM deals d JOIN customers c ON c.id = d.customer_id
                 WHERE d.id = ? AND d.deleted_at IS NULL AND c.company_id = ?",
            )
            .bind(deal_id)
            .bind(mailbox.company_id)
            .fetch_optional(pool)
            .await?;
            let Some(row) = row else {
                return Ok(None);
            };
            Some(row)
        }
        None => None,
    };
    if !readable(pool, thread_id, mailbox.company_id).await? {
        let (Some(deal_id), Some((customer_id, name))) = (on_deal, deal_customer) else {
            return Ok(None);
        };
        let exists: Option<i32> =
            sqlx::query_scalar("SELECT 1 FROM emails WHERE thread_id = ? LIMIT 1")
                .bind(thread_id)
                .fetch_optional(pool)
                .await?;
        if exists.is_some() {
            return Ok(None);
        }
        // A new conversation started from the deal.
        let addresses = customer_addresses(pool, customer_id).await?;
        let signature = user_signature(pool, mailbox.user_id).await?;
        return Ok(Some(ThreadDetail {
            thread_id: thread_id.to_string(),
            deal_id: Some(deal_id),
            customer: ThreadCustomer {
                id: Some(customer_id),
                name: name
                    .filter(|n| !n.trim().is_empty())
                    .unwrap_or_else(|| "Customer".into()),
                email: primary_address(pool, customer_id).await?,
            },
            subject: None,
            current_user_signature: signature,
            sending_email: sending_address(pool, mailbox.company_id, &mailbox.email).await?,
            messages: Vec::new(),
            participants: Vec::new(),
            customer_addresses: addresses,
        }));
    }
    let signature = user_signature(pool, mailbox.user_id).await?;
    let sending_email = sending_address(pool, mailbox.company_id, &mailbox.email).await?;
    let (deal_id, customer) = match (on_deal, deal_customer) {
        (Some(deal_id), Some((customer_id, name))) => {
            let other = counterparty(pool, thread_id, &mailbox.email).await?;
            (
                Some(deal_id),
                ThreadCustomer {
                    id: Some(customer_id),
                    name: name
                        .filter(|n| !n.trim().is_empty())
                        .unwrap_or_else(|| "Customer".into()),
                    email: customer_thread_email(pool, customer_id, &other).await?,
                },
            )
        }
        _ => thread_customer(pool, thread_id, mailbox).await?,
    };
    let customer_addresses = match customer.id {
        Some(id) => customer_addresses(pool, id).await?,
        None => Vec::new(),
    };

    let (messages, participants) =
        thread_contents(pool, storage, thread_id, trash, deal_id).await?;

    Ok(Some(ThreadDetail {
        thread_id: thread_id.to_string(),
        deal_id,
        subject: messages.first().and_then(|m| m.subject.clone()),
        customer,
        current_user_signature: signature,
        sending_email,
        messages,
        participants,
        customer_addresses,
    }))
}

#[cfg(test)]
mod tests {
    use super::from_email;

    #[test]
    fn sending_address_needs_the_company_domain() {
        assert_eq!(
            from_email(Some("acme.test"), "rep@acme.test"),
            "rep@acme.test"
        );
        assert_eq!(
            from_email(Some("acme.test"), "rep@gmail.com"),
            "sales@granite-manager.com"
        );
        assert_eq!(
            from_email(None, "rep@acme.test"),
            "sales@granite-manager.com"
        );
        assert_eq!(
            from_email(Some("acme.test"), "acme.test@victim.test"),
            "sales@granite-manager.com"
        );
    }
}
