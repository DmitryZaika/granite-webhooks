//! Reads of the deals domain. Every query is scoped by the caller's company
//! through `customers.company_id` (deals have no company column).

use super::schemas::{
    Activity, ActivityPreview, ActivitySuggestion, BoardDeal, CustomerDeal, DealCustomer,
    DealDetail, DealGroup, DealList, DealStatus, NextActivity, Note, NoteComment, NotePreview,
    Recipient, StageVisit,
};
use crate::sql::push_in;
use sqlx::{FromRow, MySql, MySqlPool, QueryBuilder};
use std::collections::{HashMap, HashSet};

/// Store wall time format of activity deadlines.
const LOCAL_DATETIME: &str = "'%Y-%m-%dT%H:%i:%s'";

/// Groups visible to the company: its own plus the shared group 1.
const GROUP_VISIBLE: &str = "gl.deleted_at IS NULL AND (gl.company_id = ? OR gl.id = 1)";

pub async fn list_groups(
    pool: &MySqlPool,
    company_id: i32,
    user_id: i32,
    status: DealStatus,
) -> Result<Vec<DealGroup>, sqlx::Error> {
    let sql = format!(
        "SELECT gl.id, gl.name,
                EXISTS (
                  SELECT 1 FROM deal_activities da
                  JOIN deals d ON d.id = da.deal_id AND d.deleted_at IS NULL
                  JOIN deals_list dl ON dl.id = d.list_id AND dl.deleted_at IS NULL
                  WHERE dl.group_id = gl.id AND d.user_id = ? AND {won}
                    AND da.company_id = ? AND da.deleted_at IS NULL AND da.is_completed = 0
                    AND da.deadline IS NOT NULL AND DATE(da.deadline) < CURDATE()
                ) AS has_overdue_activity
         FROM groups_list gl
         WHERE {GROUP_VISIBLE} AND gl.is_displayed = 1
         ORDER BY gl.id",
        won = status.sql("d"),
    );
    sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(user_id)
        .bind(company_id)
        .bind(company_id)
        .fetch_all(pool)
        .await
}

pub async fn list_lists(
    pool: &MySqlPool,
    company_id: i32,
    group_id: Option<i32>,
) -> Result<Vec<DealList>, sqlx::Error> {
    let base = format!(
        "SELECT dl.id, dl.name, dl.position, dl.group_id, gl.name AS group_name
         FROM deals_list dl JOIN groups_list gl ON gl.id = dl.group_id
         WHERE dl.deleted_at IS NULL AND {GROUP_VISIBLE}"
    );
    match group_id {
        Some(group_id) => {
            sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "{base} AND dl.group_id = ? ORDER BY dl.position, dl.id"
            )))
            .bind(company_id)
            .bind(group_id)
            .fetch_all(pool)
            .await
        }
        None => {
            sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "{base} AND gl.is_displayed = 1 ORDER BY gl.name, dl.position, dl.id"
            )))
            .bind(company_id)
            .fetch_all(pool)
            .await
        }
    }
}

/// Whether `list_id` is a live stage of a group the company can use.
pub async fn list_is_visible(
    pool: &MySqlPool,
    company_id: i32,
    list_id: i32,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT dl.name FROM deals_list dl JOIN groups_list gl ON gl.id = dl.group_id
         WHERE dl.id = ? AND dl.deleted_at IS NULL AND {GROUP_VISIBLE}"
    )))
    .bind(list_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await
}

#[derive(FromRow)]
struct BoardRow {
    id: i64,
    title: Option<String>,
    amount: Option<String>,
    list_id: i32,
    position: i32,
    status: Option<String>,
    is_won: Option<i32>,
    lost_reason: Option<String>,
    due_date: Option<String>,
    owner_id: i32,
    customer_id: Option<i32>,
    customer_name: Option<String>,
    company_name: Option<String>,
    temperature: Option<String>,
    sales_rep: Option<String>,
}

/// Whose deals a board query returns.
pub enum BoardOwner<'a> {
    User(i32),
    /// Every deal of the company, optionally one sales rep's (by name).
    Company(Option<&'a str>),
}

#[derive(FromRow)]
struct ActivityRow {
    id: i32,
    deal_id: i64,
    name: String,
    deadline: Option<String>,
    priority: String,
}

/// The caller's deals in the stages of `group_id`, with what the cards show.
#[allow(clippy::too_many_lines)]
pub async fn board(
    pool: &MySqlPool,
    company_id: i32,
    owner: BoardOwner<'_>,
    group_id: i32,
    status: DealStatus,
) -> Result<Vec<BoardDeal>, sqlx::Error> {
    let owner_sql = match owner {
        BoardOwner::User(_) => "AND d.user_id = ?",
        BoardOwner::Company(Some(_)) => "AND rep.name = ?",
        BoardOwner::Company(None) => "",
    };
    let sql = format!(
        "SELECT CAST(d.id AS SIGNED) AS id, d.title, CAST(d.amount AS CHAR) AS amount, d.list_id,
                d.position, d.status, d.is_won, d.lost_reason,
                DATE_FORMAT(d.due_date, '%Y-%m-%d') AS due_date,
                owner.id AS owner_id, c.id AS customer_id, c.name AS customer_name, c.company_name,
                CAST(c.customer_temperature AS CHAR) AS temperature, rep.name AS sales_rep
         FROM deals d
         JOIN deals_list dl ON dl.id = d.list_id AND dl.deleted_at IS NULL
         JOIN groups_list gl ON gl.id = dl.group_id
         JOIN customers owner ON owner.id = d.customer_id AND owner.company_id = ?
         LEFT JOIN customers c ON c.id = d.customer_id AND c.deleted_at IS NULL
         LEFT JOIN users rep ON rep.id = d.user_id
         WHERE d.deleted_at IS NULL {owner_sql} AND {won}
           AND dl.group_id = ? AND {GROUP_VISIBLE}
         ORDER BY d.list_id, d.position, d.id",
        won = status.sql("d"),
    );
    let mut query = sqlx::query_as::<_, BoardRow>(sqlx::AssertSqlSafe(sql)).bind(company_id);
    query = match owner {
        BoardOwner::User(user_id) => query.bind(user_id),
        BoardOwner::Company(Some(name)) => query.bind(name.to_string()),
        BoardOwner::Company(None) => query,
    };
    let rows: Vec<BoardRow> = query
        .bind(group_id)
        .bind(company_id)
        .fetch_all(pool)
        .await?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<i64> = rows.iter().map(|row| row.id).collect();

    let with_images = deal_id_set(
        pool,
        "SELECT DISTINCT CAST(deal_id AS SIGNED) FROM deals_images WHERE 1 = 1",
        "deal_id",
        &ids,
        None,
    )
    .await?;
    let mut with_emails = deal_id_set(
        pool,
        "SELECT DISTINCT CAST(deal_id AS SIGNED) FROM emails WHERE deleted_at IS NULL",
        "deal_id",
        &ids,
        None,
    )
    .await?;
    // NO_SEMIJOIN: probe each deal's customer through the
    // email_participants.customer_id index (a semijoin was ~12x slower).
    with_emails.extend(
        deal_id_set(
            pool,
            "SELECT CAST(d.id AS SIGNED) FROM deals d WHERE EXISTS (
               SELECT /*+ NO_SEMIJOIN() */ 1 FROM email_participants ep
               JOIN emails e ON e.id = ep.email_id AND e.deleted_at IS NULL
               WHERE ep.customer_id = d.customer_id)",
            "d.id",
            &ids,
            None,
        )
        .await?,
    );
    let with_notes = deal_id_set(
        pool,
        "SELECT DISTINCT CAST(deal_id AS SIGNED) FROM deal_notes WHERE deleted_at IS NULL",
        "deal_id",
        &ids,
        Some(("company_id", company_id)),
    )
    .await?;

    let mut query = QueryBuilder::<MySql>::new(format!(
        "SELECT id, CAST(deal_id AS SIGNED) AS deal_id, name,
                DATE_FORMAT(deadline, {LOCAL_DATETIME}) AS deadline, CAST(priority AS CHAR) AS priority
         FROM deal_activities
         WHERE deleted_at IS NULL AND is_completed = 0 AND company_id = "
    ));
    query.push_bind(company_id);
    push_in(&mut query, "deal_id", &ids);
    query.push(
        " ORDER BY deadline IS NULL, deadline,
           CASE priority WHEN 'high' THEN 0 WHEN 'medium' THEN 1 WHEN 'low' THEN 2 ELSE 3 END,
           created_at, id",
    );
    let activities: Vec<ActivityRow> = query.build_query_as().fetch_all(pool).await?;
    let mut activity_count: HashMap<i64, i64> = HashMap::new();
    let mut next_activity: HashMap<i64, NextActivity> = HashMap::new();
    let mut earliest: HashMap<i64, String> = HashMap::new();
    for activity in activities {
        *activity_count.entry(activity.deal_id).or_default() += 1;
        if let Some(deadline) = &activity.deadline {
            earliest
                .entry(activity.deal_id)
                .or_insert_with(|| deadline.clone());
        }
        next_activity
            .entry(activity.deal_id)
            .or_insert(NextActivity {
                id: activity.id,
                name: activity.name,
                deadline: activity.deadline,
                priority: activity.priority,
            });
    }

    let sms_flows = sms_flow_phones(pool, company_id, &ids).await?;

    Ok(rows
        .into_iter()
        .map(|row| BoardDeal {
            id: row.id,
            title: row.title,
            amount: row.amount,
            list_id: row.list_id,
            position: row.position,
            status: row.status,
            outcome: DealStatus::from_is_won(row.is_won),
            lost_reason: row.lost_reason,
            due_date: row.due_date,
            customer_id: row.owner_id,
            customer: row.customer_id.map(|id| DealCustomer {
                id,
                name: row.customer_name,
                company_name: row.company_name,
                temperature: row.temperature,
            }),
            has_images: with_images.contains(&row.id),
            has_emails: with_emails.contains(&row.id),
            has_notes: with_notes.contains(&row.id),
            open_activity_count: activity_count.get(&row.id).copied().unwrap_or(0),
            earliest_activity_deadline: earliest.remove(&row.id),
            next_activity: next_activity.remove(&row.id),
            sms_flow_phone: sms_flows.get(&row.id).cloned(),
            sales_rep: row.sales_rep,
        })
        .collect())
}

/// Runs `select` (a statement with a WHERE clause and no binds), adding
/// `AND <company_column> = company_id` when given and `AND <id_column> IN ids`.
async fn deal_id_set(
    pool: &MySqlPool,
    select: &str,
    id_column: &str,
    ids: &[i64],
    company: Option<(&str, i32)>,
) -> Result<HashSet<i64>, sqlx::Error> {
    let mut query = QueryBuilder::<MySql>::new(select);
    if let Some((column, company_id)) = company {
        query.push(format!(" AND {column} = "));
        query.push_bind(company_id);
    }
    push_in(&mut query, id_column, ids);
    let rows: Vec<i64> = query.build_query_scalar().fetch_all(pool).await?;
    Ok(rows.into_iter().collect())
}

#[derive(FromRow)]
struct SmsFlowRow {
    deal_id: i64,
    phone_digits: Option<String>,
}

/// Phone digits of an active or paused SMS flow per deal, matched on the
/// customer id or the last ten digits of either customer phone.
async fn sms_flow_phones(
    pool: &MySqlPool,
    company_id: i32,
    ids: &[i64],
) -> Result<HashMap<i64, String>, sqlx::Error> {
    let digits = |column: &str| format!("REGEXP_REPLACE(COALESCE(c.{column}, ''), '[^0-9]', '')");
    let phone_match = |column: &str| {
        format!(
            "(CHAR_LENGTH({d}) >= 10 AND e.customer_phone_digits = CAST(RIGHT({d}, 10) AS UNSIGNED))",
            d = digits(column)
        )
    };
    let mut query = QueryBuilder::<MySql>::new(format!(
        "SELECT CAST(d.id AS SIGNED) AS deal_id, CAST(e.customer_phone_digits AS CHAR) AS phone_digits
         FROM deals d
         JOIN customers c ON c.id = d.customer_id
         JOIN sms_flow_enrollments e ON e.status IN ('active', 'paused')
           AND (e.customer_id = d.customer_id OR {} OR {})
           AND e.company_id = ",
        phone_match("phone"),
        phone_match("phone_2"),
    ));
    query.push_bind(company_id);
    query.push(" WHERE 1 = 1");
    push_in(&mut query, "d.id", ids);
    query.push(" ORDER BY e.id");
    let rows: Vec<SmsFlowRow> = query.build_query_as().fetch_all(pool).await?;
    let mut phones = HashMap::new();
    for row in rows {
        if let Some(digits) = row.phone_digits.filter(|d| !d.is_empty()) {
            phones.entry(row.deal_id).or_insert(digits);
        }
    }
    Ok(phones)
}

pub async fn find_deal(
    pool: &MySqlPool,
    company_id: i32,
    deal_id: i64,
) -> Result<Option<DealDetail>, sqlx::Error> {
    let deal: Option<DealDetail> = sqlx::query_as(
        "SELECT CAST(d.id AS SIGNED) AS id, d.title, CAST(d.amount AS CHAR) AS amount,
                d.list_id, dl.group_id, d.position, d.is_won, d.lost_reason,
                DATE_FORMAT(d.due_date, '%Y-%m-%d') AS due_date, d.user_id,
                d.customer_id, c.name AS customer_name,
                (SELECT COUNT(*) FROM deals d2 WHERE d2.customer_id = d.customer_id AND d2.deleted_at IS NULL)
                  AS customer_deal_count,
                (SELECT COUNT(*) FROM deals_images WHERE deal_id = d.id)
                  + (SELECT COUNT(*) FROM deals_documents WHERE deal_id = d.id) AS files_count,
                d.created_at
         FROM deals d
         JOIN deals_list dl ON dl.id = d.list_id
         JOIN customers c ON c.id = d.customer_id
         WHERE d.id = ? AND d.deleted_at IS NULL AND c.company_id = ?",
    )
    .bind(deal_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    Ok(deal.map(|mut deal| {
        deal.outcome = DealStatus::from_is_won(deal.is_won);
        deal
    }))
}

/// Id of a live deal of the company, for checking ids the client sends.
pub async fn deal_in_company(
    pool: &MySqlPool,
    company_id: i32,
    deal_id: i64,
) -> Result<bool, sqlx::Error> {
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM deals d JOIN customers c ON c.id = d.customer_id
         WHERE d.id = ? AND c.company_id = ? AND d.deleted_at IS NULL",
    )
    .bind(deal_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    Ok(found.is_some())
}

pub async fn stage_history(pool: &MySqlPool, deal_id: i64) -> Result<Vec<StageVisit>, sqlx::Error> {
    sqlx::query_as(
        "SELECT list_id, entered_at, exited_at FROM deal_stage_history
         WHERE deal_id = ? ORDER BY entered_at, id",
    )
    .bind(deal_id)
    .fetch_all(pool)
    .await
}

const ACTIVITY_COLUMNS: &str = "id, CAST(deal_id AS SIGNED) AS deal_id, name,
    DATE_FORMAT(deadline, '%Y-%m-%dT%H:%i:%s') AS deadline, CAST(priority AS CHAR) AS priority,
    is_completed, completed_at, created_at, created_by";

pub async fn list_activities(
    pool: &MySqlPool,
    company_id: i32,
    deal_id: i64,
) -> Result<Vec<Activity>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {ACTIVITY_COLUMNS} FROM deal_activities
         WHERE deal_id = ? AND company_id = ? AND deleted_at IS NULL
         ORDER BY created_at DESC, id DESC"
    )))
    .bind(deal_id)
    .bind(company_id)
    .fetch_all(pool)
    .await
}

pub async fn find_activity(
    pool: &MySqlPool,
    company_id: i32,
    deal_id: i64,
    activity_id: i32,
) -> Result<Option<Activity>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {ACTIVITY_COLUMNS} FROM deal_activities
         WHERE id = ? AND deal_id = ? AND company_id = ? AND deleted_at IS NULL"
    )))
    .bind(activity_id)
    .bind(deal_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await
}

pub async fn activity_suggestions(
    pool: &MySqlPool,
    company_id: i32,
    user_name: &str,
) -> Result<Vec<ActivitySuggestion>, sqlx::Error> {
    sqlx::query_as(
        "SELECT TRIM(name) AS name, COUNT(*) AS uses
         FROM deal_activities
         WHERE company_id = ? AND created_by = ? AND deleted_at IS NULL AND TRIM(name) <> ''
         GROUP BY TRIM(name)
         ORDER BY uses DESC, MAX(created_at) DESC, name
         LIMIT 50",
    )
    .bind(company_id)
    .bind(user_name)
    .fetch_all(pool)
    .await
}

pub async fn list_notes(
    pool: &MySqlPool,
    company_id: i32,
    deal_id: i64,
) -> Result<Vec<Note>, sqlx::Error> {
    let mut notes: Vec<Note> = sqlx::query_as(
        "SELECT CAST(id AS SIGNED) AS id, CAST(deal_id AS SIGNED) AS deal_id, content,
                COALESCE(is_pinned, 0) <> 0 AS is_pinned, created_at, created_by
         FROM deal_notes
         WHERE deal_id = ? AND company_id = ? AND deleted_at IS NULL
         ORDER BY created_at DESC, id DESC",
    )
    .bind(deal_id)
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    if notes.is_empty() {
        return Ok(notes);
    }
    let ids: Vec<i64> = notes.iter().map(|note| note.id).collect();
    let mut query = QueryBuilder::<MySql>::new(
        "SELECT CAST(id AS SIGNED) AS id, CAST(note_id AS SIGNED) AS note_id, content, created_at, created_by
         FROM deal_note_comments WHERE deleted_at IS NULL AND company_id = ",
    );
    query.push_bind(company_id);
    push_in(&mut query, "note_id", &ids);
    query.push(" ORDER BY created_at, id");
    let comments: Vec<NoteComment> = query.build_query_as().fetch_all(pool).await?;
    for note in &mut notes {
        note.comments = comments
            .iter()
            .filter(|comment| comment.note_id == note.id)
            .cloned()
            .collect();
    }
    Ok(notes)
}

pub async fn customer_deals(
    pool: &MySqlPool,
    company_id: i32,
    user_id: i32,
    customer_id: Option<i32>,
    email: &str,
    name: &str,
) -> Result<Vec<CustomerDeal>, sqlx::Error> {
    let select =
        "SELECT CAST(d.id AS SIGNED) AS id, d.title, d.status, CAST(d.amount AS CHAR) AS amount,
                         dl.name AS list_name
                  FROM deals d
                  JOIN customers c ON c.id = d.customer_id
                  LEFT JOIN deals_list dl ON dl.id = d.list_id
                  WHERE d.deleted_at IS NULL AND c.company_id = ? AND c.deleted_at IS NULL";
    let order = " ORDER BY d.user_id = ? DESC, d.id DESC";
    let mut deals: Vec<CustomerDeal> = match customer_id {
        Some(customer_id) => {
            sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "{select} AND d.customer_id = ?{order}"
            )))
            .bind(company_id)
            .bind(customer_id)
            .bind(user_id)
            .fetch_all(pool)
            .await?
        }
        None => {
            sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "{select} AND (
                   (? <> '' AND EXISTS (SELECT 1 FROM customers_emails ce
                                        WHERE ce.customer_id = c.id AND LOWER(ce.email) = LOWER(?)))
                   OR (? <> '' AND LOWER(c.name) = LOWER(?))
                 ){order}"
            )))
            .bind(company_id)
            .bind(email)
            .bind(email)
            .bind(name)
            .bind(name)
            .bind(user_id)
            .fetch_all(pool)
            .await?
        }
    };
    if deals.is_empty() {
        return Ok(deals);
    }
    let ids: Vec<i64> = deals.iter().map(|deal| deal.id).collect();

    let mut query = QueryBuilder::<MySql>::new(
        "SELECT CAST(id AS SIGNED) AS id, CAST(deal_id AS SIGNED) AS deal_id, content, created_at
         FROM deal_notes WHERE deleted_at IS NULL AND company_id = ",
    );
    query.push_bind(company_id);
    push_in(&mut query, "deal_id", &ids);
    query.push(" ORDER BY is_pinned DESC, created_at DESC, id DESC");
    let notes: Vec<NotePreview> = query.build_query_as().fetch_all(pool).await?;

    let mut query = QueryBuilder::<MySql>::new(
        "SELECT id, CAST(deal_id AS SIGNED) AS deal_id, name,
                DATE_FORMAT(deadline, '%Y-%m-%dT%H:%i:%s') AS deadline,
                CAST(priority AS CHAR) AS priority, is_completed
         FROM deal_activities WHERE deleted_at IS NULL AND company_id = ",
    );
    query.push_bind(company_id);
    push_in(&mut query, "deal_id", &ids);
    query.push(" ORDER BY is_completed, deadline IS NULL, deadline, created_at DESC, id DESC");
    let activities: Vec<ActivityPreview> = query.build_query_as().fetch_all(pool).await?;

    for deal in &mut deals {
        deal.notes = notes
            .iter()
            .filter(|n| n.deal_id == deal.id)
            .take(2)
            .cloned()
            .collect();
        deal.activities = activities
            .iter()
            .filter(|a| a.deal_id == deal.id)
            .take(2)
            .cloned()
            .collect();
    }
    Ok(deals)
}

#[derive(FromRow)]
struct RecipientRow {
    email: String,
    name: Option<String>,
}

/// Emails of the caller's customers that have a deal in `list_id`.
pub async fn list_recipients(
    pool: &MySqlPool,
    user_id: i32,
    list_id: i32,
    include_won: bool,
    include_lost: bool,
) -> Result<Vec<Recipient>, sqlx::Error> {
    let outcome = match (include_won, include_lost) {
        (true, true) => "TRUE",
        (true, false) => "(d.is_won IS NULL OR d.is_won = 1)",
        (false, true) => "(d.is_won IS NULL OR d.is_won = 0)",
        (false, false) => "d.is_won IS NULL",
    };
    let rows: Vec<RecipientRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT DISTINCT ce.email, c.name
         FROM customers c
         JOIN customers_emails ce ON ce.customer_id = c.id
         JOIN deals d ON d.customer_id = c.id AND d.deleted_at IS NULL AND d.list_id = ? AND {outcome}
         WHERE c.sales_rep = ? AND c.deleted_at IS NULL AND ce.email IS NOT NULL AND TRIM(ce.email) <> ''
         ORDER BY ce.email, c.name"
    )))
    .bind(list_id)
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    let mut seen = HashSet::new();
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let email = row.email.trim().to_lowercase();
            if email.is_empty() || !seen.insert(email.clone()) {
                return None;
            }
            let name = row
                .name
                .map(|n| n.trim().to_string())
                .filter(|n| !n.is_empty());
            Some(Recipient { email, name })
        })
        .collect())
}

/// First live stage of a displayed group the company can use.
pub async fn first_list_of_group(
    pool: &MySqlPool,
    company_id: i32,
    group_id: i32,
) -> Result<Option<i32>, sqlx::Error> {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT dl.id FROM deals_list dl JOIN groups_list gl ON gl.id = dl.group_id
         WHERE dl.group_id = ? AND dl.deleted_at IS NULL AND gl.is_displayed = 1 AND {GROUP_VISIBLE}
         ORDER BY dl.position, dl.id LIMIT 1"
    )))
    .bind(group_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await
}
