//! The inbox, its badge, desktop alerts and the rep filter.

use super::provider::{Param, SmsProvider, fetch_all, marks, phone_variants};
use super::rollup::{Mode, rollup_sql, search};
use super::schemas::{
    Attachment, BadgeQuery, DesktopAlert, DesktopAlerts, Direction, SalesRep, ThreadList,
    ThreadSummary, ThreadsQuery, UnreadCount,
};
use super::scope::{SmsScope, filter_agent, narrowing};
use crate::auth::session::SessionUser;
use crate::auth::{AdminUser, EmployeeUser};
use crate::error::{ApiResult, ErrorBody};
use crate::extract::MultiQuery;
use crate::routes::company::queries::company_features;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use chrono::{DateTime, Utc};
use sqlx::MySqlPool;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

const UPKEEP_EVERY: Duration = Duration::from_secs(60);

/// Records a run now unless one ran within [`UPKEEP_EVERY`].
fn upkeep_due(state: &AppState, provider: SmsProvider, company_id: i32) -> bool {
    let mut map = state
        .sms_upkeep
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = Instant::now();
    map.retain(|_, at| now.duration_since(*at) < UPKEEP_EVERY * 10);
    let key = (provider.as_str(), company_id);
    let due = map
        .get(&key)
        .is_none_or(|at| now.duration_since(*at) >= UPKEEP_EVERY);
    if due {
        map.insert(key, now);
    }
    drop(map);
    due
}

/// [`common::sms::maintenance::run`] (Telnyx: failing sends that never
/// finished), at most once a minute per company.
pub async fn upkeep(
    state: &AppState,
    provider: SmsProvider,
    company_id: i32,
) -> Result<(), sqlx::Error> {
    if upkeep_due(state, provider, company_id) {
        match provider.shared() {
            Some(shared) => common::sms::maintenance::run(&state.pool, shared, company_id).await?,
            None => {
                crate::phone::texting::fail_stale_pending(&state.pool, company_id).await?;
            }
        }
    }
    Ok(())
}

#[derive(sqlx::FromRow)]
pub(super) struct RollupRow {
    pub phone_digits: String,
    pub last_id: i64,
    pub last_at: DateTime<Utc>,
    pub last_inbound_at: Option<DateTime<Utc>>,
    pub message_count: i64,
    pub unread: i64,
    pub total_count: i64,
    pub total_unread: i64,
}

#[derive(sqlx::FromRow)]
pub(super) struct LastRow {
    id: i32,
    pub text: String,
    pub sender: Option<String>,
    pub agent: Option<String>,
}

/// "📷 Photo", "📷 7 photos", "📎 File", "📷 2 photos, 📎 1 file".
pub fn attachment_label(attachments: &[Attachment]) -> String {
    let photos = attachments
        .iter()
        .filter(|a| a.content_type.starts_with("image/"))
        .count();
    let files = attachments.len() - photos;
    let mut parts = Vec::new();
    match photos {
        0 => {}
        1 => parts.push("📷 Photo".to_string()),
        n => parts.push(format!("📷 {n} photos")),
    }
    if files > 0 {
        let noun = if files == 1 { "file" } else { "files" };
        parts.push(match (photos, files) {
            (0, 1) => "📎 File".to_string(),
            (0, n) => format!("📎 {n} files"),
            (_, n) => format!("📎 {n} {noun}"),
        });
    }
    parts.join(", ")
}

/// Attachments of the given rows, in order.
pub async fn attachments(
    pool: &MySqlPool,
    provider: SmsProvider,
    ids: &[i32],
) -> Result<HashMap<i32, Vec<Attachment>>, sqlx::Error> {
    let mut out: HashMap<i32, Vec<Attachment>> = HashMap::new();
    if ids.is_empty() {
        return Ok(out);
    }
    let (table, column) = provider.attachments();
    let params: Vec<Param> = ids.iter().map(|id| Param::from(*id)).collect();
    let rows: Vec<Attachment> = fetch_all(
        pool,
        format!(
            "SELECT id, {column} AS sms_id, s3_url AS url, content_type,
                    COALESCE(NULLIF(filename, ''), 'file') AS filename, width, height
               FROM {table} WHERE {column} IN ({}) ORDER BY {column}, position, id",
            marks(ids.len())
        ),
        &params,
    )
    .await?;
    for row in rows {
        out.entry(row.sms_id).or_default().push(row);
    }
    Ok(out)
}

/// Text, sender and agent of the rows; a picture-only message previews what
/// was sent.
pub(super) async fn last_rows(
    pool: &MySqlPool,
    provider: SmsProvider,
    company_id: i32,
    ids: &[i64],
) -> Result<HashMap<i64, LastRow>, sqlx::Error> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let mut params: Vec<Param> = vec![company_id.into()];
    params.extend(ids.iter().map(|id| Param::from(*id)));
    let rows: Vec<LastRow> = fetch_all(
        pool,
        format!(
            "SELECT s.id, s.text, CAST(s.sender AS CHAR) AS sender, s.agent FROM {} s
              WHERE s.company_id = ? AND s.id IN ({})",
            provider.sms_table(),
            marks(ids.len())
        ),
        &params,
    )
    .await?;
    let textless: Vec<i32> = rows
        .iter()
        .filter(|row| row.text.trim().is_empty())
        .map(|row| row.id)
        .collect();
    let files = attachments(pool, provider, &textless).await?;
    Ok(rows
        .into_iter()
        .map(|mut row| {
            if let Some(list) = files.get(&row.id) {
                let label = attachment_label(list);
                if !label.is_empty() {
                    row.text = label;
                }
            }
            (i64::from(row.id), row)
        })
        .collect())
}

/// Customers by thread phone, through the provider's contacts.
pub async fn customers_by_phone(
    pool: &MySqlPool,
    provider: SmsProvider,
    company_id: i32,
    phones: &[String],
) -> Result<HashMap<String, (i32, Option<String>)>, sqlx::Error> {
    let mut out = HashMap::new();
    if phones.is_empty() {
        return Ok(out);
    }
    if let Some(contacts) = provider.contacts_table() {
        let list = marks(phones.len());
        let branch = |column: &str| {
            format!(
                "SELECT RIGHT(ct.{column}, 10) AS phone_digits, c.id, c.name
                   FROM {contacts} ct
                   JOIN customers c ON c.id = ct.customer_id AND c.company_id = ? AND c.deleted_at IS NULL
                  WHERE ct.company_id = ? AND RIGHT(ct.{column}, 10) IN ({list})"
            )
        };
        let mut params: Vec<Param> = Vec::new();
        for _ in 0..2 {
            params.push(company_id.into());
            params.push(company_id.into());
            params.extend(phones.iter().map(Param::from));
        }
        let rows: Vec<(String, i32, Option<String>)> = fetch_all(
            pool,
            format!(
                "{} UNION ALL {}",
                branch("phone_e164_1"),
                branch("phone_e164_2")
            ),
            &params,
        )
        .await?;
        for (phone, id, name) in rows {
            out.entry(phone).or_insert((id, name));
        }
    }
    // No provider contact: fall back to the customer record itself, as the
    // thread header does.
    let missing: Vec<&String> = phones.iter().filter(|p| !out.contains_key(*p)).collect();
    if missing.is_empty() {
        return Ok(out);
    }
    let list = marks(missing.len());
    let branch = |column: &str| {
        format!(
            "SELECT RIGHT(REGEXP_REPLACE(COALESCE(c.{column}, ''), '[^0-9]', ''), 10) AS phone_digits,
                    c.id, c.name
               FROM customers c
              WHERE c.company_id = ? AND c.deleted_at IS NULL
                AND RIGHT(REGEXP_REPLACE(COALESCE(c.{column}, ''), '[^0-9]', ''), 10) IN ({list})"
        )
    };
    let mut params: Vec<Param> = Vec::new();
    for _ in 0..2 {
        params.push(company_id.into());
        params.extend(missing.iter().map(|p| Param::from(*p)));
    }
    let rows: Vec<(String, i32, Option<String>)> = fetch_all(
        pool,
        format!(
            "{} UNION ALL {} ORDER BY id",
            branch("phone"),
            branch("phone_2")
        ),
        &params,
    )
    .await?;
    for (phone, id, name) in rows {
        out.entry(phone).or_insert((id, name));
    }
    Ok(out)
}

/// Supplier names by phone (ten digits).
pub async fn suppliers_by_phone(
    pool: &MySqlPool,
    company_id: i32,
    phones: &[String],
) -> Result<HashMap<String, String>, sqlx::Error> {
    let keys: Vec<&String> = phones
        .iter()
        .filter(|phone| phone.len() == 10)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let mut out = HashMap::new();
    if keys.is_empty() {
        return Ok(out);
    }
    let mut params: Vec<Param> = vec![company_id.into()];
    params.extend(keys.iter().map(|phone| Param::from(*phone)));
    let rows: Vec<(Option<String>, Option<String>)> = fetch_all(
        pool,
        format!(
            "SELECT supplier_name, phone FROM suppliers
              WHERE company_id = ? AND {} IN ({})",
            super::rollup::SUPPLIER_PHONE10,
            marks(keys.len())
        ),
        &params,
    )
    .await?;
    for (name, phone) in rows {
        let digits = super::provider::phone10(phone.as_deref().unwrap_or_default());
        let name = name.unwrap_or_default().trim().to_string();
        if digits.len() == 10 && !name.is_empty() {
            out.entry(digits).or_insert(name);
        }
    }
    Ok(out)
}

/// Rows of the rollup query in `mode`, or none when nothing is visible.
pub(super) async fn rollup<T>(
    pool: &MySqlPool,
    provider: SmsProvider,
    user: &SessionUser,
    scope: SmsScope,
    filter: Option<&str>,
    term: &str,
    mode: Mode,
) -> Result<Vec<T>, sqlx::Error>
where
    T: for<'r> sqlx::FromRow<'r, sqlx::mysql::MySqlRow> + Send + Unpin,
{
    let narrowed = narrowing(pool, provider, user, scope, filter).await?;
    if narrowed.is_empty() {
        return Ok(Vec::new());
    }
    let search = search(pool, user.company_id, term).await?;
    let (sql, params) = rollup_sql(provider, &narrowed, user, &search, mode);
    fetch_all(pool, sql, &params).await
}

pub(super) async fn unread_threads(
    pool: &MySqlPool,
    provider: SmsProvider,
    user: &SessionUser,
    scope: SmsScope,
    filter: Option<&str>,
) -> Result<i64, sqlx::Error> {
    let rows: Vec<(i64,)> = rollup(pool, provider, user, scope, filter, "", Mode::Count).await?;
    Ok(rows.first().map_or(0, |row| row.0))
}

/// List conversations
///
/// A page of the caller's SMS conversations (`scope=all`: the company's, for
/// admins), latest first — or unread first when narrowed to one rep. A
/// search matches phone digits, message text and customer or supplier names.
#[crate::openapi::path(
    get,
    path = "/v1/sms/{provider}/threads",
    operation_id = "list_sms_threads",
    tag = "sms",
    params(("provider" = SmsProvider, Path, description = "SMS provider"), ThreadsQuery),
    responses(
        (status = 200, description = "A page of conversations", body = ThreadList),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list_threads(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(provider): Path<SmsProvider>,
    MultiQuery(query): MultiQuery<ThreadsQuery>,
) -> ApiResult<Json<ThreadList>> {
    let pool = &state.pool;
    let term: String = query.search.chars().take(100).collect();
    let limit = query.limit.unwrap_or(20).clamp(1, 100);
    let offset = query.offset.unwrap_or(0).max(0);
    let scope = query.scope.for_user(&user);
    let filter = filter_agent(pool, provider, &user, scope, query.agent_id.as_deref()).await?;
    upkeep(&state, provider, user.company_id).await?;

    let rows: Vec<RollupRow> = rollup(
        pool,
        provider,
        &user,
        scope,
        filter.as_deref(),
        &term,
        Mode::List { limit, offset },
    )
    .await?;
    let total_count = rows.first().map_or(0, |row| row.total_count);
    // Without rows (a search matching nothing, an offset past the end) the
    // badge still counts every unread thread.
    let unread_count = match rows.first() {
        Some(row) => row.total_unread,
        None => unread_threads(pool, provider, &user, scope, filter.as_deref()).await?,
    };
    let phones: Vec<String> = rows
        .iter()
        .map(|row| super::provider::phone10(&row.phone_digits))
        .filter(|phone| !phone.is_empty())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let ids: Vec<i64> = rows.iter().map(|row| row.last_id).collect();
    let last = last_rows(pool, provider, user.company_id, &ids).await?;
    let customers = customers_by_phone(pool, provider, user.company_id, &phones).await?;
    let suppliers = suppliers_by_phone(pool, user.company_id, &phones).await?;

    let threads: Vec<ThreadSummary> = rows
        .into_iter()
        .map(|row| {
            let key = super::provider::phone10(&row.phone_digits);
            let customer = customers.get(&key);
            let last = last.get(&row.last_id);
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
        .collect();
    let has_more = offset + i64::try_from(threads.len()).unwrap_or(0) < total_count;
    Ok(Json(ThreadList {
        threads,
        total_count,
        unread_count,
        has_more,
    }))
}

/// Count unread conversations
///
/// Conversations with unread messages, for the sidebar badge.
#[crate::openapi::path(
    get,
    path = "/v1/sms/{provider}/unread-count",
    operation_id = "count_unread_sms_threads",
    tag = "sms",
    params(("provider" = SmsProvider, Path, description = "SMS provider"), BadgeQuery),
    responses(
        (status = 200, description = "The count", body = UnreadCount),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn unread_count(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(provider): Path<SmsProvider>,
    MultiQuery(query): MultiQuery<BadgeQuery>,
) -> ApiResult<Json<UnreadCount>> {
    let scope = query.scope.for_user(&user);
    let count = unread_threads(&state.pool, provider, &user, scope, None).await?;
    Ok(Json(UnreadCount { count }))
}

#[derive(sqlx::FromRow)]
struct AlertRow {
    phone_digits: String,
    last_id: i64,
    last_at: DateTime<Utc>,
}

/// Unread texts to alert about
///
/// Up to 50 unread conversations whose latest message came from the
/// customer, for desktop notifications. Empty for a user without an agent id
/// (Telnyx: without a line) or a company without the provider.
#[crate::openapi::path(
    get,
    path = "/v1/sms/{provider}/desktop-alerts",
    operation_id = "list_sms_desktop_alerts",
    tag = "sms",
    params(("provider" = SmsProvider, Path, description = "SMS provider")),
    responses(
        (status = 200, description = "Alerts", body = DesktopAlerts),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn desktop_alerts(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(provider): Path<SmsProvider>,
) -> ApiResult<Json<DesktopAlerts>> {
    let alerts = provider_alerts(&state.pool, provider, &user, provider).await?;
    Ok(Json(DesktopAlerts { alerts }))
}

/// The caller's unread conversations through `provider` whose latest
/// message came from the customer (at most 50), linking to the `inbox`
/// page; none without a seat there or the company's connection.
pub(super) async fn provider_alerts(
    pool: &MySqlPool,
    provider: SmsProvider,
    user: &SessionUser,
    inbox: SmsProvider,
) -> ApiResult<Vec<DesktopAlert>> {
    let user = user.clone();
    let features = company_features(pool, user.company_id).await?;
    let (enabled, texts) = match provider {
        SmsProvider::Cloudtalk => (features.cloudtalk, provider.can_send(&user)),
        SmsProvider::Ringcentral => (features.ringcentral, provider.can_send(&user)),
        SmsProvider::Telnyx => (
            features.telnyx_sms,
            !crate::phone::texting::inbox_lines(pool, user.company_id, user.id)
                .await?
                .is_empty(),
        ),
    };
    if !texts || !enabled {
        return Ok(Vec::new());
    }
    let rows: Vec<AlertRow> = rollup(
        pool,
        provider,
        &user,
        SmsScope::Mine,
        None,
        "",
        Mode::Alerts { limit: 50 },
    )
    .await?;
    let ids: Vec<i64> = rows.iter().map(|row| row.last_id).collect();
    let phones: Vec<String> = rows
        .iter()
        .map(|row| super::provider::phone10(&row.phone_digits))
        .collect();
    let last = last_rows(pool, provider, user.company_id, &ids).await?;
    let customers = customers_by_phone(pool, provider, user.company_id, &phones).await?;
    let alerts = rows
        .into_iter()
        .map(|row| {
            let name = customers
                .get(&super::provider::phone10(&row.phone_digits))
                .and_then(|c| c.1.as_deref())
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string);
            DesktopAlert {
                id: format!("sms-{}", row.phone_digits),
                title: name.unwrap_or_else(|| row.phone_digits.clone()),
                message: last
                    .get(&row.last_id)
                    .map(|l| l.text.clone())
                    .unwrap_or_default(),
                href: format!("/employee/{}/thread/{}", inbox.as_str(), row.phone_digits),
                sent_at: row.last_at,
                phone_digits: row.phone_digits,
            }
        })
        .collect();
    Ok(alerts)
}

/// List reps who text
///
/// The company's users with an agent (`CloudTalk`) or extension
/// (`RingCentral`) id, or with a Telnyx line (`agent_id` = their user id),
/// for the admin inbox's rep filter.
#[crate::openapi::path(
    get,
    path = "/v1/sms/{provider}/sales-reps",
    operation_id = "list_sms_sales_reps",
    tag = "sms",
    params(("provider" = SmsProvider, Path, description = "SMS provider")),
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
    Path(provider): Path<SmsProvider>,
) -> ApiResult<Json<Vec<SalesRep>>> {
    let Some(column) = provider.agent_column() else {
        // Telnyx: the users with a line, by their user id.
        let reps = crate::phone::texting::texting_users(&state.pool, user.company_id)
            .await?
            .into_iter()
            .map(|(id, name)| SalesRep {
                agent_id: id.to_string(),
                name,
            })
            .collect();
        return Ok(Json(reps));
    };
    let reps = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT TRIM({column}) AS agent_id, MIN(name) AS name FROM users
          WHERE company_id = ? AND is_deleted = 0
            AND {column} IS NOT NULL AND TRIM({column}) != ''
          GROUP BY TRIM({column})
          ORDER BY name"
    )))
    .bind(user.company_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(reps))
}

#[cfg(test)]
mod tests {
    use super::{Attachment, attachment_label};

    fn files(types: &[&str]) -> Vec<Attachment> {
        types
            .iter()
            .map(|content_type| Attachment {
                id: 1,
                sms_id: 1,
                url: String::new(),
                content_type: (*content_type).to_string(),
                filename: String::new(),
                width: None,
                height: None,
            })
            .collect()
    }

    #[test]
    fn labels_what_a_textless_message_sent() {
        assert_eq!(attachment_label(&files(&["image/png"])), "📷 Photo");
        assert_eq!(attachment_label(&files(&["image/png"; 7])), "📷 7 photos");
        assert_eq!(attachment_label(&files(&["application/pdf"])), "📎 File");
        assert_eq!(
            attachment_label(&files(&["application/pdf", "text/plain"])),
            "📎 2 files"
        );
        assert_eq!(
            attachment_label(&files(&["image/png", "image/jpeg", "application/pdf"])),
            "📷 2 photos, 📎 1 file"
        );
        assert_eq!(attachment_label(&[]), "");
    }
}
