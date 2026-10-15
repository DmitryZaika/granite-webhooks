//! Which threads, and which rows in them, a user may see.
//!
//! "Mine" is the threads carrying a row tagged with the user's agent id (the
//! user sees their own rows plus untagged ones) and the threads of untagged
//! inbound rows sent to the user's own number (every row). "All" — admins
//! only — is the whole company, or one rep's "mine" when filtered by agent.
//! Company lines and the onboarding sample are never thread keys.

use super::provider::{Param, SAMPLE_PHONES, SmsProvider, marks};
use crate::auth::session::SessionUser;
use serde::Deserialize;
use sqlx::MySqlPool;
use std::collections::BTreeSet;
use utoipa::ToSchema;

/// Whose conversations to list.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum SmsScope {
    /// The caller's own conversations.
    #[default]
    Mine,
    /// The whole company (admins; anyone else gets `mine`).
    All,
}

impl SmsScope {
    /// `all` only for admins.
    pub const fn for_user(self, user: &SessionUser) -> Self {
        match self {
            Self::All if user.is_admin || user.is_superuser => Self::All,
            _ => Self::Mine,
        }
    }
}

/// A company user with `agent_id`, when the scope is `all`; anything else
/// filters nothing.
pub async fn filter_agent(
    pool: &MySqlPool,
    provider: SmsProvider,
    user: &SessionUser,
    scope: SmsScope,
    agent_id: Option<&str>,
) -> Result<Option<String>, sqlx::Error> {
    let wanted = agent_id.map(str::trim).unwrap_or_default();
    if scope != SmsScope::All || wanted.is_empty() {
        return Ok(None);
    }
    let column = provider.agent_column();
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT TRIM({column}) FROM users
          WHERE company_id = ? AND is_deleted = 0 AND TRIM(IFNULL({column}, '')) = ?
          LIMIT 1"
    )))
    .bind(user.company_id)
    .bind(wanted)
    .fetch_optional(pool)
    .await
}

/// The threads a user may see.
#[derive(Debug)]
pub struct Narrowing {
    /// Visible thread keys; `None` = the whole company.
    pub phones: Option<Vec<String>>,
    /// Threads whose every row is visible (the "my number" branch).
    pub whole_threads: Vec<String>,
    /// Agent whose own rows (plus untagged ones) are visible.
    pub agent_id: Option<String>,
    /// Company lines, never thread keys.
    pub company_lines: Vec<String>,
}

pub async fn company_lines(
    pool: &MySqlPool,
    provider: SmsProvider,
    company_id: i32,
) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT phone10 FROM sms_company_phones
          WHERE provider = ? AND company_id = ? AND agent_sender = 1 AND inbound_recipient = 1",
    )
    .bind(provider.as_str())
    .bind(company_id)
    .fetch_all(pool)
    .await
}

async fn agent_threads(
    pool: &MySqlPool,
    provider: SmsProvider,
    company_id: i32,
    agent_id: &str,
) -> Result<Vec<String>, sqlx::Error> {
    // idx_sms_agent_threads (company_id, agent, phone_digits): index only.
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT DISTINCT s.phone_digits FROM {} s
          WHERE s.company_id = ? AND s.agent = ? AND s.phone_digits IS NOT NULL",
        provider.sms_table()
    )))
    .bind(company_id)
    .bind(agent_id)
    .fetch_all(pool)
    .await
}

async fn my_number_threads(
    pool: &MySqlPool,
    provider: SmsProvider,
    company_id: i32,
    phone10: &str,
) -> Result<Vec<String>, sqlx::Error> {
    // idx_sms_inbound_recipient (company_id, direction, recipient10, agent, phone_digits).
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT DISTINCT s.phone_digits FROM {} s
          WHERE s.company_id = ? AND s.direction = 'inbound' AND s.agent IS NULL
            AND s.recipient10 = ? AND s.is_echo = 0 AND s.phone_digits IS NOT NULL",
        provider.sms_table()
    )))
    .bind(company_id)
    .bind(phone10)
    .fetch_all(pool)
    .await
}

pub async fn narrowing(
    pool: &MySqlPool,
    provider: SmsProvider,
    user: &SessionUser,
    scope: SmsScope,
    filter_agent: Option<&str>,
) -> Result<Narrowing, sqlx::Error> {
    let company_lines = company_lines(pool, provider, user.company_id).await?;
    if scope == SmsScope::All && filter_agent.is_none() {
        return Ok(Narrowing {
            phones: None,
            whole_threads: Vec::new(),
            agent_id: None,
            company_lines,
        });
    }
    let (agent_id, phone10) = if scope == SmsScope::All {
        (filter_agent.map(str::to_string), None)
    } else {
        (provider.agent_of(user), provider.phone_of(user))
    };
    let agent_phones = match &agent_id {
        Some(agent) => agent_threads(pool, provider, user.company_id, agent).await?,
        None => Vec::new(),
    };
    let mine = match &phone10 {
        Some(phone) => my_number_threads(pool, provider, user.company_id, phone).await?,
        None => Vec::new(),
    };
    let excluded =
        |phone: &String| company_lines.contains(phone) || SAMPLE_PHONES.contains(&phone.as_str());
    let whole_threads: Vec<String> = mine
        .iter()
        .filter(|phone| !excluded(phone))
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let phones: Vec<String> = agent_phones
        .into_iter()
        .chain(mine)
        .filter(|phone| !excluded(phone))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    Ok(Narrowing {
        phones: Some(phones),
        whole_threads,
        agent_id,
        company_lines,
    })
}

impl Narrowing {
    /// Nothing is visible at all.
    pub fn is_empty(&self) -> bool {
        self.phones.as_ref().is_some_and(Vec::is_empty)
    }

    /// WHERE over the SMS table aliased `s` for the visible rows.
    pub fn row_filter(&self, company_id: i32) -> (String, Vec<Param>) {
        let mut parts = vec!["s.company_id = ?".to_string(), "s.is_echo = 0".to_string()];
        let mut params: Vec<Param> = vec![company_id.into()];
        let Some(phones) = &self.phones else {
            parts.push(format!(
                "s.phone_digits NOT IN ({})",
                marks(SAMPLE_PHONES.len())
            ));
            params.extend(SAMPLE_PHONES.iter().map(|phone| Param::from(*phone)));
            if !self.company_lines.is_empty() {
                parts.push(format!(
                    "s.phone_digits NOT IN ({})",
                    marks(self.company_lines.len())
                ));
                params.extend(self.company_lines.iter().map(Param::from));
            }
            return (parts.join(" AND "), params);
        };
        parts.push(format!("s.phone_digits IN ({})", marks(phones.len())));
        params.extend(phones.iter().map(Param::from));
        // The agent branch is row-level (own rows + untagged); the "my number"
        // branch is thread-level.
        if let Some(agent) = &self.agent_id {
            let mut visible = "s.agent = ? OR s.agent IS NULL".to_string();
            params.push(agent.into());
            if !self.whole_threads.is_empty() {
                visible.push_str(" OR s.phone_digits IN (");
                visible.push_str(&marks(self.whole_threads.len()));
                visible.push(')');
                params.extend(self.whole_threads.iter().map(Param::from));
            }
            parts.push(format!("({visible})"));
        }
        (parts.join(" AND "), params)
    }
}

/// Access to one thread, and the extra row filter inside it.
pub struct ThreadAccess {
    /// Extra predicate over `s`, e.g. `(s.agent = ? OR s.agent IS NULL)`.
    pub clause: Option<(String, Param)>,
}

async fn exists(pool: &MySqlPool, sql: String, params: &[Param]) -> Result<bool, sqlx::Error> {
    let rows: Vec<(i32,)> = super::provider::fetch_all(pool, sql, params).await?;
    Ok(!rows.is_empty())
}

/// Per-phone index lookups rather than the company-wide narrowing. `None`
/// when the user may not see the thread.
pub async fn thread_access(
    pool: &MySqlPool,
    provider: SmsProvider,
    user: &SessionUser,
    scope: SmsScope,
    filter_agent: Option<&str>,
    phone: &str,
) -> Result<Option<ThreadAccess>, sqlx::Error> {
    let table = provider.sms_table();
    let company: Param = user.company_id.into();
    if scope == SmsScope::All && filter_agent.is_none() {
        let found = exists(
            pool,
            format!(
                "SELECT 1 FROM {table} s WHERE s.company_id = ? AND s.phone_digits = ? LIMIT 1"
            ),
            &[company, phone.into()],
        )
        .await?;
        return Ok(found.then_some(ThreadAccess { clause: None }));
    }
    let (agent_id, phone10) = if scope == SmsScope::All {
        (filter_agent.map(str::to_string), None)
    } else {
        (provider.agent_of(user), provider.phone_of(user))
    };
    if let Some(mine) = &phone10 {
        let to_my_number = exists(
            pool,
            format!(
                "SELECT 1 FROM {table} s
                  WHERE s.company_id = ? AND s.direction = 'inbound' AND s.agent IS NULL
                    AND s.recipient10 = ? AND s.is_echo = 0 AND s.phone_digits = ?
                  LIMIT 1"
            ),
            &[company.clone(), mine.into(), phone.into()],
        )
        .await?;
        if to_my_number {
            return Ok(Some(ThreadAccess { clause: None }));
        }
    }
    if let Some(agent) = agent_id {
        let tagged = exists(
            pool,
            format!(
                "SELECT 1 FROM {table} s
                  WHERE s.company_id = ? AND s.agent = ? AND s.phone_digits = ? LIMIT 1"
            ),
            &[company, (&agent).into(), phone.into()],
        )
        .await?;
        if tagged {
            return Ok(Some(ThreadAccess {
                clause: Some(("(s.agent = ? OR s.agent IS NULL)".to_string(), agent.into())),
            }));
        }
    }
    Ok(None)
}
