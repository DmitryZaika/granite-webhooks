//! Project writes: starting a project from a won deal, linking its
//! transaction, and the stage and milestone actions.

use super::access::{STAGE_SQL, Stage, Viewer, check_transition};
use super::schemas::{Project, ProjectAction};
use crate::error::{ApiError, ApiResult};
use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use lambda_http::tracing;
use sqlx::{MySql, MySqlConnection, MySqlPool};

fn iso(column: &str) -> String {
    format!("DATE_FORMAT({column}, '%Y-%m-%dT%H:%i:%sZ')")
}

/// `SELECT` of [`Project`] from `projects p`; add the `WHERE`.
pub fn project_select() -> String {
    format!(
        "SELECT p.id, CAST(p.source_deal_id AS SIGNED) AS source_deal_id, p.customer_id,
                p.sale_id, p.sales_rep_id, p.owner_id, p.title,
                CAST(p.quoted_amount AS DOUBLE) AS quoted_amount, p.project_address,
                {STAGE_SQL} AS stage_code, p.position, p.is_on_hold <> 0 AS is_on_hold,
                {cancelled} AS cancelled_at, {templated} AS templated_at,
                {archived} AS archived_at, p.version, {created} AS created_at,
                {updated} AS updated_at, {installed} AS installed_at, {paid} AS paid_at,
                {sale_cancelled} AS sale_cancelled_at,
                CAST(s.price AS DOUBLE) AS transaction_price,
                COALESCE(c.name, '') AS customer_name, o.name AS owner_name,
                rep.name AS sales_rep_name,
                (EXISTS (SELECT 1 FROM slab_inventory si
                          WHERE si.sale_id = s.id AND si.deleted_at IS NULL)
                 AND NOT EXISTS (SELECT 1 FROM slab_inventory si
                                  WHERE si.sale_id = s.id AND si.deleted_at IS NULL
                                    AND si.cut_date IS NULL)) AS ready_to_install
           FROM projects p
           JOIN customers c ON c.id = p.customer_id AND c.company_id = p.company_id
           LEFT JOIN sales s ON s.id = p.sale_id AND s.company_id = p.company_id
           LEFT JOIN users o ON o.id = p.owner_id AND o.company_id = p.company_id
           LEFT JOIN users rep ON rep.id = p.sales_rep_id AND rep.company_id = p.company_id",
        cancelled = iso("p.cancelled_at"),
        templated = iso("p.templated_at"),
        archived = iso("p.archived_at"),
        created = iso("p.created_at"),
        updated = iso("p.updated_at"),
        installed = iso("s.installed_date"),
        paid = iso("s.paid_date"),
        sale_cancelled = iso("s.cancelled_date"),
    )
}

/// A project of the company, whoever may see it.
pub async fn load_project(
    conn: &mut MySqlConnection,
    company_id: i32,
    id: i32,
) -> ApiResult<Project> {
    let sql = format!("{} WHERE p.id = ? AND p.company_id = ?", project_select());
    sqlx::query_as::<_, Project>(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .bind(company_id)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or(ApiError::NotFound)
}

/// A project the caller may open (a task on it is enough); 404 otherwise.
pub async fn visible_project(
    conn: &mut MySqlConnection,
    viewer: &Viewer,
    id: i32,
) -> ApiResult<Project> {
    let sql = format!(
        "{} WHERE p.id = ? AND p.company_id = ? AND {}",
        project_select(),
        viewer.visible_sql(true)
    );
    sqlx::query_as::<_, Project>(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .bind(viewer.company_id)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or(ApiError::NotFound)
}

/// Locks the project for a change the page made at `version`.
async fn lock(
    conn: &mut MySqlConnection,
    viewer: &Viewer,
    id: i32,
    version: i32,
    allow_archived: bool,
) -> ApiResult<Project> {
    let row: Option<(i32, bool)> = sqlx::query_as(
        "SELECT version, archived_at IS NOT NULL FROM projects
          WHERE id = ? AND company_id = ? FOR UPDATE",
    )
    .bind(id)
    .bind(viewer.company_id)
    .fetch_optional(&mut *conn)
    .await?;
    let (current, archived) = row.ok_or(ApiError::NotFound)?;
    let project = visible_project(conn, viewer, id).await?;
    if current != version {
        return Err(ApiError::Conflict(
            "Someone changed this project. Refresh the page and try again.".into(),
        ));
    }
    if archived && !allow_archived {
        return Err(ApiError::Conflict(
            "Unarchive this project before changing it".into(),
        ));
    }
    Ok(project)
}

async fn insert_history(
    conn: &mut MySqlConnection,
    company_id: i32,
    project_id: i32,
    actor_id: i32,
    change: (Option<Stage>, Stage),
    source: &str,
    reason: Option<&str>,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO project_stage_history
            (project_id, company_id, from_stage, to_stage, actor_id, source_action, reason)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(project_id)
    .bind(company_id)
    .bind(change.0.map(Stage::as_str))
    .bind(change.1.as_str())
    .bind(actor_id)
    .bind(source)
    .bind(reason)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// A history line when the stage moved.
async fn add_history(
    conn: &mut MySqlConnection,
    viewer: &Viewer,
    project_id: i32,
    change: (Stage, Stage),
    source: &str,
    reason: Option<&str>,
) -> sqlx::Result<()> {
    if change.0 == change.1 {
        return Ok(());
    }
    insert_history(
        conn,
        viewer.company_id,
        project_id,
        viewer.user_id,
        (Some(change.0), change.1),
        source,
        reason,
    )
    .await
}

async fn bump(conn: &mut MySqlConnection, company_id: i32, id: i32) -> sqlx::Result<()> {
    sqlx::query("UPDATE projects SET version = version + 1 WHERE id = ? AND company_id = ?")
        .bind(id)
        .bind(company_id)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

async fn save_stage(
    conn: &mut MySqlConnection,
    viewer: &Viewer,
    project: &Project,
    stage: Stage,
    source: &str,
    reason: Option<&str>,
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE projects SET stage_code = ?, version = version + 1 WHERE id = ? AND company_id = ?",
    )
    .bind(stage.as_str())
    .bind(project.id)
    .bind(viewer.company_id)
    .execute(&mut *conn)
    .await?;
    add_history(
        conn,
        viewer,
        project.id,
        (project.stage_code, stage),
        source,
        reason,
    )
    .await
}

fn conflict(message: &str) -> ApiError {
    ApiError::Conflict(message.into())
}

/// A milestone date: `YYYY-MM-DD` (noon UTC, so it reads as that day across
/// the US) or an RFC 3339 time; never in the future.
fn milestone_date(raw: &str) -> ApiResult<NaiveDateTime> {
    let raw = raw.trim();
    let at = if raw.len() == 10 {
        NaiveDate::parse_from_str(raw, "%Y-%m-%d")
            .ok()
            .map(|date| date.and_time(NaiveTime::from_hms_opt(12, 0, 0).unwrap_or_default()))
    } else {
        DateTime::parse_from_rfc3339(raw)
            .ok()
            .map(|at| at.with_timezone(&Utc).naive_utc())
    }
    .ok_or_else(|| ApiError::BadRequest("Pick a valid date".into()))?;
    if at > Utc::now().naive_utc() + chrono::Duration::hours(12) {
        return Err(ApiError::BadRequest(
            "Pick a date that isn't in the future".into(),
        ));
    }
    Ok(at)
}

fn reason(raw: &str) -> ApiResult<String> {
    let reason = raw.trim();
    if reason.is_empty() {
        return Err(ApiError::BadRequest("Give a reason".into()));
    }
    Ok(reason.chars().take(1000).collect())
}

/// Marks the transaction installed; every active slab must be cut.
async fn set_installed(
    conn: &mut MySqlConnection,
    company_id: i32,
    sale_id: i32,
    at: NaiveDateTime,
) -> ApiResult<()> {
    let sale: Option<(bool, bool)> = sqlx::query_as(
        "SELECT installed_date IS NOT NULL, cancelled_date IS NOT NULL FROM sales
          WHERE id = ? AND company_id = ? FOR UPDATE",
    )
    .bind(sale_id)
    .bind(company_id)
    .fetch_optional(&mut *conn)
    .await?;
    let (installed, cancelled) = sale.ok_or(ApiError::NotFound)?;
    if cancelled {
        return Err(conflict("The transaction is cancelled"));
    }
    if installed {
        return Err(conflict("The transaction is already installed"));
    }
    let (slabs, uncut): (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), CAST(COALESCE(SUM(cut_date IS NULL), 0) AS SIGNED)
           FROM slab_inventory WHERE sale_id = ? AND deleted_at IS NULL",
    )
    .bind(sale_id)
    .fetch_one(&mut *conn)
    .await?;
    if slabs == 0 {
        return Err(conflict(
            "The transaction has no slabs yet. Add them before installing.",
        ));
    }
    if uncut > 0 {
        return Err(conflict(&format!(
            "{uncut} slab(s) of the transaction aren't cut yet. Cut them before installing."
        )));
    }
    sqlx::query("UPDATE sales SET installed_date = ? WHERE id = ? AND company_id = ?")
        .bind(at)
        .bind(sale_id)
        .bind(company_id)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Confirms full payment on the transaction.
async fn set_paid(
    conn: &mut MySqlConnection,
    company_id: i32,
    sale_id: i32,
    at: NaiveDateTime,
) -> ApiResult<()> {
    let sale: Option<(Option<String>, bool)> = sqlx::query_as(
        "SELECT DATE_FORMAT(paid_date, '%b %e, %Y'), cancelled_date IS NOT NULL FROM sales
          WHERE id = ? AND company_id = ? FOR UPDATE",
    )
    .bind(sale_id)
    .bind(company_id)
    .fetch_optional(&mut *conn)
    .await?;
    let (paid, cancelled) = sale.ok_or(ApiError::NotFound)?;
    if cancelled {
        return Err(conflict("The transaction is cancelled"));
    }
    if let Some(paid) = paid {
        return Err(conflict(&format!(
            "Payment was already confirmed on {paid}. Use Correct payment to change the date."
        )));
    }
    sqlx::query("UPDATE sales SET paid_date = ? WHERE id = ? AND company_id = ?")
        .bind(at)
        .bind(sale_id)
        .bind(company_id)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

async fn require_colleague(
    conn: &mut MySqlConnection,
    company_id: i32,
    user_id: i32,
) -> ApiResult<()> {
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT id FROM users WHERE id = ? AND company_id = ? AND is_deleted = 0",
    )
    .bind(user_id)
    .bind(company_id)
    .fetch_optional(&mut *conn)
    .await?;
    found
        .map(|_| ())
        .ok_or_else(|| ApiError::BadRequest("Pick someone in your company".into()))
}

/// One project action (`POST /v1/projects/{id}/actions`); the project after it.
pub async fn apply(
    pool: &MySqlPool,
    viewer: &Viewer,
    id: i32,
    action: ProjectAction,
) -> ApiResult<Project> {
    let mut tx = pool.begin().await?;
    let allow_archived = matches!(
        action,
        ProjectAction::Assign { .. } | ProjectAction::Hold { .. } | ProjectAction::Archive { .. }
    );
    let p = lock(&mut tx, viewer, id, action.version(), allow_archived).await?;
    let company = viewer.company_id;
    let perms = viewer.permissions;
    let cancelled = p.cancelled_at.is_some() || p.sale_cancelled_at.is_some();
    match action {
        ProjectAction::Stage { stage: to, .. } => {
            Viewer::require(perms.production)?;
            if cancelled {
                return Err(conflict("This project is cancelled"));
            }
            if p.is_on_hold {
                return Err(conflict("Take this project off hold before moving it"));
            }
            check_transition(p.stage_code, to)?;
            if p.stage_code != to {
                if to >= Stage::Fabrication && p.sale_id.is_none() {
                    return Err(conflict("Link the project's transaction first"));
                }
                match to {
                    Stage::Templated => {
                        sqlx::query(
                            "UPDATE projects SET templated_at = COALESCE(templated_at, UTC_TIMESTAMP())
                              WHERE id = ? AND company_id = ?",
                        )
                        .bind(id)
                        .bind(company)
                        .execute(&mut *tx)
                        .await?;
                    }
                    Stage::Installed => {
                        let sale = p.sale_id.unwrap_or_default();
                        set_installed(&mut tx, company, sale, Utc::now().naive_utc()).await?;
                    }
                    Stage::Completed => {
                        if p.installed_at.is_none() || p.paid_at.is_none() {
                            return Err(conflict(
                                "Confirm the installation and the full payment first",
                            ));
                        }
                    }
                    Stage::New | Stage::Fabrication => {}
                }
                let stored = if to == Stage::Installed && p.paid_at.is_some() {
                    Stage::Completed
                } else {
                    to
                };
                save_stage(&mut tx, viewer, &p, stored, "stage-change", None).await?;
            }
        }
        ProjectAction::LinkSale { sale_id, .. } => {
            Viewer::require(perms.manage || perms.production || perms.mark_paid)?;
            if cancelled {
                return Err(conflict("A cancelled project can't link a transaction"));
            }
            match p.sale_id {
                Some(current) if current == sale_id => {}
                Some(_) => return Err(conflict("This project has a transaction already")),
                None => {
                    let sale: Option<(i32, bool)> = sqlx::query_as(
                        "SELECT customer_id, cancelled_date IS NOT NULL FROM sales
                          WHERE id = ? AND company_id = ? FOR UPDATE",
                    )
                    .bind(sale_id)
                    .bind(company)
                    .fetch_optional(&mut *tx)
                    .await?;
                    let (customer, sale_cancelled) = sale.ok_or(ApiError::NotFound)?;
                    if customer != p.customer_id || sale_cancelled {
                        return Err(conflict(
                            "Pick an active transaction of this project's customer",
                        ));
                    }
                    sqlx::query(
                        "UPDATE projects SET sale_id = ?, version = version + 1
                          WHERE id = ? AND company_id = ?",
                    )
                    .bind(sale_id)
                    .bind(id)
                    .bind(company)
                    .execute(&mut *tx)
                    .await
                    .map_err(|error| {
                        unique(error, "That transaction belongs to another project")
                    })?;
                    let linked = load_project(&mut tx, company, id).await?;
                    add_history(
                        &mut tx,
                        viewer,
                        id,
                        (p.stage_code, linked.stage_code),
                        "link-transaction",
                        None,
                    )
                    .await?;
                }
            }
        }
        ProjectAction::Paid { date, .. } => {
            Viewer::require(perms.mark_paid)?;
            if cancelled {
                return Err(conflict("This project is cancelled"));
            }
            let sale = p
                .sale_id
                .ok_or_else(|| conflict("Link the transaction before confirming payment"))?;
            set_paid(&mut tx, company, sale, milestone_date(&date)?).await?;
            bump(&mut tx, company, id).await?;
            let now = load_project(&mut tx, company, id).await?;
            add_history(
                &mut tx,
                viewer,
                id,
                (p.stage_code, now.stage_code),
                "payment-confirmed",
                None,
            )
            .await?;
        }
        ProjectAction::Assign { owner_id, .. } => {
            Viewer::require(perms.manage)?;
            if let Some(owner) = owner_id {
                require_colleague(&mut tx, company, owner).await?;
            }
            sqlx::query(
                "UPDATE projects SET owner_id = ?, version = version + 1
                  WHERE id = ? AND company_id = ?",
            )
            .bind(owner_id)
            .bind(id)
            .bind(company)
            .execute(&mut *tx)
            .await?;
        }
        ProjectAction::Hold { on_hold, .. } => {
            Viewer::require(perms.manage)?;
            sqlx::query(
                "UPDATE projects SET is_on_hold = ?, version = version + 1
                  WHERE id = ? AND company_id = ?",
            )
            .bind(on_hold)
            .bind(id)
            .bind(company)
            .execute(&mut *tx)
            .await?;
        }
        ProjectAction::Archive { archived, .. } => {
            Viewer::require(perms.manage)?;
            if archived && p.stage_code != Stage::Completed && !cancelled {
                return Err(conflict("Finish or cancel the project before archiving it"));
            }
            sqlx::query(
                "UPDATE projects SET archived_at = IF(?, UTC_TIMESTAMP(), NULL),
                        version = version + 1
                  WHERE id = ? AND company_id = ?",
            )
            .bind(archived)
            .bind(id)
            .bind(company)
            .execute(&mut *tx)
            .await?;
        }
        ProjectAction::Cancel { reason: raw, .. } => {
            Viewer::require(perms.manage)?;
            let why = reason(&raw)?;
            if p.cancelled_at.is_none() {
                sqlx::query(
                    "UPDATE projects SET cancelled_at = UTC_TIMESTAMP(), version = version + 1
                      WHERE id = ? AND company_id = ?",
                )
                .bind(id)
                .bind(company)
                .execute(&mut *tx)
                .await?;
                insert_history(
                    &mut tx,
                    company,
                    id,
                    viewer.user_id,
                    (Some(p.stage_code), p.stage_code),
                    "cancelled",
                    Some(&why),
                )
                .await?;
            }
        }
        ProjectAction::CorrectPayment {
            date, reason: raw, ..
        } => {
            Viewer::require(perms.mark_paid)?;
            let why = reason(&raw)?;
            if cancelled {
                return Err(conflict("This project is cancelled"));
            }
            let sale = p
                .sale_id
                .ok_or_else(|| conflict("Link the transaction before correcting payment"))?;
            let at = date.as_deref().map(milestone_date).transpose()?;
            sqlx::query("UPDATE sales SET paid_date = ? WHERE id = ? AND company_id = ?")
                .bind(at)
                .bind(sale)
                .bind(company)
                .execute(&mut *tx)
                .await?;
            bump(&mut tx, company, id).await?;
            let now = load_project(&mut tx, company, id).await?;
            if now.stage_code == p.stage_code {
                sqlx::query(
                    "INSERT INTO project_notes (project_id, company_id, content, created_by)
                     VALUES (?, ?, ?, ?)",
                )
                .bind(id)
                .bind(company)
                .bind(format!("Payment correction: {why}"))
                .bind(viewer.user_id)
                .execute(&mut *tx)
                .await?;
            } else {
                add_history(
                    &mut tx,
                    viewer,
                    id,
                    (p.stage_code, now.stage_code),
                    "payment-correction",
                    Some(&why),
                )
                .await?;
            }
        }
        ProjectAction::Correct {
            stage: to,
            reason: raw,
            ..
        } => {
            Viewer::require(perms.manage)?;
            let why = reason(&raw)?;
            if cancelled {
                return Err(conflict("This project is cancelled"));
            }
            if to >= Stage::Installed {
                return Err(ApiError::BadRequest(
                    "Installation and payment are confirmed with their own buttons".into(),
                ));
            }
            if to == Stage::Fabrication && p.sale_id.is_none() {
                return Err(conflict("Link the project's transaction first"));
            }
            if let (Some(sale), Some(_)) = (p.sale_id, &p.installed_at) {
                sqlx::query(
                    "UPDATE sales SET installed_date = NULL WHERE id = ? AND company_id = ?",
                )
                .bind(sale)
                .bind(company)
                .execute(&mut *tx)
                .await?;
            }
            sqlx::query(
                "UPDATE projects
                    SET templated_at = IF(? = 'new', NULL, COALESCE(templated_at, UTC_TIMESTAMP()))
                  WHERE id = ? AND company_id = ?",
            )
            .bind(to.as_str())
            .bind(id)
            .bind(company)
            .execute(&mut *tx)
            .await?;
            save_stage(&mut tx, viewer, &p, to, "manager-correction", Some(&why)).await?;
        }
    }
    let project = load_project(&mut tx, company, id).await?;
    tx.commit().await?;
    Ok(project)
}

fn unique(error: sqlx::Error, message: &str) -> ApiError {
    match &error {
        sqlx::Error::Database(db) if db.is_unique_violation() => conflict(message),
        _ => ApiError::from(error),
    }
}

/// The company has Projects on.
pub async fn enabled<'e>(
    executor: impl sqlx::Executor<'e, Database = MySql>,
    company_id: i32,
) -> sqlx::Result<bool> {
    let on: Option<bool> = sqlx::query_scalar(
        "SELECT COALESCE(projects_enabled, 0) <> 0 FROM company WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(company_id)
    .fetch_optional(executor)
    .await?;
    Ok(on == Some(true))
}

/// The deal's project id.
pub async fn deal_project(
    conn: &mut MySqlConnection,
    company_id: i32,
    deal_id: i64,
) -> sqlx::Result<Option<i32>> {
    sqlx::query_scalar("SELECT id FROM projects WHERE company_id = ? AND source_deal_id = ?")
        .bind(company_id)
        .bind(deal_id)
        .fetch_optional(&mut *conn)
        .await
}

/// Starts the project of won deal `deal_id`: Awaiting Template, with the
/// waiting template and installation events of the deal's sold quotes.
async fn create_project(
    conn: &mut MySqlConnection,
    company_id: i32,
    deal_id: i64,
    actor_id: Option<i32>,
) -> ApiResult<i32> {
    let result = sqlx::query(
        "INSERT INTO projects (company_id, source_deal_id, customer_id, sales_rep_id, title,
                               quoted_amount, project_address, created_by)
         SELECT c.company_id, d.id, d.customer_id, d.user_id,
                LEFT(COALESCE(NULLIF(TRIM(d.title), ''),
                              CONCAT(COALESCE(NULLIF(TRIM(c.name), ''), 'Customer'), ' project')), 255),
                d.amount, NULLIF(TRIM(c.address), ''), COALESCE(?, d.user_id)
           FROM deals d JOIN customers c ON c.id = d.customer_id
          WHERE d.id = ? AND c.company_id = ? AND COALESCE(?, d.user_id) IS NOT NULL",
    )
    .bind(actor_id)
    .bind(deal_id)
    .bind(company_id)
    .bind(actor_id)
    .execute(&mut *conn)
    .await
    .map_err(|error| unique(error, "This deal has a project already"))?;
    if result.rows_affected() == 0 {
        return Err(conflict("The deal has no sales rep to start its project"));
    }
    let id = i32::try_from(result.last_insert_id())
        .map_err(|_| ApiError::Internal("project id".into()))?;
    let creator: i32 = sqlx::query_scalar("SELECT created_by FROM projects WHERE id = ?")
        .bind(id)
        .fetch_one(&mut *conn)
        .await?;
    insert_history(
        conn,
        company_id,
        id,
        creator,
        (None, Stage::New),
        "deal-handoff",
        None,
    )
    .await?;
    sqlx::query(
        "UPDATE events e JOIN quotes q ON q.id = e.quote_id
            SET e.project_id = ?
          WHERE q.deal_id = ? AND q.company_id = ? AND e.project_id IS NULL
            AND e.deleted_date IS NULL",
    )
    .bind(id)
    .bind(deal_id)
    .bind(company_id)
    .execute(&mut *conn)
    .await?;
    Ok(id)
}

/// `POST /v1/deals/{id}/project`: the deal's project, made now if needed.
pub async fn convert_deal(
    pool: &MySqlPool,
    viewer: &Viewer,
    deal_id: i64,
) -> ApiResult<(i32, bool)> {
    Viewer::require(viewer.permissions.convert)?;
    let mut tx = pool.begin().await?;
    let deal: Option<(Option<i32>, Option<i64>)> = sqlx::query_as(
        "SELECT d.user_id, CAST(d.is_won AS SIGNED) FROM deals d
           JOIN customers c ON c.id = d.customer_id
          WHERE d.id = ? AND c.company_id = ? AND d.deleted_at IS NULL AND c.deleted_at IS NULL
          FOR UPDATE",
    )
    .bind(deal_id)
    .bind(viewer.company_id)
    .fetch_optional(&mut *tx)
    .await?;
    // Sales reps convert their own deals.
    let (_, is_won) = deal
        .filter(|(owner, _)| viewer.permissions.view_all || *owner == Some(viewer.user_id))
        .ok_or(ApiError::NotFound)?;
    if let Some(id) = deal_project(&mut tx, viewer.company_id, deal_id).await? {
        return Ok((id, false));
    }
    if is_won != Some(1) {
        return Err(conflict("Mark the deal won first"));
    }
    let id = create_project(&mut tx, viewer.company_id, deal_id, Some(viewer.user_id)).await?;
    tx.commit().await?;
    Ok((id, true))
}

/// A deal was won (deals board, sold quote): its project starts, when the
/// company has Projects. Logged, never returned: the win is already saved.
pub async fn start_for_won_deal(
    pool: &MySqlPool,
    company_id: i32,
    deal_id: i64,
    actor_id: Option<i32>,
) {
    if let Err(error) = try_start(pool, company_id, deal_id, actor_id).await {
        tracing::error!(?error, deal_id, "could not start the won deal's project");
    }
}

async fn try_start(
    pool: &MySqlPool,
    company_id: i32,
    deal_id: i64,
    actor_id: Option<i32>,
) -> ApiResult<()> {
    if !enabled(pool, company_id).await? {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    let won: Option<bool> = sqlx::query_scalar(
        "SELECT COALESCE(d.is_won, 0) = 1 FROM deals d JOIN customers c ON c.id = d.customer_id
          WHERE d.id = ? AND c.company_id = ? AND d.deleted_at IS NULL FOR UPDATE",
    )
    .bind(deal_id)
    .bind(company_id)
    .fetch_optional(&mut *tx)
    .await?;
    if won != Some(true) || deal_project(&mut tx, company_id, deal_id).await?.is_some() {
        return Ok(());
    }
    create_project(&mut tx, company_id, deal_id, actor_id).await?;
    tx.commit().await?;
    Ok(())
}

/// A transaction was made for the customer: it becomes the transaction of
/// their newest open project that has none. Logged, never returned.
pub async fn link_new_sale(
    conn: &mut MySqlConnection,
    company_id: i32,
    customer_id: i32,
    sale_id: i32,
) {
    if let Err(error) = try_link(conn, company_id, customer_id, sale_id).await {
        tracing::error!(
            ?error,
            sale_id,
            "could not link the new transaction to a project"
        );
    }
}

async fn try_link(
    conn: &mut MySqlConnection,
    company_id: i32,
    customer_id: i32,
    sale_id: i32,
) -> sqlx::Result<()> {
    if !enabled(&mut *conn, company_id).await? {
        return Ok(());
    }
    sqlx::query(
        "UPDATE projects SET sale_id = ?, version = version + 1
          WHERE company_id = ? AND customer_id = ? AND sale_id IS NULL
            AND cancelled_at IS NULL AND archived_at IS NULL
          ORDER BY id DESC LIMIT 1",
    )
    .bind(sale_id)
    .bind(company_id)
    .bind(customer_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::milestone_date;

    #[test]
    fn milestone_dates() {
        assert_eq!(
            milestone_date("2026-01-05").unwrap().to_string(),
            "2026-01-05 12:00:00"
        );
        assert_eq!(
            milestone_date("2026-01-05T15:30:00Z").unwrap().to_string(),
            "2026-01-05 15:30:00"
        );
        assert!(milestone_date("2026-13-05").is_err());
        assert!(milestone_date("2999-01-01").is_err());
    }
}
