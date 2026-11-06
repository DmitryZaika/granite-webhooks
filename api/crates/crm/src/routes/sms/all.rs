//! The inboxes over every provider, each provider's conversations merged by
//! phone so texts show whichever provider carried them: the admin inbox
//! (`/v1/sms/all/...`, the whole company; a rep filter takes the rep's user
//! id and reads each provider through that rep's agent id there) and the
//! caller's own (`/v1/sms/mine/...`, every provider the caller texts
//! through, for people on more than one).

use super::provider::{SAMPLE_PHONES, SmsProvider, phone_variants, phone10};
use super::rollup::Mode;
use super::schemas::{
    DesktopAlert, DesktopAlerts, Direction, SalesRep, ThreadDetail, ThreadList, ThreadQuery,
    ThreadSummary, ThreadsQuery, UnreadCount,
};
use super::scope::{SmsScope, has_own, thread_access};
use super::send::seat;
use super::thread::{
    active_flow, checked_phone, customer_by_phone, messages, set_marker, thread_unread,
};
use super::threads::{
    RollupRow, customers_by_phone, last_rows, provider_alerts, rollup, suppliers_by_phone,
    unread_threads, upkeep,
};
use crate::auth::session::SessionUser;
use crate::auth::{AdminUser, EmployeeUser};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::MultiQuery;
use crate::openapi::IntoParams;
use crate::routes::company::queries::company_features;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use sqlx::MySqlPool;
use std::collections::{HashMap, HashSet};

/// The providers to read and the agent id to narrow each to: every provider
/// unfiltered without a rep, else those the rep (a user id) has an agent id on.
async fn sources(
    pool: &MySqlPool,
    company_id: i32,
    rep: Option<&str>,
) -> Result<Vec<(SmsProvider, Option<String>)>, sqlx::Error> {
    let Some(rep) = rep.map(str::trim).filter(|rep| !rep.is_empty()) else {
        return Ok(SmsProvider::ALL.iter().map(|p| (*p, None)).collect());
    };
    let Ok(user_id) = rep.parse::<i32>() else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for provider in SmsProvider::ALL {
        let Some(column) = provider.agent_column() else {
            // Telnyx: the rep by user id, when they have a line.
            if !crate::phone::texting::inbox_lines(pool, company_id, user_id)
                .await?
                .is_empty()
            {
                out.push((provider, Some(user_id.to_string())));
            }
            continue;
        };
        let agent: Option<Option<String>> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT TRIM({column}) FROM users WHERE id = ? AND company_id = ? AND is_deleted = 0"
        )))
        .bind(user_id)
        .bind(company_id)
        .fetch_optional(pool)
        .await?;
        if let Some(agent) = agent.flatten().filter(|agent| !agent.is_empty()) {
            out.push((provider, Some(agent)));
        }
    }
    Ok(out)
}

/// One phone's conversation over the providers, carried by the latest row.
struct Merged {
    provider: SmsProvider,
    row: RollupRow,
    /// What the inbox orders by.
    sort_at: DateTime<Utc>,
}

/// List conversations over every provider
///
/// A page of the company's SMS conversations through any provider, merged by
/// phone, latest first — or unread first when narrowed to one rep
/// (`agent_id` = the rep's user id from `/v1/sms/all/sales-reps`). A search
/// matches phone digits, message text and customer or supplier names.
#[crate::openapi::path(
    get,
    path = "/v1/sms/all/threads",
    operation_id = "list_all_sms_threads",
    tag = "sms",
    params(ThreadsQuery),
    responses(
        (status = 200, description = "A page of conversations", body = ThreadList),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list_threads(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    MultiQuery(query): MultiQuery<ThreadsQuery>,
) -> ApiResult<Json<ThreadList>> {
    let sources = sources(&state.pool, user.company_id, query.agent_id.as_deref()).await?;
    let one_rep = query
        .agent_id
        .as_deref()
        .is_some_and(|rep| !rep.trim().is_empty());
    merged_threads(&state, &user, &query, &sources, SmsScope::All, one_rep).await
}

/// List my conversations over every provider
///
/// The caller's own SMS conversations through every provider they text
/// through or have texted through (a `CloudTalk` agent id, a `RingCentral`
/// extension, a Telnyx line), merged by phone: unread first, then latest.
/// Each provider's part is what its own inbox shows the caller. A search
/// matches phone digits, message text and customer or supplier names.
#[crate::openapi::path(
    get,
    path = "/v1/sms/mine/threads",
    operation_id = "list_my_sms_threads",
    tag = "sms",
    params(ThreadsQuery),
    responses(
        (status = 200, description = "A page of conversations", body = ThreadList),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list_my_threads(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    MultiQuery(query): MultiQuery<ThreadsQuery>,
) -> ApiResult<Json<ThreadList>> {
    let sources: Vec<(SmsProvider, Option<String>)> = own_providers(&state.pool, &user)
        .await?
        .into_iter()
        .map(|provider| (provider, None))
        .collect();
    merged_threads(&state, &user, &query, &sources, SmsScope::Mine, true).await
}

/// The providers holding texts of the caller's: by an id or number they have
/// or had there, or a text they sent ([`has_own`]).
async fn own_providers(
    pool: &MySqlPool,
    user: &SessionUser,
) -> Result<Vec<SmsProvider>, sqlx::Error> {
    let mut out = Vec::new();
    for provider in SmsProvider::ALL {
        if has_own(pool, provider, user).await? {
            out.push(provider);
        }
    }
    Ok(out)
}

/// The providers the caller can text through now: a seat there, and the
/// company connected to it.
async fn sendable_providers(
    pool: &MySqlPool,
    user: &SessionUser,
) -> Result<Vec<SmsProvider>, sqlx::Error> {
    let features = company_features(pool, user.company_id).await?;
    let mut out = Vec::new();
    for provider in SmsProvider::ALL {
        let connected = match provider {
            SmsProvider::Cloudtalk => features.cloudtalk,
            SmsProvider::Ringcentral => features.ringcentral,
            SmsProvider::Telnyx => features.telnyx_sms,
        };
        if connected && seat(pool, provider, user).await?.is_some() {
            out.push(provider);
        }
    }
    Ok(out)
}

/// A page of `sources`' conversations merged by phone: `scope` per
/// provider, unread first when `one_rep`.
async fn merged_threads(
    state: &AppState,
    user: &SessionUser,
    query: &ThreadsQuery,
    sources: &[(SmsProvider, Option<String>)],
    scope: SmsScope,
    one_rep: bool,
) -> ApiResult<Json<ThreadList>> {
    let pool = &state.pool;
    let user = user.clone();
    let term: String = query.search.chars().take(100).collect();
    let limit = query.limit.unwrap_or(20).clamp(1, 100);
    let offset = query.offset.unwrap_or(0).max(0);

    // Every provider's first `offset + limit` threads hold this page's.
    let window = offset + limit;
    let mut by_phone: HashMap<String, Merged> = HashMap::new();
    let mut total_count = 0;
    let mut unread_count = 0;
    let mut more = false;
    for (provider, agent) in sources {
        upkeep(state, *provider, user.company_id).await?;
        let rows: Vec<RollupRow> = rollup(
            pool,
            *provider,
            &user,
            scope,
            agent.as_deref(),
            &term,
            Mode::List {
                limit: window,
                offset: 0,
            },
        )
        .await?;
        match rows.first() {
            Some(first) => {
                total_count += first.total_count;
                unread_count += first.total_unread;
                more |= first.total_count > window;
            }
            None => {
                unread_count +=
                    unread_threads(pool, *provider, &user, scope, agent.as_deref()).await?;
            }
        }
        for row in rows {
            let sort_at = if one_rep {
                row.last_at
            } else {
                row.last_inbound_at.unwrap_or(row.last_at)
            };
            let key = phone10(&row.phone_digits);
            let Some(seen) = by_phone.get_mut(&key) else {
                by_phone.insert(
                    key,
                    Merged {
                        provider: *provider,
                        row,
                        sort_at,
                    },
                );
                continue;
            };
            // The same phone through another provider: one conversation.
            total_count -= 1;
            seen.sort_at = seen.sort_at.max(sort_at);
            let message_count = seen.row.message_count + row.message_count;
            let unread = seen.row.unread + row.unread;
            if row.last_at > seen.row.last_at {
                seen.provider = *provider;
                seen.row = row;
            }
            seen.row.message_count = message_count;
            seen.row.unread = unread;
        }
    }

    let mut merged: Vec<Merged> = by_phone.into_values().collect();
    merged.sort_by(|a, b| {
        let unread_first = if one_rep {
            (b.row.unread > 0).cmp(&(a.row.unread > 0))
        } else {
            std::cmp::Ordering::Equal
        };
        unread_first
            .then(b.sort_at.cmp(&a.sort_at))
            .then(b.row.last_at.cmp(&a.row.last_at))
    });
    let has_more = more || i64::try_from(merged.len()).unwrap_or(0) > window;
    let page: Vec<Merged> = merged
        .into_iter()
        .skip(usize::try_from(offset).unwrap_or(0))
        .take(usize::try_from(limit).unwrap_or(0))
        .collect();
    Ok(Json(ThreadList {
        threads: summaries(pool, user.company_id, page).await?,
        total_count,
        unread_count,
        has_more,
    }))
}

/// The page's conversations with their latest text and customer or supplier.
async fn summaries(
    pool: &MySqlPool,
    company_id: i32,
    page: Vec<Merged>,
) -> Result<Vec<ThreadSummary>, sqlx::Error> {
    let phones: Vec<String> = page
        .iter()
        .map(|m| phone10(&m.row.phone_digits))
        .filter(|phone| !phone.is_empty())
        .collect();
    let mut last = HashMap::new();
    let mut customers = HashMap::new();
    for provider in SmsProvider::ALL {
        let ids: Vec<i64> = page
            .iter()
            .filter(|m| m.provider == provider)
            .map(|m| m.row.last_id)
            .collect();
        for (id, row) in last_rows(pool, provider, company_id, &ids).await? {
            last.insert((provider.as_str(), id), row);
        }
        for (phone, customer) in customers_by_phone(pool, provider, company_id, &phones).await? {
            customers.entry(phone).or_insert(customer);
        }
    }
    let suppliers = suppliers_by_phone(pool, company_id, &phones).await?;

    Ok(page
        .into_iter()
        .map(|Merged { provider, row, .. }| {
            let key = phone10(&row.phone_digits);
            let customer = customers.get(&key);
            let last = last.get(&(provider.as_str(), row.last_id));
            let variants = phone_variants(&row.phone_digits);
            let inbound = last
                .and_then(|l| l.sender.as_ref())
                .is_some_and(|sender| variants.contains(sender));
            ThreadSummary {
                customer_id: customer.map(|c| c.0),
                customer_name: customer
                    .and_then(|c| c.1.clone())
                    .or_else(|| suppliers.get(&key).cloned()),
                last_message_text: last.map(|l| l.text.clone()).unwrap_or_default(),
                last_message_at: row.last_at,
                last_direction: if inbound {
                    Direction::Inbound
                } else {
                    Direction::Outbound
                },
                last_agent: last.and_then(|l| l.agent.clone()),
                message_count: row.message_count,
                unread_count: row.unread,
                phone_digits: row.phone_digits,
            }
        })
        .collect())
}

/// Get a conversation over every provider
///
/// The newest messages with one phone through any provider, oldest first
/// (page back by raising `limit`; `before_id` is ignored), the customer or
/// supplier it belongs to and the SMS flow running on it. `agent_id` reads
/// as one rep (their user id). Never sendable: the admin inbox is read-only.
#[crate::openapi::path(
    get,
    path = "/v1/sms/all/threads/{phone}",
    operation_id = "get_all_sms_thread",
    tag = "sms",
    params(
        ("phone" = String, Path, description = "The other party's phone digits"),
        ThreadQuery,
    ),
    responses(
        (status = 200, description = "The conversation", body = ThreadDetail),
        (status = 400, description = "Not a phone number", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "The onboarding sample", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get_thread(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(phone): Path<String>,
    MultiQuery(query): MultiQuery<ThreadQuery>,
) -> ApiResult<Json<ThreadDetail>> {
    let sources = sources(&state.pool, user.company_id, query.agent_id.as_deref()).await?;
    merged_thread(&state, &user, &phone, &query, &sources, SmsScope::All).await
}

/// Get my conversation over every provider
///
/// The newest messages with one phone through every provider the caller
/// texts through (as `GET /v1/sms/mine/threads`), oldest first, each with
/// the provider that carried it (page back by raising `limit`;
/// `before_id` is ignored); the customer or supplier it belongs to and the
/// SMS flow running on it. `replyProvider` is the provider to answer
/// through: the one of the latest message the caller can text through,
/// else the first they can; `canSend` is false when they can text through
/// none. Opening it in the caller's own inbox marks it read (`unreadCount`
/// is 0).
#[crate::openapi::path(
    get,
    path = "/v1/sms/mine/threads/{phone}",
    operation_id = "get_my_sms_thread",
    tag = "sms",
    params(
        ("phone" = String, Path, description = "The other party's phone digits"),
        ThreadQuery,
    ),
    responses(
        (status = 200, description = "The conversation", body = ThreadDetail),
        (status = 400, description = "Not a phone number", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "The onboarding sample", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get_my_thread(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(phone): Path<String>,
    MultiQuery(query): MultiQuery<ThreadQuery>,
) -> ApiResult<Json<ThreadDetail>> {
    let sources: Vec<(SmsProvider, Option<String>)> = own_providers(&state.pool, &user)
        .await?
        .into_iter()
        .map(|provider| (provider, None))
        .collect();
    let Json(mut thread) =
        merged_thread(&state, &user, &phone, &query, &sources, SmsScope::Mine).await?;
    let sendable = sendable_providers(&state.pool, &user).await?;
    let latest = thread
        .messages
        .iter()
        .rev()
        .map(|message| message.provider)
        .find(|provider| sendable.contains(provider));
    thread.reply_provider = latest.or_else(|| sendable.first().copied());
    thread.can_send = thread.reply_provider.is_some();
    // The caller's own inbox marks the thread read on open.
    thread.unread_count = 0;
    Ok(Json(thread))
}

/// Mark my conversation read
///
/// As `POST /v1/sms/{provider}/threads/{phone}/read` on every provider of
/// the caller's (as `GET /v1/sms/mine/threads`).
#[crate::openapi::path(
    post,
    path = "/v1/sms/mine/threads/{phone}/read",
    operation_id = "mark_my_sms_thread_read",
    tag = "sms",
    params(("phone" = String, Path, description = "The other party's phone digits")),
    responses(
        (status = 204, description = "Marked (or not visible to the caller)"),
        (status = 400, description = "Not a phone number", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn mark_my_thread_read(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(phone): Path<String>,
) -> ApiResult<StatusCode> {
    mark_mine(&state, &user, &phone, false).await
}

/// Mark my conversation unread
///
/// As `POST /v1/sms/{provider}/threads/{phone}/unread` on every provider of
/// the caller's (as `GET /v1/sms/mine/threads`).
#[crate::openapi::path(
    post,
    path = "/v1/sms/mine/threads/{phone}/unread",
    operation_id = "mark_my_sms_thread_unread",
    tag = "sms",
    params(("phone" = String, Path, description = "The other party's phone digits")),
    responses(
        (status = 204, description = "Marked (or not visible to the caller)"),
        (status = 400, description = "Not a phone number", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn mark_my_thread_unread(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(phone): Path<String>,
) -> ApiResult<StatusCode> {
    mark_mine(&state, &user, &phone, true).await
}

async fn mark_mine(
    state: &AppState,
    user: &SessionUser,
    phone: &str,
    unread: bool,
) -> ApiResult<StatusCode> {
    checked_phone(phone)?;
    for provider in own_providers(&state.pool, user).await? {
        set_marker(state, user, provider, phone, unread).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// One phone's messages through `sources`, merged oldest first; never
/// sendable (the caller decides).
async fn merged_thread(
    state: &AppState,
    user: &SessionUser,
    phone: &str,
    query: &ThreadQuery,
    sources: &[(SmsProvider, Option<String>)],
    scope: SmsScope,
) -> ApiResult<Json<ThreadDetail>> {
    let pool = &state.pool;
    let phone = phone.to_string();
    let digits = checked_phone(&phone)?;
    if SAMPLE_PHONES[0] == phone {
        return Err(ApiError::NotFound);
    }
    let limit = query.limit.unwrap_or(30).clamp(1, 200);
    let page = ThreadQuery {
        limit: Some(limit),
        before_id: None,
        scope,
        agent_id: None,
    };
    let mut all = Vec::new();
    let mut has_older = false;
    let mut unread_count = 0;
    let mut seen = false;
    for (provider, agent) in sources {
        let provider = *provider;
        upkeep(state, provider, user.company_id).await?;
        let access = thread_access(pool, provider, user, scope, agent.as_deref(), &digits).await?;
        let Some(access) = access else {
            continue;
        };
        seen = true;
        let (rows, older) =
            messages(pool, provider, user.company_id, &digits, &access, &page).await?;
        has_older |= older;
        all.extend(rows);
        unread_count += thread_unread(pool, provider, user, &digits, &access).await?;
    }
    // A text copied when its rep changed provider shows once.
    let originals: HashSet<String> = all
        .iter()
        .filter(|message| message.copy_of.is_none())
        .map(|message| format!("{}:{}", message.provider.as_str(), message.id))
        .collect();
    let mut shown = HashSet::new();
    all.retain(|message| {
        message
            .copy_of
            .as_ref()
            .is_none_or(|original| !originals.contains(original) && shown.insert(original.clone()))
    });
    all.sort_by_key(|message| message.created_at);
    let keep = usize::try_from(limit).unwrap_or(0);
    if all.len() > keep {
        has_older = true;
        all.drain(..all.len() - keep);
    }
    let mut customer = None;
    for provider in SmsProvider::ALL {
        customer = customer_by_phone(pool, provider, user.company_id, &phone).await?;
        if customer.is_some() {
            break;
        }
    }
    let supplier_name = if customer.is_some() {
        None
    } else {
        suppliers_by_phone(pool, user.company_id, std::slice::from_ref(&digits))
            .await?
            .remove(&digits)
    };
    let active_flow = active_flow(pool, user, &digits, seen).await?;
    Ok(Json(ThreadDetail {
        phone_digits: phone,
        customer,
        supplier_name,
        messages: all,
        unread_count,
        can_send: false,
        has_older,
        active_flow,
        reply_provider: None,
    }))
}

/// List reps who text through any provider
///
/// The company's users with an agent or extension id on any provider, for
/// the admin inbox's rep filter; `agent_id` is the user's id.
#[crate::openapi::path(
    get,
    path = "/v1/sms/all/sales-reps",
    operation_id = "list_all_sms_sales_reps",
    tag = "sms",
    responses(
        (status = 200, description = "Reps by name", body = [SalesRep]),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn sales_reps(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
) -> ApiResult<Json<Vec<SalesRep>>> {
    let mut texts = SmsProvider::ALL
        .iter()
        .filter_map(|provider| provider.agent_column())
        .map(|column| format!("TRIM(IFNULL({column}, '')) != ''"))
        .collect::<Vec<_>>();
    // A Telnyx line.
    texts.push(
        "EXISTS (SELECT 1 FROM telnyx_number_users nu
                   JOIN telnyx_numbers n ON n.id = nu.number_id AND n.company_id = users.company_id
                  WHERE nu.user_id = users.id AND nu.can_call_from <> 0 AND n.active <> 0)"
            .to_string(),
    );
    let texts = texts.join(" OR ");
    let reps = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT CAST(id AS CHAR) AS agent_id, COALESCE(name, '') AS name FROM users
          WHERE company_id = ? AND is_deleted = 0 AND ({texts})
          ORDER BY name, id"
    )))
    .bind(user.company_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(reps))
}

/// Count my unread conversations over every provider
///
/// The caller's own conversations with unread messages through every
/// provider they text through (as `GET /v1/sms/mine/threads`), for the
/// sidebar badge. A phone with unread texts through two providers counts
/// twice.
#[crate::openapi::path(
    get,
    path = "/v1/sms/mine/unread-count",
    operation_id = "count_my_unread_sms_threads",
    tag = "sms",
    responses(
        (status = 200, description = "The count", body = UnreadCount),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn my_unread_count(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<UnreadCount>> {
    let mut count = 0;
    for provider in own_providers(&state.pool, &user).await? {
        count += unread_threads(&state.pool, provider, &user, SmsScope::Mine, None).await?;
    }
    Ok(Json(UnreadCount { count }))
}

/// `GET /v1/sms/mine/desktop-alerts`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct AlertsQuery {
    /// The SMS page the alerts open (the caller's inbox); default `telnyx`.
    #[param(inline)]
    pub inbox: Option<SmsProvider>,
}

/// My unread texts to alert about, over every provider
///
/// As `GET /v1/sms/{provider}/desktop-alerts` for every provider the caller
/// texts through: one alert per phone (the latest), newest first, at most
/// 50, each opening the conversation on the caller's `inbox` page.
#[crate::openapi::path(
    get,
    path = "/v1/sms/mine/desktop-alerts",
    operation_id = "list_my_sms_desktop_alerts",
    tag = "sms",
    params(AlertsQuery),
    responses(
        (status = 200, description = "Alerts", body = DesktopAlerts),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn my_desktop_alerts(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    MultiQuery(query): MultiQuery<AlertsQuery>,
) -> ApiResult<Json<DesktopAlerts>> {
    let inbox = query.inbox.unwrap_or(SmsProvider::Telnyx);
    let mut by_phone: HashMap<String, DesktopAlert> = HashMap::new();
    for provider in own_providers(&state.pool, &user).await? {
        for alert in provider_alerts(&state.pool, provider, &user, inbox).await? {
            let key = phone10(&alert.phone_digits);
            match by_phone.get(&key) {
                Some(seen) if seen.sent_at >= alert.sent_at => {}
                _ => {
                    by_phone.insert(key, alert);
                }
            }
        }
    }
    let mut alerts: Vec<DesktopAlert> = by_phone.into_values().collect();
    alerts.sort_by(|a, b| b.sent_at.cmp(&a.sent_at));
    alerts.truncate(50);
    Ok(Json(DesktopAlerts { alerts }))
}
