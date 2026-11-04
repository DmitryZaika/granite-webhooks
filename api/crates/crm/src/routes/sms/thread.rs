//! One conversation, and its read marker.

use super::provider::{
    COPY_KEY, Param, SAMPLE_PHONES, SmsProvider, fetch_all, phone_variants, valid_thread_phone,
};
use super::rollup::{MANUAL_UNREAD_AT, manual_unread};
use super::schemas::{
    ActiveFlow, Direction, Message, MessageStatus, ThreadCustomer, ThreadDetail, ThreadQuery,
};
use super::scope::{SmsScope, ThreadAccess, filter_agent, thread_access};
use super::threads::{attachments, suppliers_by_phone, upkeep};
use crate::auth::EmployeeUser;
use crate::auth::session::SessionUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::MultiQuery;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, NaiveDateTime, Utc};
use sqlx::MySqlPool;
use std::collections::HashMap;

/// A flow step that asks the AI to write the follow-up.
const AI_STEP: &str = "__GR_AI_FOLLOWUP__";

#[derive(sqlx::FromRow)]
struct MessageRow {
    id: i32,
    sender: Option<String>,
    recipient: String,
    text: String,
    status: String,
    agent: Option<String>,
    conversation_at: DateTime<Utc>,
    /// The original a copy names (`moves`).
    #[sqlx(default)]
    copy_of: Option<String>,
}

/// The thread's rows (`s`) visible to the caller, before `before_id`.
fn thread_where(access: &ThreadAccess, before_id: Option<i64>) -> String {
    let mut sql = "s.company_id = ? AND s.phone_digits = ? AND s.is_echo = 0".to_string();
    if let Some((clause, _)) = &access.clause {
        sql.push_str(" AND ");
        sql.push_str(clause);
    }
    if before_id.is_some() {
        sql.push_str(" AND s.id < ?");
    }
    sql
}

fn thread_params(
    company_id: i32,
    phone: &str,
    access: &ThreadAccess,
    before_id: Option<i64>,
) -> Vec<Param> {
    let mut params: Vec<Param> = vec![company_id.into(), phone.into()];
    if let Some((_, values)) = &access.clause {
        params.extend(values.iter().cloned());
    }
    if let Some(id) = before_id {
        params.push(id.into());
    }
    params
}

/// Provider-id range holding the thread's rows and their outer neighbours,
/// for the conversation-time window; `None` without provider ids.
async fn conversation_window(
    pool: &MySqlPool,
    provider: SmsProvider,
    company_id: i32,
    filter: &str,
    params: &[Param],
) -> Result<Option<(i64, i64)>, sqlx::Error> {
    let (table, id) = (provider.sms_table(), provider.id_column());
    let range: Vec<(Option<i64>, Option<i64>)> = fetch_all(
        pool,
        format!(
            "SELECT MIN(s.{id}), MAX(s.{id}) FROM {table} s WHERE {filter} AND s.{id} IS NOT NULL"
        ),
        params,
    )
    .await?;
    let Some((Some(low), Some(high))) = range.first().copied() else {
        return Ok(None);
    };
    let (below, above): (Option<i64>, Option<i64>) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT (SELECT MAX({id}) FROM {table} WHERE company_id = ? AND {id} < ?),
                (SELECT MIN({id}) FROM {table} WHERE company_id = ? AND {id} > ?)"
    )))
    .bind(company_id)
    .bind(low)
    .bind(company_id)
    .bind(high)
    .fetch_one(pool)
    .await?;
    Ok(Some((below.unwrap_or(low), above.unwrap_or(high))))
}

/// The newest `limit + 1` visible rows, newest first. `CloudTalk` places a
/// late webhook insert between its provider-id neighbours (LAG/LEAD over
/// the company's rows in the thread's id range).
async fn message_rows(
    pool: &MySqlPool,
    provider: SmsProvider,
    company_id: i32,
    filter: &str,
    mut params: Vec<Param>,
    limit: i64,
) -> Result<Vec<MessageRow>, sqlx::Error> {
    let (table, id) = (provider.sms_table(), provider.id_column());
    let columns = "s.id, CAST(s.sender AS CHAR) AS sender, CAST(s.recipient AS CHAR) AS recipient,
                   s.text, CAST(s.status AS CHAR) AS status, s.agent";
    let window = if provider.has_conversation_time() {
        conversation_window(pool, provider, company_id, filter, &params).await?
    } else {
        None
    };
    let sql = if let Some((low, high)) = window {
        params.extend([company_id.into(), low.into(), high.into()]);
        format!(
            "SELECT t.id, t.sender, t.recipient, t.text, t.status, t.agent,
                    COALESCE(adj.conversation_at, t.created_date) AS conversation_at
               FROM (SELECT {columns}, s.{id} AS provider_id, s.created_date
                       FROM {table} s WHERE {filter}) t
               LEFT JOIN (
                 SELECT id,
                   CASE WHEN next_date >= prev_date AND next_id > prev_id AND created_date > next_date
                        THEN DATE_ADD(prev_date, INTERVAL GREATEST(0, FLOOR(
                               TIMESTAMPDIFF(SECOND, prev_date, next_date)
                               * ({id} - prev_id) / (next_id - prev_id))) SECOND)
                   END AS conversation_at
                 FROM (
                   SELECT id, {id}, created_date,
                     LAG({id}) OVER by_id AS prev_id,
                     LAG(created_date) OVER by_id AS prev_date,
                     LEAD({id}) OVER by_id AS next_id,
                     LEAD(created_date) OVER by_id AS next_date
                   FROM {table}
                   WHERE company_id = ? AND {id} BETWEEN ? AND ?
                   WINDOW by_id AS (ORDER BY {id})
                 ) w
                 WHERE prev_id IS NOT NULL AND next_id IS NOT NULL
               ) adj ON adj.id = t.id
              ORDER BY conversation_at DESC, t.provider_id DESC, t.id DESC
              LIMIT ?"
        )
    } else {
        let order = provider.order_column();
        let copy_of = format!(
            ", IF(COALESCE(s.idempotency_key, '') REGEXP '{COPY_KEY}', s.idempotency_key, NULL)
                    AS copy_of"
        );
        format!(
            "SELECT {columns}, s.created_date AS conversation_at{copy_of} FROM {table} s
              WHERE {filter}
              ORDER BY s.created_date DESC, s.{order} DESC, s.id DESC
              LIMIT ?"
        )
    };
    params.push((limit + 1).into());
    fetch_all(pool, sql, &params).await
}

/// Agent names by agent id, and agents by their own number.
async fn agent_maps(
    pool: &MySqlPool,
    provider: SmsProvider,
    company_id: i32,
) -> Result<(HashMap<String, String>, HashMap<String, (String, String)>), sqlx::Error> {
    let (Some(agent), Some(phone)) = (provider.agent_column(), provider.phone_column()) else {
        // Telnyx: users by id, and the users of direct lines.
        return Ok((
            crate::phone::texting::user_names(pool, company_id).await?,
            crate::phone::texting::direct_line_users(pool, company_id).await?,
        ));
    };
    let names: Vec<(String, Option<String>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT TRIM({agent}), MIN(name) FROM users
          WHERE company_id = ? AND is_deleted = 0 AND {agent} IS NOT NULL AND TRIM({agent}) != ''
          GROUP BY TRIM({agent})"
    )))
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    let numbers: Vec<(String, Option<String>, String)> =
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT TRIM({agent}), MIN(name), {phone} FROM users
          WHERE company_id = ? AND is_deleted = 0 AND {agent} IS NOT NULL AND TRIM({agent}) != ''
            AND {phone} IS NOT NULL AND TRIM({phone}) != ''
          GROUP BY TRIM({agent}), {phone}"
        )))
        .bind(company_id)
        .fetch_all(pool)
        .await?;
    let names = names
        .into_iter()
        .map(|(id, name)| (id, name.unwrap_or_default()))
        .collect();
    let mut by_number = HashMap::new();
    for (id, name, number) in numbers {
        let number = super::provider::phone10(&number);
        if !number.is_empty() {
            by_number
                .entry(number)
                .or_insert_with(|| (id, name.unwrap_or_default()));
        }
    }
    Ok((names, by_number))
}

/// The customer with this phone: through the provider's contacts, else the
/// customer record itself.
pub async fn customer_by_phone(
    pool: &MySqlPool,
    provider: SmsProvider,
    company_id: i32,
    phone: &str,
) -> Result<Option<ThreadCustomer>, sqlx::Error> {
    if let Some(contacts) = provider.contacts_table() {
        let linked: Option<ThreadCustomer> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT c.id, c.name, u.name AS sales_rep_name
           FROM customers c
           JOIN {contacts} cc ON cc.customer_id = c.id
           LEFT JOIN users u ON u.id = c.sales_rep AND u.is_deleted = 0
          WHERE cc.company_id = ? AND c.deleted_at IS NULL
            AND (RIGHT(cc.phone_e164_1, 10) = RIGHT(?, 10) OR RIGHT(cc.phone_e164_2, 10) = RIGHT(?, 10))
          LIMIT 1"
    )))
    .bind(company_id)
    .bind(phone)
    .bind(phone)
    .fetch_optional(pool)
    .await?;
        if linked.is_some() {
            return Ok(linked);
        }
    }
    sqlx::query_as(
        "SELECT c.id, c.name, u.name AS sales_rep_name
           FROM customers c
           LEFT JOIN users u ON u.id = c.sales_rep AND u.is_deleted = 0
          WHERE c.company_id = ? AND c.deleted_at IS NULL
            AND (RIGHT(REGEXP_REPLACE(COALESCE(c.phone, ''), '[^0-9]', ''), 10) = RIGHT(?, 10)
              OR RIGHT(REGEXP_REPLACE(COALESCE(c.phone_2, ''), '[^0-9]', ''), 10) = RIGHT(?, 10))
          LIMIT 1",
    )
    .bind(company_id)
    .bind(phone)
    .bind(phone)
    .fetch_optional(pool)
    .await
}

/// Unread inbound messages of one thread for the caller.
pub(super) async fn thread_unread(
    pool: &MySqlPool,
    provider: SmsProvider,
    user: &SessionUser,
    phone: &str,
    access: &ThreadAccess,
) -> Result<i64, sqlx::Error> {
    let (sms, reads) = (provider.sms_table(), provider.reads_table());
    let extra = access
        .clause
        .as_ref()
        .map(|(clause, _)| format!(" AND {clause}"))
        .unwrap_or_default();
    let mut params: Vec<Param> = vec![
        user.id.into(),
        phone.into(),
        user.company_id.into(),
        phone.into(),
    ];
    if let Some((_, values)) = &access.clause {
        params.extend(values.iter().cloned());
    }
    params.push(phone.into());
    let manual = manual_unread("r.last_read_at");
    let rows: Vec<(i64,)> = fetch_all(
        pool,
        format!(
            "SELECT COUNT(*) FROM {sms} s
               LEFT JOIN {reads} r
                 ON r.user_id = ? AND r.company_id = s.company_id AND r.customer_phone_digits = ?
              WHERE s.company_id = ? AND s.phone_digits = ? AND s.is_echo = 0
                AND s.direction = 'inbound' AND s.sender10 = s.phone_digits{extra}
                AND ({manual}
                  OR ((r.last_read_at IS NULL OR s.created_date > r.last_read_at)
                      AND s.created_date > COALESCE(
                        (SELECT MAX(response.created_date) FROM {sms} response
                          WHERE response.company_id = s.company_id AND response.phone_digits = ?
                            AND response.is_echo = 0
                            AND ((response.direction = 'outbound' AND response.status IN ('sent', 'pending'))
                              OR (response.direction = 'inbound' AND response.agent IS NOT NULL
                                AND response.sender10 <> response.phone_digits))),
                        '1970-01-01 00:00:00')))"
        ),
        &params,
    )
    .await?;
    Ok(rows.first().map_or(0, |row| row.0))
}

#[derive(sqlx::FromRow)]
struct FlowRow {
    enrollment_id: i32,
    flow_id: i32,
    flow_name: String,
    status: String,
    next_step_position: i32,
    next_step_name: Option<String>,
    next_step_custom_text: Option<String>,
    next_step_template_body: Option<String>,
    next_send_at: Option<NaiveDateTime>,
    error_message: Option<String>,
    enrollment_user_id: i32,
    sales_rep_id: Option<i32>,
    total_steps: i64,
}

fn step_preview(custom: Option<&str>, template: Option<&str>) -> Option<String> {
    let text = match custom {
        Some(custom) if custom != AI_STEP => custom,
        _ => template?,
    };
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// The flow running (or failed) on the phone, if the caller may see it:
/// admins, viewers of the thread, the rep who started it, or the customer's
/// rep.
pub async fn active_flow(
    pool: &MySqlPool,
    user: &SessionUser,
    phone: &str,
    sees_thread: bool,
) -> Result<Option<ActiveFlow>, sqlx::Error> {
    let row: Option<FlowRow> = sqlx::query_as(
        "SELECT e.id AS enrollment_id, e.flow_id, f.name AS flow_name, CAST(e.status AS CHAR) AS status,
                e.current_step_position AS next_step_position,
                COALESCE(t.name, rt.name, et.template_name) AS next_step_name,
                st.custom_text AS next_step_custom_text,
                COALESCE(t.body, rt.body, et.template_body) AS next_step_template_body,
                e.next_send_at, e.error_message,
                e.user_id AS enrollment_user_id, cu.sales_rep AS sales_rep_id,
                (SELECT COUNT(*) FROM sms_flow_steps s2 WHERE s2.flow_id = e.flow_id) AS total_steps
           FROM sms_flow_enrollments e
           JOIN sms_flows f ON f.id = e.flow_id
           LEFT JOIN sms_flow_steps st ON st.flow_id = e.flow_id AND st.position = e.current_step_position
           LEFT JOIN cloudtalk_sms_templates t
             ON t.id = st.template_id AND t.company_id = e.company_id AND COALESCE(st.channel, 'sms') = 'sms'
           LEFT JOIN ringcentral_sms_templates rt
             ON rt.id = st.template_id AND rt.company_id = e.company_id AND COALESCE(st.channel, 'sms') = 'sms'
           LEFT JOIN email_templates et
             ON et.id = st.template_id AND et.company_id = e.company_id
            AND COALESCE(st.channel, 'sms') = 'email' AND et.deleted_at IS NULL
           LEFT JOIN customers cu ON cu.id = e.customer_id AND cu.company_id = e.company_id
          WHERE e.company_id = ? AND e.customer_phone_digits = ?
            AND e.status IN ('active', 'paused', 'failed')
          ORDER BY CASE WHEN e.status IN ('active', 'paused') THEN 0 ELSE 1 END, e.updated_at DESC
          LIMIT 1",
    )
    .bind(user.company_id)
    .bind(phone)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let visible = user.is_admin
        || user.is_superuser
        || sees_thread
        || row.enrollment_user_id == user.id
        || row.sales_rep_id == Some(user.id);
    Ok(visible.then(|| ActiveFlow {
        next_step_preview: step_preview(
            row.next_step_custom_text.as_deref(),
            row.next_step_template_body.as_deref(),
        ),
        enrollment_id: row.enrollment_id,
        flow_id: row.flow_id,
        flow_name: row.flow_name,
        status: row.status,
        next_step_position: row.next_step_position,
        next_step_name: row.next_step_name,
        next_send_at: row.next_send_at.map(|at| at.and_utc()),
        total_steps: row.total_steps,
        error_message: row.error_message,
    }))
}

const fn status_of(stored: &str) -> MessageStatus {
    match stored.as_bytes() {
        b"pending" => MessageStatus::Sending,
        b"failed" => MessageStatus::Failed,
        _ => MessageStatus::Sent,
    }
}

/// Visible messages, oldest first, and whether older ones exist.
pub(super) async fn messages(
    pool: &MySqlPool,
    provider: SmsProvider,
    company_id: i32,
    phone: &str,
    access: &ThreadAccess,
    query: &ThreadQuery,
) -> Result<(Vec<Message>, bool), sqlx::Error> {
    let limit = query.limit.unwrap_or(30).clamp(1, 200);
    let before_id = query.before_id.filter(|id| *id > 0);
    let filter = thread_where(access, before_id);
    let params = thread_params(company_id, phone, access, before_id);
    let mut rows = message_rows(pool, provider, company_id, &filter, params, limit).await?;
    let has_older = i64::try_from(rows.len()).unwrap_or(0) > limit;
    rows.truncate(usize::try_from(limit).unwrap_or(0));
    rows.reverse();
    let messages = to_messages(pool, provider, company_id, phone, rows).await?;
    Ok((messages, has_older))
}

/// Rows of one thread as messages: direction seen from the customer's phone,
/// the rep who sent (or whose number received) each, and the files.
async fn to_messages(
    pool: &MySqlPool,
    provider: SmsProvider,
    company_id: i32,
    phone: &str,
    rows: Vec<MessageRow>,
) -> Result<Vec<Message>, sqlx::Error> {
    let ids: Vec<i32> = rows.iter().map(|row| row.id).collect();
    let mut files = attachments(pool, provider, &ids).await?;
    let (names, numbers) = if rows.is_empty() {
        (HashMap::new(), HashMap::new())
    } else {
        agent_maps(pool, provider, company_id).await?
    };
    let variants = phone_variants(phone);
    Ok(rows
        .into_iter()
        .map(|row| {
            let inbound = row.sender.as_ref().is_some_and(|s| variants.contains(s));
            let (agent_id, agent_name) = if inbound {
                numbers
                    .get(&super::provider::phone10(&row.recipient))
                    .map_or((None, None), |(id, name)| {
                        (Some(id.clone()), Some(name.clone()))
                    })
            } else {
                let id = row
                    .agent
                    .as_deref()
                    .map(str::trim)
                    .filter(|agent| !agent.is_empty())
                    .map(str::to_string);
                let name = id.as_ref().and_then(|id| names.get(id).cloned());
                (id, name)
            };
            Message {
                provider,
                attachments: files.remove(&row.id).unwrap_or_default(),
                id: row.id,
                direction: if inbound {
                    Direction::Inbound
                } else {
                    Direction::Outbound
                },
                text: row.text,
                agent: row.agent,
                copy_of: row.copy_of,
                agent_id,
                agent_name,
                created_at: row.conversation_at,
                status: status_of(&row.status),
            }
        })
        .collect())
}

/// One message of the company's, by id.
pub async fn message_by_id(
    pool: &MySqlPool,
    provider: SmsProvider,
    company_id: i32,
    phone: &str,
    id: i64,
) -> Result<Option<Message>, sqlx::Error> {
    let rows: Vec<MessageRow> = fetch_all(
        pool,
        format!(
            "SELECT s.id, CAST(s.sender AS CHAR) AS sender, CAST(s.recipient AS CHAR) AS recipient,
                    s.text, CAST(s.status AS CHAR) AS status, s.agent, s.created_date AS conversation_at
               FROM {} s WHERE s.company_id = ? AND s.id = ?",
            provider.sms_table()
        ),
        &[company_id.into(), id.into()],
    )
    .await?;
    Ok(to_messages(pool, provider, company_id, phone, rows)
        .await?
        .into_iter()
        .next())
}

pub(super) fn checked_phone(phone: &str) -> ApiResult<String> {
    if !valid_thread_phone(phone) {
        return Err(ApiError::BadRequest("invalid_phone".into()));
    }
    Ok(super::provider::phone10(phone))
}

/// Get a conversation
///
/// The newest messages with one phone (page back with `before_id`), the
/// customer or supplier it belongs to and the SMS flow running on it. A
/// conversation the caller may not see comes back without messages. The
/// onboarding tour's sample phone is not a real conversation (404).
#[crate::openapi::path(
    get,
    path = "/v1/sms/{provider}/threads/{phone}",
    operation_id = "get_sms_thread",
    tag = "sms",
    params(
        ("provider" = SmsProvider, Path, description = "SMS provider"),
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
pub async fn get_thread(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((provider, phone)): Path<(SmsProvider, String)>,
    MultiQuery(query): MultiQuery<ThreadQuery>,
) -> ApiResult<Json<ThreadDetail>> {
    let pool = &state.pool;
    let digits = checked_phone(&phone)?;
    if SAMPLE_PHONES[0] == phone {
        return Err(ApiError::NotFound);
    }
    upkeep(&state, provider, user.company_id).await?;
    let scope = query.scope.for_user(&user);
    let filter = filter_agent(pool, provider, &user, scope, query.agent_id.as_deref()).await?;
    let access = thread_access(pool, provider, &user, scope, filter.as_deref(), &digits).await?;
    let (messages, has_older) = match &access {
        Some(access) => messages(pool, provider, user.company_id, &digits, access, &query).await?,
        None => (Vec::new(), false),
    };
    let customer = customer_by_phone(pool, provider, user.company_id, &phone).await?;
    let supplier_name = if customer.is_some() {
        None
    } else {
        suppliers_by_phone(pool, user.company_id, std::slice::from_ref(&digits))
            .await?
            .remove(&digits)
    };
    // The caller's own inbox marks the thread read on open.
    let unread_count = match (&access, query.scope) {
        (Some(access), SmsScope::All) => {
            thread_unread(pool, provider, &user, &digits, access).await?
        }
        _ => 0,
    };
    let active_flow = active_flow(pool, &user, &digits, access.is_some()).await?;
    let can_send = super::send::seat(pool, provider, &user).await?.is_some();
    Ok(Json(ThreadDetail {
        phone_digits: phone,
        customer,
        supplier_name,
        messages,
        unread_count,
        can_send,
        has_older,
        active_flow,
        reply_provider: None,
    }))
}

/// Sets the caller's read marker on a thread they can see; silently nothing
/// otherwise, so the answer never tells which phones exist.
pub(super) async fn set_marker(
    state: &AppState,
    user: &SessionUser,
    provider: SmsProvider,
    phone: &str,
    unread: bool,
) -> ApiResult<StatusCode> {
    let pool = &state.pool;
    let digits = checked_phone(phone)?;
    let scope = SmsScope::All.for_user(user);
    if thread_access(pool, provider, user, scope, None, &digits)
        .await?
        .is_none()
    {
        return Ok(StatusCode::NO_CONTENT);
    }
    let reads = provider.reads_table();
    let sql = if unread {
        format!(
            "INSERT INTO {reads} (user_id, company_id, customer_phone_digits, last_read_at)
             VALUES (?, ?, ?, '{MANUAL_UNREAD_AT}')
             ON DUPLICATE KEY UPDATE last_read_at = VALUES(last_read_at)"
        )
    } else {
        format!(
            "INSERT INTO {reads} (user_id, company_id, customer_phone_digits, last_read_at)
             VALUES (?, ?, ?, NOW())
             ON DUPLICATE KEY UPDATE last_read_at = GREATEST(last_read_at, NOW())"
        )
    };
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(user.id)
        .bind(user.company_id)
        .bind(&digits)
        .execute(pool)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Mark a conversation read
///
/// Clears the conversation's unread messages for the caller.
#[crate::openapi::path(
    post,
    path = "/v1/sms/{provider}/threads/{phone}/read",
    operation_id = "mark_sms_thread_read",
    tag = "sms",
    params(
        ("provider" = SmsProvider, Path, description = "SMS provider"),
        ("phone" = String, Path, description = "The other party's phone digits"),
    ),
    responses(
        (status = 204, description = "Marked (or not visible to the caller)"),
        (status = 400, description = "Not a phone number", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn mark_read(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((provider, phone)): Path<(SmsProvider, String)>,
) -> ApiResult<StatusCode> {
    set_marker(&state, &user, provider, &phone, false).await
}

/// Mark a conversation unread
///
/// Keeps the conversation unread for the caller until it is opened again.
#[crate::openapi::path(
    post,
    path = "/v1/sms/{provider}/threads/{phone}/unread",
    operation_id = "mark_sms_thread_unread",
    tag = "sms",
    params(
        ("provider" = SmsProvider, Path, description = "SMS provider"),
        ("phone" = String, Path, description = "The other party's phone digits"),
    ),
    responses(
        (status = 204, description = "Marked (or not visible to the caller)"),
        (status = 400, description = "Not a phone number", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn mark_unread(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((provider, phone)): Path<(SmsProvider, String)>,
) -> ApiResult<StatusCode> {
    set_marker(&state, &user, provider, &phone, true).await
}
