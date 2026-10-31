//! Which threads, and which rows in them, a user may see.
//!
//! A text is the rep's by its user or by its number, either one: "mine" is
//! the threads carrying a row sent by the user or tagged with an agent id
//! they have or ever had there (the user sees their own rows plus untagged
//! ones), and the threads texted to or from a number of theirs, now or ever,
//! on any provider (every row; `user_sms_identities`). "All" — admins only —
//! is the whole company, or one rep's "mine" when filtered by agent.
//! Company lines and the onboarding sample are never thread keys.
//!
//! Telnyx: "mine" is every thread of the numbers the user may call from
//! (whole threads, so a shared line is a shared inbox); the rep filter takes
//! the rep's user id.

use super::provider::{Param, SAMPLE_PHONES, SmsProvider, marks, phone10};
use crate::auth::session::SessionUser;
use crate::openapi::ToSchema;
use serde::Deserialize;
use sqlx::MySqlPool;
use std::collections::BTreeSet;

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
    #[must_use]
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
    let Some(column) = provider.agent_column() else {
        // Telnyx: the rep's user id.
        let Ok(rep) = wanted.parse::<i32>() else {
            return Ok(None);
        };
        let found: Option<i32> = sqlx::query_scalar(
            "SELECT id FROM users WHERE id = ? AND company_id = ? AND is_deleted = 0",
        )
        .bind(rep)
        .bind(user.company_id)
        .fetch_optional(pool)
        .await?;
        return Ok(found.map(|id| id.to_string()));
    };
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
    /// The rep whose own rows (plus untagged ones) are visible.
    pub own: Option<OwnRows>,
    /// Company lines, never thread keys.
    pub company_lines: Vec<String>,
    /// One rep's inbox (unread first), not the company's.
    pub one_rep: bool,
}

/// A rep's own rows: tagged with an agent id of theirs, or sent by them.
#[derive(Debug, Clone)]
pub struct OwnRows {
    agents: Vec<String>,
    sender_user_id: Option<i32>,
}

impl OwnRows {
    fn new(ids: &RepIds) -> Option<Self> {
        (!ids.agents.is_empty() || ids.user_id.is_some()).then(|| Self {
            agents: ids.agents.clone(),
            sender_user_id: ids.user_id,
        })
    }

    /// The predicates over `s` for the rep's own rows, ORed.
    fn own(&self) -> (Vec<String>, Vec<Param>) {
        let mut parts = Vec::new();
        let mut params: Vec<Param> = Vec::new();
        if !self.agents.is_empty() {
            parts.push(format!("s.agent IN ({})", marks(self.agents.len())));
            params.extend(self.agents.iter().map(Param::from));
        }
        if let Some(user_id) = self.sender_user_id {
            parts.push("s.sender_user_id = ?".to_string());
            params.push(user_id.into());
        }
        (parts, params)
    }

    /// The rep's own rows plus untagged ones, ORed.
    fn visible(&self) -> (Vec<String>, Vec<Param>) {
        let (mut parts, params) = self.own();
        parts.push("s.agent IS NULL".to_string());
        (parts, params)
    }
}

/// A rep's ids on `CloudTalk` or `RingCentral`: every agent id they have or
/// had there, and every number of theirs they have or had on any provider (a
/// number moved between providers stays theirs). Company lines are shared,
/// never one rep's.
struct RepIds {
    user_id: Option<i32>,
    agents: Vec<String>,
    lines: Vec<String>,
}

/// The caller's ids, or with `scope=all` those of the rep whose agent id is
/// `filter_agent`.
async fn rep_ids(
    pool: &MySqlPool,
    provider: SmsProvider,
    user: &SessionUser,
    scope: SmsScope,
    filter_agent: Option<&str>,
    company_lines: &[String],
) -> Result<RepIds, sqlx::Error> {
    let (user_id, agent, line) = if scope == SmsScope::All {
        let (Some(agent), Some(agent_column), Some(phone_column)) = (
            filter_agent,
            provider.agent_column(),
            provider.phone_column(),
        ) else {
            return Ok(RepIds {
                user_id: None,
                agents: Vec::new(),
                lines: Vec::new(),
            });
        };
        let rep: Option<(i32, Option<String>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT id, {phone_column} FROM users
              WHERE company_id = ? AND is_deleted = 0 AND TRIM(IFNULL({agent_column}, '')) = ?
              ORDER BY id LIMIT 1"
        )))
        .bind(user.company_id)
        .bind(agent)
        .fetch_optional(pool)
        .await?;
        let line = rep
            .as_ref()
            .and_then(|(_, phone)| phone.as_deref())
            .map(phone10);
        (rep.map(|(id, _)| id), Some(agent.to_string()), line)
    } else {
        (
            Some(user.id),
            provider.agent_of(user),
            provider.phone_of(user),
        )
    };
    let mut agents: BTreeSet<String> = agent.into_iter().collect();
    let mut lines: BTreeSet<String> = line.into_iter().collect();
    if let Some(user_id) = user_id {
        let remembered: Vec<(String, String)> = sqlx::query_as(
            "SELECT CAST(kind AS CHAR), value FROM user_sms_identities
              WHERE company_id = ? AND user_id = ? AND (kind = 'line' OR provider = ?)",
        )
        .bind(user.company_id)
        .bind(user_id)
        .bind(provider.as_str())
        .fetch_all(pool)
        .await?;
        for (kind, value) in remembered {
            if kind == "agent" {
                agents.insert(value);
            } else {
                lines.insert(value);
            }
        }
    }
    lines.retain(|line| line.len() == 10 && !company_lines.contains(line));
    Ok(RepIds {
        user_id,
        agents: agents.into_iter().collect(),
        lines: lines.into_iter().collect(),
    })
}

/// Whether `user` has anything of theirs on `provider`: an agent id or
/// number they have or had (on Telnyx, a line they may call from now), or a
/// text they sent (on Telnyx, also one tagged with their user id).
pub async fn has_own(
    pool: &MySqlPool,
    provider: SmsProvider,
    user: &SessionUser,
) -> Result<bool, sqlx::Error> {
    if provider == SmsProvider::Telnyx {
        if !crate::phone::texting::inbox_lines(pool, user.company_id, user.id)
            .await?
            .is_empty()
        {
            return Ok(true);
        }
        // idx_sms_agent_threads (company_id, agent, ...).
        let tagged = exists(
            pool,
            "SELECT 1 FROM telnyx_sms s WHERE s.company_id = ? AND s.agent = ? LIMIT 1".to_string(),
            &[user.company_id.into(), user.id.to_string().into()],
        )
        .await?;
        if tagged {
            return Ok(true);
        }
    } else {
        let company_lines = company_lines(pool, provider, user.company_id).await?;
        let ids = rep_ids(pool, provider, user, SmsScope::Mine, None, &company_lines).await?;
        if !ids.agents.is_empty() || !ids.lines.is_empty() {
            return Ok(true);
        }
    }
    // idx_{provider}_sms_sender_user (sender_user_id).
    exists(
        pool,
        format!(
            "SELECT 1 FROM {} s WHERE s.sender_user_id = ? AND s.company_id = ? LIMIT 1",
            provider.sms_table()
        ),
        &[user.id.into(), user.company_id.into()],
    )
    .await
}

pub async fn company_lines(
    pool: &MySqlPool,
    provider: SmsProvider,
    company_id: i32,
) -> Result<Vec<String>, sqlx::Error> {
    if provider == SmsProvider::Telnyx {
        return crate::phone::texting::company_lines(pool, company_id).await;
    }
    sqlx::query_scalar(
        "SELECT phone10 FROM sms_company_phones
          WHERE provider = ? AND company_id = ? AND agent_sender = 1 AND inbound_recipient = 1",
    )
    .bind(provider.as_str())
    .bind(company_id)
    .fetch_all(pool)
    .await
}

/// Threads carrying a row sent by the rep or tagged with an agent id of theirs.
async fn own_threads(
    pool: &MySqlPool,
    provider: SmsProvider,
    company_id: i32,
    own: &OwnRows,
) -> Result<Vec<String>, sqlx::Error> {
    let table = provider.sms_table();
    let mut threads = Vec::new();
    if !own.agents.is_empty() {
        // idx_sms_agent_threads (company_id, agent, phone_digits): index only.
        let mut params: Vec<Param> = vec![company_id.into()];
        params.extend(own.agents.iter().map(Param::from));
        let rows: Vec<(String,)> = super::provider::fetch_all(
            pool,
            format!(
                "SELECT DISTINCT s.phone_digits FROM {table} s
                  WHERE s.company_id = ? AND s.agent IN ({}) AND s.phone_digits IS NOT NULL",
                marks(own.agents.len())
            ),
            &params,
        )
        .await?;
        threads.extend(rows.into_iter().map(|(phone,)| phone));
    }
    if let Some(user_id) = own.sender_user_id {
        // idx_{provider}_sms_sender_user (sender_user_id).
        let rows: Vec<(String,)> = super::provider::fetch_all(
            pool,
            format!(
                "SELECT DISTINCT s.phone_digits FROM {table} s
                  WHERE s.sender_user_id = ? AND s.company_id = ? AND s.phone_digits IS NOT NULL"
            ),
            &[user_id.into(), company_id.into()],
        )
        .await?;
        threads.extend(rows.into_iter().map(|(phone,)| phone));
    }
    Ok(threads)
}

/// `(texted to one of `lines`, or from one)` over `s`, and its binds.
fn on_lines(lines: &[String]) -> (String, Vec<Param>) {
    let marks = marks(lines.len());
    let mut params: Vec<Param> = lines.iter().map(Param::from).collect();
    params.extend(lines.iter().map(Param::from));
    (
        format!(
            "((s.direction = 'inbound' AND s.recipient10 IN ({marks}))
              OR (s.direction = 'outbound' AND s.sender10 IN ({marks})))"
        ),
        params,
    )
}

async fn my_number_threads(
    pool: &MySqlPool,
    provider: SmsProvider,
    company_id: i32,
    lines: &[String],
) -> Result<Vec<String>, sqlx::Error> {
    if lines.is_empty() {
        return Ok(Vec::new());
    }
    // idx_sms_inbound_recipient (company_id, direction, recipient10, ...) and
    // idx_sms_sender10 (company_id, sender10).
    let (on_lines, line_params) = on_lines(lines);
    let mut params: Vec<Param> = vec![company_id.into()];
    params.extend(line_params);
    let rows: Vec<(String,)> = super::provider::fetch_all(
        pool,
        format!(
            "SELECT DISTINCT s.phone_digits FROM {} s
              WHERE s.company_id = ? AND {on_lines}
                AND s.is_echo = 0 AND s.phone_digits IS NOT NULL",
            provider.sms_table()
        ),
        &params,
    )
    .await?;
    Ok(rows.into_iter().map(|(phone,)| phone).collect())
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
            own: None,
            company_lines,
            one_rep: false,
        });
    }
    if provider == SmsProvider::Telnyx {
        let (rep, lines) = telnyx_lines(pool, user, scope, filter_agent).await?;
        let threads: Vec<String> =
            crate::phone::texting::line_threads(pool, user.company_id, &lines, rep)
                .await?
                .into_iter()
                .filter(|phone| {
                    !company_lines.contains(phone) && !SAMPLE_PHONES.contains(&phone.as_str())
                })
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
        return Ok(Narrowing {
            phones: Some(threads.clone()),
            whole_threads: threads,
            own: None,
            company_lines,
            one_rep: true,
        });
    }
    let ids = rep_ids(pool, provider, user, scope, filter_agent, &company_lines).await?;
    let own = OwnRows::new(&ids);
    let own_phones = match &own {
        Some(own) => own_threads(pool, provider, user.company_id, own).await?,
        None => Vec::new(),
    };
    let mine = my_number_threads(pool, provider, user.company_id, &ids.lines).await?;
    let excluded =
        |phone: &String| company_lines.contains(phone) || SAMPLE_PHONES.contains(&phone.as_str());
    let whole_threads: Vec<String> = mine
        .iter()
        .filter(|phone| !excluded(phone))
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let phones: Vec<String> = own_phones
        .into_iter()
        .chain(mine)
        .filter(|phone| !excluded(phone))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    Ok(Narrowing {
        phones: Some(phones),
        whole_threads,
        one_rep: own.is_some(),
        own,
        company_lines,
    })
}

/// The rep whose Telnyx threads are visible, the caller or with
/// `scope=all` the filtered rep (`filter` = user id), and their lines
/// (last ten digits).
async fn telnyx_lines(
    pool: &MySqlPool,
    user: &SessionUser,
    scope: SmsScope,
    filter: Option<&str>,
) -> Result<(Option<i32>, Vec<String>), sqlx::Error> {
    let rep = if scope == SmsScope::All {
        filter.and_then(|id| id.parse::<i32>().ok())
    } else {
        Some(user.id)
    };
    let lines = match rep {
        Some(rep) => crate::phone::texting::inbox_lines(pool, user.company_id, rep).await?,
        None => Vec::new(),
    };
    Ok((rep, lines))
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
        // The own-rows branch is row-level (own rows + untagged); the "my
        // number" branch is thread-level.
        if let Some(own) = &self.own {
            let (mut visible, own_params) = own.visible();
            params.extend(own_params);
            if !self.whole_threads.is_empty() {
                visible.push(format!(
                    "s.phone_digits IN ({})",
                    marks(self.whole_threads.len())
                ));
                params.extend(self.whole_threads.iter().map(Param::from));
            }
            parts.push(format!("({})", visible.join(" OR ")));
        }
        (parts.join(" AND "), params)
    }
}

/// Access to one thread, and the extra row filter inside it.
pub struct ThreadAccess {
    /// Extra predicate over `s` and its binds, e.g.
    /// `(s.agent IN (?) OR s.sender_user_id = ? OR s.agent IS NULL)`.
    pub clause: Option<(String, Vec<Param>)>,
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
    if provider == SmsProvider::Telnyx {
        let (rep, lines) = telnyx_lines(pool, user, scope, filter_agent).await?;
        let seen =
            crate::phone::texting::thread_on_lines(pool, user.company_id, &lines, rep, phone)
                .await?;
        return Ok(seen.then_some(ThreadAccess { clause: None }));
    }
    let company_lines = company_lines(pool, provider, user.company_id).await?;
    let ids = rep_ids(pool, provider, user, scope, filter_agent, &company_lines).await?;
    if !ids.lines.is_empty() {
        let (on_lines, line_params) = on_lines(&ids.lines);
        let mut params = vec![company.clone(), phone.into()];
        params.extend(line_params);
        let on_my_number = exists(
            pool,
            format!(
                "SELECT 1 FROM {table} s
                  WHERE s.company_id = ? AND s.phone_digits = ? AND s.is_echo = 0
                    AND {on_lines}
                  LIMIT 1"
            ),
            &params,
        )
        .await?;
        if on_my_number {
            return Ok(Some(ThreadAccess { clause: None }));
        }
    }
    if let Some(own) = OwnRows::new(&ids) {
        let (mine, mine_params) = own.own();
        let mut params = vec![company, phone.into()];
        params.extend(mine_params);
        let tagged = exists(
            pool,
            format!(
                "SELECT 1 FROM {table} s
                  WHERE s.company_id = ? AND s.phone_digits = ? AND ({})
                  LIMIT 1",
                mine.join(" OR ")
            ),
            &params,
        )
        .await?;
        if tagged {
            let (visible, params) = own.visible();
            return Ok(Some(ThreadAccess {
                clause: Some((format!("({})", visible.join(" OR ")), params)),
            }));
        }
    }
    Ok(None)
}
