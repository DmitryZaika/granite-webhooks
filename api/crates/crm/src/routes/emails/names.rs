//! Names people gave their addresses in mail headers ("Ann Lee" <ann@x.com>),
//! used when a row has no user or customer name. Port of
//! `fillEmailListGmailNames` / `applyGmailDisplayNamesToEmailRows`.

use super::schemas::MailboxRow;
use super::sql::{Arg, Sql, display_name_from_address, normalize_address};
use sqlx::MySqlPool;
use std::collections::HashMap;

/// A name that is not just the address itself.
fn has_real_name(name: Option<&str>, email: Option<&str>) -> bool {
    let name = name.unwrap_or_default().trim();
    !name.is_empty() && name.to_lowercase() != normalize_address(email)
}

fn apply(row: &mut MailboxRow, names: &HashMap<String, String>) {
    if !has_real_name(row.sender_name.as_deref(), row.sender_email.as_deref()) {
        let found = display_name_from_address(row.sender_email.as_deref()).or_else(|| {
            names
                .get(&normalize_address(row.sender_email.as_deref()))
                .cloned()
        });
        if found.is_some() {
            row.sender_name = found;
        }
    }
    if !has_real_name(row.receiver_name.as_deref(), row.receiver_email.as_deref()) {
        let found = display_name_from_address(row.receiver_email.as_deref()).or_else(|| {
            names
                .get(&normalize_address(row.receiver_email.as_deref()))
                .cloned()
        });
        if found.is_some() {
            row.receiver_name = found;
        }
    }
}

/// Fills missing sender/receiver names: from the header itself, then the
/// message's own participant names, then any name seen for the address.
pub async fn fill_display_names(
    pool: &MySqlPool,
    rows: &mut [MailboxRow],
) -> Result<(), sqlx::Error> {
    let empty = HashMap::new();
    for row in rows.iter_mut() {
        apply(row, &empty);
    }
    if rows.is_empty() {
        return Ok(());
    }

    let ids: Vec<Arg> = rows.iter().map(|row| Arg::from(row.id)).collect();
    let marks = vec!["?"; ids.len()].join(",");
    let per_email: Vec<(i32, String, String)> = Sql::new(
        format!(
            "SELECT email_id, email, display_name FROM email_participants
             WHERE display_name IS NOT NULL AND TRIM(display_name) <> '' AND email_id IN ({marks})
             ORDER BY id"
        ),
        ids,
    )
    .query_as()
    .fetch_all(pool)
    .await?;
    let mut by_email: HashMap<i32, HashMap<String, String>> = HashMap::new();
    for (email_id, email, name) in per_email {
        let email = normalize_address(Some(&email));
        let name = name.trim().to_string();
        if email.is_empty() || name.is_empty() {
            continue;
        }
        by_email
            .entry(email_id)
            .or_default()
            .entry(email)
            .or_insert(name);
    }
    for row in rows.iter_mut() {
        if let Some(local) = by_email.get(&row.id) {
            apply(row, local);
        }
    }

    let mut addresses: Vec<String> = rows
        .iter()
        .flat_map(|row| [row.sender_email.as_deref(), row.receiver_email.as_deref()])
        .map(normalize_address)
        .filter(|a| !a.is_empty())
        .collect();
    addresses.sort();
    addresses.dedup();
    if addresses.is_empty() {
        return Ok(());
    }
    let marks = vec!["?"; addresses.len()].join(",");
    let known: Vec<(String, String)> = Sql::new(
        format!(
            "SELECT email, display_name FROM email_participants
             WHERE display_name IS NOT NULL AND TRIM(display_name) <> '' AND email IN ({marks})
             ORDER BY id"
        ),
        addresses.into_iter().map(Arg::from).collect(),
    )
    .query_as()
    .fetch_all(pool)
    .await?;
    let mut names = HashMap::new();
    for (email, name) in known {
        let email = normalize_address(Some(&email));
        let name = name.trim().to_string();
        if !email.is_empty() && !name.is_empty() {
            names.entry(email).or_insert(name);
        }
    }
    for row in rows.iter_mut() {
        apply(row, &names);
    }
    Ok(())
}
