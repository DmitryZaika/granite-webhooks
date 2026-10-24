//! `GET /v1/users/activity`: what the company's users did in the last 90
//! days, for the admin Users Activity page.
//!
//! Replaces the Remix `admin.users-activity` loader's queries (the page keeps
//! its onboarding progress, which lives in the onboarding store). Calls come
//! from `GET /v1/users/call-activity`.

use crate::auth::AdminUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::MultiQuery;
use crate::routes::leaderboard::queries::{CallSmsPointsRow, Window, call_sms_points_by_user};
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use chrono::{Datelike, Duration, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, MySqlPool};
use std::collections::HashMap;
use utoipa::{IntoParams, ToSchema};

/// Days of activity listed.
const DAYS: i64 = 90;
/// Text previews are capped in SQL so 90 days stay a light payload.
const PREVIEW: usize = 400;
/// `positions.id` of a sales rep.
const SALES_REP: i32 = 1;

/// Query of `GET /v1/users/activity`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ActivityQuery {
    /// First day of the "Calls & SMS points" table (default: the 1st of this month, UTC).
    pub calls_from: Option<NaiveDate>,
    /// Last day of that table, inclusive (default: today, UTC).
    pub calls_to: Option<NaiveDate>,
}

/// A sales rep of the company.
#[derive(Debug, Serialize, FromRow, ToSchema)]
pub struct ActivityRep {
    /// User id.
    pub id: i32,
    /// Name.
    pub name: Option<String>,
    /// Email.
    pub email: String,
    /// `CloudTalk` agent id.
    pub cloudtalk_agent_id: Option<String>,
}

/// Something a user did.
#[derive(Debug, Serialize, ToSchema)]
pub struct UserActivity {
    /// Id of the row in its source.
    pub id: i64,
    /// `deal_activity`, `deal_note`, `email`, `cloudtalk_sms`, ...
    pub source: String,
    /// Who did it, when known.
    pub user_id: Option<i64>,
    /// Their name.
    pub user_name: Option<String>,
    /// "Add Note", "Sent email", ...
    pub action: String,
    /// When (UTC, ISO 8601).
    pub created_at: Option<String>,
    /// The customer (or user, for "Created user").
    pub customer_name: Option<String>,
    /// The customer.
    pub customer_id: Option<i64>,
    /// The deal.
    pub deal_id: Option<i64>,
    /// The email thread.
    pub thread_id: Option<String>,
    /// One line shown next to the action: email subject, activity name, ...
    pub summary: Option<String>,
    /// Who it went to: an email address or phone number.
    pub contact: Option<String>,
    /// Longer text (capped), shown when the row is expanded.
    pub body: Option<String>,
    /// Image or document URL.
    pub file_url: Option<String>,
    /// A deal activity's deadline (UTC, ISO 8601).
    pub due_at: Option<String>,
    /// Labels: stage, amount, "Received", ...
    pub tags: Vec<String>,
}

/// `GET /v1/users/activity`.
#[derive(Debug, Serialize, ToSchema)]
pub struct ActivityPage {
    /// The company's sales reps, by name.
    pub sales_reps: Vec<ActivityRep>,
    /// The last 90 days, newest first.
    pub activities: Vec<UserActivity>,
    /// Calls, texts and points per user in the table's days; null while the
    /// company's leaderboard is off.
    pub call_sms_points: Option<Vec<CallSmsPointsRow>>,
    /// The table's days.
    pub calls_from: NaiveDate,
    /// Inclusive.
    pub calls_to: NaiveDate,
}

/// A row of any source; columns a source lacks stay empty.
#[derive(Debug, Default, FromRow)]
struct Row {
    id: i64,
    source: String,
    user_id: Option<i64>,
    user_name: Option<String>,
    action: String,
    created_at: Option<String>,
    #[sqlx(default)]
    customer_name: Option<String>,
    #[sqlx(default)]
    customer_id: Option<i64>,
    #[sqlx(default)]
    deal_id: Option<i64>,
    #[sqlx(default)]
    summary: Option<String>,
    #[sqlx(default)]
    contact: Option<String>,
    #[sqlx(default)]
    body: Option<String>,
    #[sqlx(default)]
    file_url: Option<String>,
    #[sqlx(default)]
    deadline: Option<String>,
    #[sqlx(default)]
    customer_source: Option<String>,
    #[sqlx(default)]
    amount: Option<f64>,
    #[sqlx(default)]
    stage: Option<String>,
}

impl Row {
    fn into_activity(self) -> UserActivity {
        UserActivity {
            id: self.id,
            source: self.source,
            user_id: self.user_id,
            user_name: self.user_name,
            action: self.action,
            created_at: self.created_at,
            customer_name: self.customer_name,
            customer_id: self.customer_id,
            deal_id: self.deal_id,
            thread_id: None,
            summary: self.summary,
            contact: self.contact,
            body: self.body,
            file_url: self.file_url,
            due_at: None,
            tags: Vec::new(),
        }
    }
}

#[derive(Debug, FromRow)]
struct SmsRow {
    id: i64,
    source: String,
    user_id: Option<i64>,
    user_name: Option<String>,
    created_at: Option<String>,
    sender: String,
    recipient: String,
    body: Option<String>,
    direction: Option<String>,
    status: Option<String>,
}

#[derive(Debug, FromRow)]
struct EmailRow {
    id: i64,
    user_id: Option<i64>,
    user_name: Option<String>,
    created_at: Option<String>,
    deal_customer_name: Option<String>,
    deal_customer_id: Option<i64>,
    deal_id: Option<i64>,
    thread_id: Option<String>,
    subject: Option<String>,
    body: Option<String>,
    receiver_email: Option<String>,
    sender_email: Option<String>,
}

/// A customer's id, name and two phone numbers.
type PhoneRow = (i64, Option<String>, Option<String>, Option<String>);

/// A customer an email or a text was matched to.
#[derive(Debug, Clone)]
pub(super) struct CustomerRef {
    pub(super) id: i64,
    pub(super) name: Option<String>,
}

/// Whole dollars with thousands separators (`$1,235`); None for 0.
fn format_money(value: Option<f64>) -> Option<String> {
    let value = value.filter(|v| v.is_finite() && *v != 0.0)?;
    #[allow(clippy::cast_possible_truncation)]
    let dollars = value.abs().round() as i64;
    let digits = dollars.to_string();
    let mut grouped = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    let sign = if value < 0.0 { "-" } else { "" };
    Some(format!("{sign}${grouped}"))
}

/// The first non-blank line, cut to `max` characters with an ellipsis.
fn first_line(value: Option<&str>, max: usize) -> Option<String> {
    let line = value?
        .split('\n')
        .map(str::trim)
        .find(|part| !part.is_empty())?;
    if line.chars().count() > max {
        let cut: String = line.chars().take(max - 1).collect();
        Some(format!("{cut}…"))
    } else {
        Some(line.to_string())
    }
}

/// The decoded last path segment of a URL.
fn file_name(url: Option<&str>) -> Option<String> {
    let last = url?.split('?').next()?.rsplit('/').next()?;
    if last.is_empty() {
        return None;
    }
    Some(
        percent_encoding::percent_decode_str(last)
            .decode_utf8()
            .map_or_else(|_| last.to_string(), std::borrow::Cow::into_owned),
    )
}

/// The address inside `Name <address>`, lower-cased.
fn email_key(raw: Option<&str>) -> String {
    let raw = raw.unwrap_or_default().trim();
    let address = raw
        .find('<')
        .and_then(|start| {
            raw[start + 1..]
                .find('>')
                .map(|end| &raw[start + 1..start + 1 + end])
        })
        .unwrap_or(raw);
    address.trim().to_lowercase()
}

/// The last 10 digits of a phone number, when it has at least 10.
pub(super) fn last10(value: &str) -> Option<String> {
    let digits: String = value.chars().filter(char::is_ascii_digit).collect();
    (digits.len() >= 10).then(|| digits[digits.len() - 10..].to_string())
}

/// Phone (last 10 digits) → customer: `CloudTalk` contact numbers first, then
/// the customers' own numbers win.
pub(super) async fn customers_by_phone(
    pool: &MySqlPool,
    company: i32,
) -> sqlx::Result<HashMap<String, CustomerRef>> {
    let contacts: Vec<PhoneRow> = sqlx::query_as(
        "SELECT CAST(c.id AS SIGNED), c.name, ct.phone_e164_1, ct.phone_e164_2
           FROM cloudtalk_contacts ct
           JOIN customers c ON c.id = ct.customer_id AND c.company_id = ? AND c.deleted_at IS NULL
          WHERE ct.company_id = ?",
    )
    .bind(company)
    .bind(company)
    .fetch_all(pool)
    .await?;
    let customers: Vec<PhoneRow> = sqlx::query_as(
        "SELECT CAST(id AS SIGNED), name, phone, phone_2 FROM customers
          WHERE company_id = ? AND deleted_at IS NULL",
    )
    .bind(company)
    .fetch_all(pool)
    .await?;
    let mut map = HashMap::new();
    for (overwrite, rows) in [(false, contacts), (true, customers)] {
        for (id, name, first, second) in rows {
            for phone in [first, second].into_iter().flatten() {
                if let Some(key) = last10(&phone)
                    && (overwrite || !map.contains_key(&key))
                {
                    map.insert(
                        key,
                        CustomerRef {
                            id,
                            name: name.clone(),
                        },
                    );
                }
            }
        }
    }
    Ok(map)
}

/// Email address → customer (the first customer listing it).
async fn customers_by_email(
    pool: &MySqlPool,
    company: i32,
) -> sqlx::Result<HashMap<String, CustomerRef>> {
    let rows: Vec<(i64, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT CAST(c.id AS SIGNED), c.name, ce.email
           FROM customers c JOIN customers_emails ce ON ce.customer_id = c.id
          WHERE c.company_id = ? AND c.deleted_at IS NULL",
    )
    .bind(company)
    .fetch_all(pool)
    .await?;
    let mut map = HashMap::new();
    for (id, name, email) in rows {
        let key = email_key(email.as_deref());
        if !key.is_empty() {
            map.entry(key).or_insert(CustomerRef { id, name });
        }
    }
    Ok(map)
}

/// Rows of one source query; every query takes the company and the cutoff.
async fn rows(
    pool: &MySqlPool,
    sql: &str,
    company: i32,
    cutoff: NaiveDate,
) -> sqlx::Result<Vec<Row>> {
    let binds = sql.matches('?').count();
    let mut query = sqlx::query_as::<_, Row>(sqlx::AssertSqlSafe(sql.to_string()));
    // Every query lists the company first and the cutoff last.
    for _ in 1..binds {
        query = query.bind(company);
    }
    query.bind(cutoff).fetch_all(pool).await
}

/// Joins `u_creator`: the live user of the company named in `column`.
fn creator(company: &str, column: &str) -> String {
    format!(
        "LEFT JOIN users u_creator ON u_creator.company_id = {company} AND u_creator.is_deleted = 0
          AND {column} IS NOT NULL AND TRIM({column}) != ''
          AND LOWER(TRIM(u_creator.name)) = LOWER(TRIM({column}))"
    )
}

#[allow(clippy::too_many_lines)] // one query per source, read top to bottom
async fn deal_rows(
    pool: &MySqlPool,
    company: i32,
    cutoff: NaiveDate,
) -> sqlx::Result<Vec<UserActivity>> {
    let deal_scope = "LEFT JOIN deals d ON d.id = {t}.deal_id AND d.deleted_at IS NULL
        LEFT JOIN customers cust ON cust.id = d.customer_id AND cust.deleted_at IS NULL";
    let scope = |table: &str| deal_scope.replace("{t}", table);
    let deal_user = "LEFT JOIN users u_deal ON u_deal.id = d.user_id AND u_deal.company_id = {t}.company_id AND u_deal.is_deleted = 0";
    let owner = |table: &str| deal_user.replace("{t}", table);
    let mut all = Vec::new();

    let created = rows(pool, &format!(
        "SELECT CAST(da.id AS SIGNED) AS id, 'deal_activity' AS source,
                CAST(COALESCE(u_creator.id, u_deal.id) AS SIGNED) AS user_id,
                COALESCE(u_creator.name, u_deal.name) AS user_name,
                CASE WHEN LOWER(da.name) LIKE '%text%' THEN 'Sent a text' ELSE 'Add Activity' END AS action,
                DATE_FORMAT(da.created_at, '%Y-%m-%dT%H:%i:%sZ') AS created_at,
                cust.name AS customer_name, CAST(cust.id AS SIGNED) AS customer_id,
                CAST(da.deal_id AS SIGNED) AS deal_id, da.name AS summary,
                DATE_FORMAT(da.deadline, '%Y-%m-%dT%H:%i:%sZ') AS deadline
           FROM deal_activities da {} {} {}
          WHERE da.company_id = ? AND da.deleted_at IS NULL AND da.created_at >= ?",
        scope("da"), creator("da.company_id", "da.created_by"), owner("da"),
    ), company, cutoff).await?;
    all.extend(created.into_iter().map(|row| {
        let due_at = row.deadline.clone();
        let mut activity = row.into_activity();
        activity.due_at = due_at;
        activity
    }));

    for sql in [
        format!(
            "SELECT CAST(da.id AS SIGNED) AS id, 'deal_activity_deleted' AS source,
                    CAST(u_deal.id AS SIGNED) AS user_id, u_deal.name AS user_name, 'Delete Activity' AS action,
                    DATE_FORMAT(da.deleted_at, '%Y-%m-%dT%H:%i:%sZ') AS created_at,
                    cust.name AS customer_name, CAST(cust.id AS SIGNED) AS customer_id,
                    CAST(da.deal_id AS SIGNED) AS deal_id, da.name AS summary
               FROM deal_activities da {} {}
              WHERE da.company_id = ? AND da.deleted_at IS NOT NULL AND da.deleted_at >= ?",
            scope("da"), owner("da"),
        ),
        format!(
            "SELECT CAST(da.id AS SIGNED) AS id, 'deal_activity_completed' AS source,
                    CAST(COALESCE(u_creator.id, u_deal.id) AS SIGNED) AS user_id,
                    COALESCE(u_creator.name, u_deal.name) AS user_name, 'Complete Activity' AS action,
                    DATE_FORMAT(da.completed_at, '%Y-%m-%dT%H:%i:%sZ') AS created_at,
                    cust.name AS customer_name, CAST(cust.id AS SIGNED) AS customer_id,
                    CAST(da.deal_id AS SIGNED) AS deal_id, da.name AS summary
               FROM deal_activities da {} {} {}
              WHERE da.company_id = ? AND da.deleted_at IS NULL AND da.is_completed = 1
                AND da.completed_at IS NOT NULL AND da.completed_at >= ?",
            scope("da"), creator("da.company_id", "da.created_by"), owner("da"),
        ),
    ] {
        all.extend(rows(pool, &sql, company, cutoff).await?.into_iter().map(Row::into_activity));
    }

    for (source, action, date) in [
        ("deal_note", "Add Note", "created_at"),
        ("deal_note_deleted", "Delete Note", "deleted_at"),
    ] {
        let live = if date == "created_at" {
            "dn.deleted_at IS NULL AND dn.created_at >= ?"
        } else {
            "dn.deleted_at IS NOT NULL AND dn.deleted_at >= ?"
        };
        let notes = rows(pool, &format!(
            "SELECT CAST(dn.id AS SIGNED) AS id, '{source}' AS source,
                    CAST(u_deal.id AS SIGNED) AS user_id, u_deal.name AS user_name, '{action}' AS action,
                    DATE_FORMAT(dn.{date}, '%Y-%m-%dT%H:%i:%sZ') AS created_at,
                    cust.name AS customer_name, CAST(cust.id AS SIGNED) AS customer_id,
                    CAST(dn.deal_id AS SIGNED) AS deal_id, LEFT(dn.content, {PREVIEW}) AS body
               FROM deal_notes dn {} {}
              WHERE dn.company_id = ? AND {live}",
            scope("dn"), owner("dn"),
        ), company, cutoff).await?;
        all.extend(notes.into_iter().map(|row| {
            let summary = first_line(row.body.as_deref(), 120);
            let mut activity = row.into_activity();
            activity.summary = summary;
            activity
        }));
    }

    for (table, source, action, url, summary) in [
        (
            "deals_images",
            "deal_image",
            "Added image to a deal",
            "image_url",
            "NULL",
        ),
        (
            "deals_documents",
            "deal_document",
            "Added document",
            "image_url",
            "NULLIF(TRIM(f.name), '')",
        ),
    ] {
        let sql = format!(
            "SELECT CAST(f.id AS SIGNED) AS id, '{source}' AS source,
                    CAST(u_creator.id AS SIGNED) AS user_id,
                    COALESCE(u_creator.name, NULLIF(TRIM(f.created_by), '')) AS user_name,
                    '{action}' AS action,
                    DATE_FORMAT(f.created_at, '%Y-%m-%dT%H:%i:%sZ') AS created_at,
                    COALESCE((SELECT NULLIF(TRIM(c2.name), '') FROM customers c2
                               WHERE c2.id = d.customer_id AND c2.company_id = ? LIMIT 1),
                             CONCAT('Deal #', CAST(d.id AS CHAR))) AS customer_name,
                    CAST(d.customer_id AS SIGNED) AS customer_id, CAST(d.id AS SIGNED) AS deal_id,
                    {summary} AS summary, f.{url} AS file_url
               FROM {table} f
               JOIN deals d ON d.id = f.deal_id AND d.deleted_at IS NULL
               {}
              WHERE EXISTS (SELECT 1 FROM customers c_scope
                             WHERE c_scope.id = d.customer_id AND c_scope.company_id = ?)
                AND f.created_at >= ?",
            creator("?", "f.created_by"),
        );
        let files = rows(pool, &sql, company, cutoff).await?;
        all.extend(files.into_iter().map(|row| {
            let name = row
                .summary
                .clone()
                .or_else(|| file_name(row.file_url.as_deref()));
            let mut activity = row.into_activity();
            activity.summary = name;
            activity
        }));
    }

    let created = rows(pool, &format!(
        "SELECT CAST(d.id AS SIGNED) AS id, 'deal_created' AS source,
                CAST(u_creator.id AS SIGNED) AS user_id,
                COALESCE(u_creator.name, NULLIF(TRIM(d.created_by), '')) AS user_name,
                'Created deal' AS action,
                DATE_FORMAT(d.created_at, '%Y-%m-%dT%H:%i:%sZ') AS created_at,
                cust.name AS customer_name, CAST(cust.id AS SIGNED) AS customer_id,
                CAST(d.id AS SIGNED) AS deal_id, CAST(d.amount AS DOUBLE) AS amount, dl.name AS stage,
                LEFT(d.description, {PREVIEW}) AS body
           FROM deals d
           JOIN customers cust ON cust.id = d.customer_id AND cust.deleted_at IS NULL
           LEFT JOIN deals_list dl ON dl.id = d.list_id
           {}
          WHERE cust.company_id = ? AND d.created_at >= ?",
        creator("cust.company_id", "d.created_by"),
    ), company, cutoff).await?;
    all.extend(created.into_iter().map(|row| {
        let money = format_money(row.amount);
        let tags: Vec<String> = [row.stage.clone(), money.clone()]
            .into_iter()
            .flatten()
            .filter(|tag| !tag.is_empty())
            .collect();
        let summary = [money, first_line(row.body.as_deref(), 80)]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ");
        let mut activity = row.into_activity();
        activity.tags = tags;
        activity.summary = (!summary.is_empty()).then_some(summary);
        activity
    }));

    let deleted = rows(pool, &format!(
        "SELECT CAST(d.id AS SIGNED) AS id, 'deal_deleted' AS source,
                CAST(COALESCE(u_deal.id, u_creator.id) AS SIGNED) AS user_id,
                COALESCE(u_deal.name, u_creator.name, NULLIF(TRIM(d.created_by), '')) AS user_name,
                'Deleted deal' AS action,
                DATE_FORMAT(d.deleted_at, '%Y-%m-%dT%H:%i:%sZ') AS created_at,
                cust.name AS customer_name, CAST(cust.id AS SIGNED) AS customer_id,
                LEFT(d.description, {PREVIEW}) AS body
           FROM deals d
           JOIN customers cust ON cust.id = d.customer_id AND cust.deleted_at IS NULL
           LEFT JOIN users u_deal ON u_deal.id = d.user_id AND u_deal.company_id = cust.company_id AND u_deal.is_deleted = 0
           {}
          WHERE cust.company_id = ? AND d.deleted_at IS NOT NULL AND d.deleted_at >= ?",
        creator("cust.company_id", "d.created_by"),
    ), company, cutoff).await?;
    all.extend(deleted.into_iter().map(|row| {
        let summary = first_line(row.body.as_deref(), 80);
        let mut activity = row.into_activity();
        activity.summary = summary;
        activity
    }));
    Ok(all)
}

async fn people_rows(
    pool: &MySqlPool,
    company: i32,
    cutoff: NaiveDate,
) -> sqlx::Result<Vec<UserActivity>> {
    let mut all: Vec<UserActivity> = rows(
        pool,
        &format!(
            "SELECT CAST(u_new.id AS SIGNED) AS id, 'user_created' AS source,
                CAST(u_creator.id AS SIGNED) AS user_id, u_creator.name AS user_name,
                'Created user' AS action,
                DATE_FORMAT(u_new.created_date, '%Y-%m-%dT%H:%i:%sZ') AS created_at,
                u_new.name AS customer_name, u_new.email AS contact
           FROM users u_new
           {}
          WHERE u_new.company_id = ? AND u_new.is_deleted = 0 AND u_new.created_date >= ?
            AND u_new.created_by IS NOT NULL AND TRIM(u_new.created_by) != ''",
            creator("u_new.company_id", "u_new.created_by"),
        ),
        company,
        cutoff,
    )
    // `users.created_by` is not in every schema (no migration adds it):
    // like the Remix page, the source is then just empty.
    .await
    .unwrap_or_default()
    .into_iter()
    .map(Row::into_activity)
    .collect();
    let customers = rows(pool, &format!(
        "SELECT CAST(c.id AS SIGNED) AS id, 'customer_created' AS source,
                CAST(u_creator.id AS SIGNED) AS user_id,
                COALESCE(u_creator.name, NULLIF(TRIM(c.created_by), '')) AS user_name,
                'Created customer' AS action,
                DATE_FORMAT(c.created_date, '%Y-%m-%dT%H:%i:%sZ') AS created_at,
                c.name AS customer_name, CAST(c.id AS SIGNED) AS customer_id,
                COALESCE(NULLIF(TRIM(c.phone), ''), NULLIF(TRIM(ce_primary.email), '')) AS contact,
                NULLIF(TRIM(CONCAT_WS(', ', NULLIF(TRIM(c.address), ''), NULLIF(TRIM(c.city), ''))), '') AS summary,
                NULLIF(TRIM(c.source), '') AS customer_source
           FROM customers c
           LEFT JOIN customers_emails ce_primary ON ce_primary.id = c.email_id
           {}
          WHERE c.company_id = ? AND c.deleted_at IS NULL AND c.created_date >= ?
            AND NOT ((c.created_by IS NULL OR TRIM(c.created_by) = '')
                     AND LOWER(TRIM(IFNULL(c.source, ''))) = 'leads')",
        creator("c.company_id", "c.created_by"),
    ), company, cutoff).await?;
    all.extend(customers.into_iter().map(|row| {
        let tags = row
            .customer_source
            .as_ref()
            .map(|source| vec![format!("Source: {source}")])
            .unwrap_or_default();
        let mut activity = row.into_activity();
        activity.tags = tags;
        activity
    }));
    Ok(all)
}

async fn sms_rows(
    pool: &MySqlPool,
    company: i32,
    cutoff: NaiveDate,
) -> sqlx::Result<Vec<UserActivity>> {
    let phones = customers_by_phone(pool, company).await?;
    let part = |table: &str, source: &str, agent: &str| {
        format!(
            "SELECT CAST(m.id AS SIGNED) AS id, '{source}' AS source, CAST(u.id AS SIGNED) AS user_id,
                    u.name AS user_name,
                    DATE_FORMAT(m.created_date, '%Y-%m-%dT%H:%i:%sZ') AS created_at,
                    CAST(IFNULL(m.sender, '') AS CHAR) AS sender,
                    CAST(IFNULL(m.recipient, '') AS CHAR) AS recipient,
                    LEFT(m.text, {PREVIEW}) AS body,
                    CAST(m.direction AS CHAR) AS direction, CAST(m.status AS CHAR) AS status
               FROM {table} m
               JOIN users u ON u.{agent} = m.agent
              WHERE u.company_id = ? AND u.is_deleted = 0
                AND (m.company_id = ? OR m.company_id IS NULL) AND m.created_date >= ?"
        )
    };
    let sql = format!(
        "{} UNION ALL {} ORDER BY created_at DESC, id DESC",
        part("cloudtalk_sms", "cloudtalk_sms", "cloudtalk_agent_id"),
        part(
            "ringcentral_sms",
            "ringcentral_sms",
            "ringcentral_extension_id"
        ),
    );
    let texts: Vec<SmsRow> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(company)
        .bind(company)
        .bind(cutoff)
        .bind(company)
        .bind(company)
        .bind(cutoff)
        .fetch_all(pool)
        .await?;
    Ok(texts
        .into_iter()
        .map(|row| {
            let customer = [&row.recipient, &row.sender]
                .into_iter()
                .filter_map(|party| last10(party))
                .find_map(|key| phones.get(&key).cloned());
            let inbound = row.direction.as_deref() == Some("inbound");
            let mut tags = Vec::new();
            if inbound {
                tags.push("Received".to_string());
            }
            match row.status.as_deref() {
                Some("failed") => tags.push("Failed".into()),
                Some("pending") => tags.push("Pending".into()),
                _ => {}
            }
            let party = if inbound { &row.sender } else { &row.recipient };
            UserActivity {
                id: row.id,
                source: row.source,
                user_id: row.user_id,
                user_name: row.user_name,
                action: "Sent a text".into(),
                created_at: row.created_at,
                customer_name: customer.as_ref().and_then(|c| c.name.clone()),
                customer_id: customer.map(|c| c.id),
                deal_id: None,
                thread_id: None,
                summary: first_line(row.body.as_deref(), 120),
                contact: (!party.is_empty()).then(|| party.clone()),
                body: row.body,
                file_url: None,
                due_at: None,
                tags,
            }
        })
        .collect())
}

async fn email_rows(
    pool: &MySqlPool,
    company: i32,
    cutoff: NaiveDate,
) -> sqlx::Result<Vec<UserActivity>> {
    let emails = customers_by_email(pool, company).await?;
    let rows: Vec<EmailRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT CAST(e.id AS SIGNED) AS id, CAST(e.sender_user_id AS SIGNED) AS user_id, u.name AS user_name,
                DATE_FORMAT(e.sent_at, '%Y-%m-%dT%H:%i:%sZ') AS created_at,
                c_deal.name AS deal_customer_name, CAST(c_deal.id AS SIGNED) AS deal_customer_id,
                CAST(d.id AS SIGNED) AS deal_id, e.thread_id, e.subject,
                LEFT(e.body, {PREVIEW}) AS body, e.receiver_email, e.sender_email
           FROM emails e
           JOIN users u ON u.id = e.sender_user_id
           LEFT JOIN deals d ON d.id = e.deal_id AND d.deleted_at IS NULL
           LEFT JOIN customers c_deal ON c_deal.id = d.customer_id AND c_deal.deleted_at IS NULL
          WHERE u.company_id = ? AND u.is_deleted = 0 AND e.deleted_at IS NULL AND e.is_draft = 0
            AND e.sent_at >= ?
          ORDER BY e.sent_at DESC, e.id DESC"
    )))
    .bind(company)
    .bind(cutoff)
    .fetch_all(pool)
    .await?;
    let trimmed = |value: &Option<String>| {
        value
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    Ok(rows
        .into_iter()
        .map(|row| {
            let matched = [&row.receiver_email, &row.sender_email]
                .into_iter()
                .map(|email| email_key(email.as_deref()))
                .filter(|key| !key.is_empty())
                .find_map(|key| emails.get(&key).cloned());
            let receiver = trimmed(&row.receiver_email);
            UserActivity {
                id: row.id,
                source: "email".into(),
                user_id: row.user_id,
                user_name: row.user_name,
                action: "Sent email".into(),
                created_at: row.created_at,
                customer_name: row
                    .deal_customer_name
                    .clone()
                    .or_else(|| matched.as_ref().and_then(|c| c.name.clone()))
                    .or_else(|| receiver.clone())
                    .or_else(|| trimmed(&row.sender_email)),
                customer_id: row.deal_customer_id.or_else(|| matched.map(|c| c.id)),
                deal_id: row.deal_id,
                thread_id: row.thread_id,
                summary: Some(trimmed(&row.subject).unwrap_or_else(|| "(no subject)".into())),
                contact: receiver,
                body: row.body,
                file_url: None,
                due_at: None,
                tags: Vec::new(),
            }
        })
        .collect())
}

/// Users activity
///
/// The last 90 days of what the company's users did: deal activities (added,
/// completed, deleted), notes, deal pictures and documents, deals made and
/// deleted, users and customers added, texts and emails (matched to customers
/// by phone and address), newest first; the sales reps; and, while the
/// leaderboard is on, everyone's calls, texts and points in the table's days.
#[utoipa::path(
    get,
    path = "/v1/users/activity",
    operation_id = "list_users_activity",
    tag = "users",
    params(ActivityQuery),
    responses(
        (status = 200, description = "Activity", body = ActivityPage),
        (status = 400, description = "The table's days end before they start", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn activity(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    MultiQuery(query): MultiQuery<ActivityQuery>,
) -> ApiResult<Json<ActivityPage>> {
    let pool = &state.pool;
    let company = user.company_id;
    let today = Utc::now().date_naive();
    let calls_from = query
        .calls_from
        .unwrap_or_else(|| today.with_day(1).unwrap_or(today));
    let calls_to = query.calls_to.unwrap_or(today);
    if calls_to < calls_from {
        return Err(ApiError::BadRequest(
            "The days end before they start".into(),
        ));
    }
    let cutoff = today - Duration::days(DAYS);
    let sales_reps: Vec<ActivityRep> = sqlx::query_as(
        "SELECT DISTINCT u.id, u.name, u.email, u.cloudtalk_agent_id
           FROM users u
           JOIN users_positions up ON up.user_id = u.id AND up.company_id = u.company_id
          WHERE u.is_deleted = 0 AND up.position_id = ? AND u.company_id = ?
          ORDER BY u.name, u.id",
    )
    .bind(SALES_REP)
    .bind(company)
    .fetch_all(pool)
    .await?;
    let features = crate::routes::company::queries::company_features(pool, company).await?;
    let call_sms_points = if features.leaderboard {
        let window = Window {
            from: Some(calls_from),
            to: calls_to.succ_opt(),
        };
        Some(call_sms_points_by_user(pool, company, window).await?)
    } else {
        None
    };
    let mut activities = deal_rows(pool, company, cutoff).await?;
    activities.extend(people_rows(pool, company, cutoff).await?);
    activities.extend(sms_rows(pool, company, cutoff).await?);
    activities.extend(email_rows(pool, company, cutoff).await?);
    // ISO 8601 UTC sorts as text; stable, so ties keep the source order.
    activities.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(Json(ActivityPage {
        sales_reps,
        activities,
        call_sms_points,
        calls_from,
        calls_to,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_helpers_match_the_page() {
        assert_eq!(format_money(Some(1234.5)), Some("$1,235".into()));
        assert_eq!(format_money(Some(-50.0)), Some("-$50".into()));
        assert_eq!(format_money(Some(0.0)), None);
        assert_eq!(format_money(Some(1_000_000.0)), Some("$1,000,000".into()));
        assert_eq!(
            first_line(Some("\n  hello \nworld"), 120),
            Some("hello".into())
        );
        assert_eq!(first_line(Some("abcdef"), 4), Some("abc…".into()));
        assert_eq!(first_line(Some("  \n "), 4), None);
        assert_eq!(
            file_name(Some("https://x.test/a/My%20Plan.pdf?sig=1")),
            Some("My Plan.pdf".into())
        );
        assert_eq!(email_key(Some(" Ann <ANN@x.com> ")), "ann@x.com");
        assert_eq!(email_key(Some("b@y.com")), "b@y.com");
        assert_eq!(last10("+1 (317) 555-0100"), Some("3175550100".into()));
        assert_eq!(last10("555-0100"), None);
    }
}
