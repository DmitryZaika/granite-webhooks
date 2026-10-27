use super::access::{Stage, Viewer};
use super::schemas::{
    BoardQuery, Calendar, ConvertedDeal, DealProject, LinkableSale, Person, Project, ProjectAction,
    ProjectAppointment, ProjectFile, ProjectNote, ProjectTask, ProjectWorkspace, ProjectsBoard,
    StageChange,
};
use super::service::{self, project_select, visible_project};
use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::{JsonBody, MultiQuery};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use sqlx::{MySql, MySqlConnection, QueryBuilder};

const PAGE: u32 = 100;

/// `SELECT` of [`ProjectTask`] from `project_activities t`.
pub const TASK_SELECT: &str = "SELECT t.id, t.project_id, t.name, t.assigned_user_id,
        au.name AS assigned_user_name,
        DATE_FORMAT(t.deadline, '%Y-%m-%dT%H:%i:%s') AS deadline, CAST(t.priority AS CHAR) AS priority,
        DATE_FORMAT(t.completed_at, '%Y-%m-%dT%H:%i:%sZ') AS completed_at,
        t.created_by, cu.name AS created_by_name,
        DATE_FORMAT(t.created_at, '%Y-%m-%dT%H:%i:%sZ') AS created_at
   FROM project_activities t
   LEFT JOIN users au ON au.id = t.assigned_user_id
   LEFT JOIN users cu ON cu.id = t.created_by";

async fn people(conn: &mut MySqlConnection, company_id: i32) -> sqlx::Result<Vec<Person>> {
    sqlx::query_as(
        "SELECT id, COALESCE(NULLIF(TRIM(name), ''), email) AS name FROM users
          WHERE company_id = ? AND is_deleted = 0 ORDER BY name, id",
    )
    .bind(company_id)
    .fetch_all(&mut *conn)
    .await
}

const APPOINTMENT_SELECT: &str = "SELECT e.id, e.project_id, cal.slug AS kind, e.title,
        DATE_FORMAT(e.start_date, '%Y-%m-%dT%H:%i:%sZ') AS start_date,
        COALESCE(e.unscheduled, 0) <> 0 AS unscheduled
   FROM events e
   JOIN calendars cal ON cal.id = e.calendar_id AND cal.deleted_at IS NULL
                     AND cal.slug IN ('templates', 'installations')";

/// The board
///
/// The caller's projects, as columns of the stages they work on: everything
/// for admins, Managers and Sales Managers; Awaiting Template to In
/// Fabrication for Office Managers; Templated onward for Shop Managers; their
/// own for sales reps.
#[crate::openapi::path(
    get,
    path = "/v1/projects",
    operation_id = "list_projects",
    tag = "projects",
    params(BoardQuery),
    responses(
        (status = 200, description = "The board", body = ProjectsBoard),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "The caller's position has no Projects board", body = ErrorBody),
        (status = 404, description = "Projects is off for the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn board(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    MultiQuery(query): MultiQuery<BoardQuery>,
) -> ApiResult<Json<ProjectsBoard>> {
    let viewer = Viewer::load(&state.pool, &user).await?;
    if !viewer.has_board() {
        return Err(ApiError::Denied(
            "Projects is for managers, sales, the office and the shop. Ask an admin to add your position.".into(),
        ));
    }
    let page = query.page.unwrap_or(1).clamp(1, 100_000);
    let mut sql = QueryBuilder::<MySql>::new(project_select());
    sql.push(" WHERE p.company_id = ")
        .push_bind(viewer.company_id);
    sql.push(format!(" AND {}", viewer.visible_sql(false)));
    if !query.archived {
        sql.push(" AND p.archived_at IS NULL");
    }
    sql.push(match query.view.as_deref() {
        Some("cancelled") => " AND (p.cancelled_at IS NOT NULL OR s.cancelled_date IS NOT NULL)",
        Some("completed") => {
            " AND p.cancelled_at IS NULL AND s.cancelled_date IS NULL
              AND s.installed_date IS NOT NULL AND s.paid_date IS NOT NULL"
        }
        _ => {
            " AND p.cancelled_at IS NULL AND s.cancelled_date IS NULL
              AND (s.installed_date IS NULL OR s.paid_date IS NULL)"
        }
    });
    match query.owner.as_deref().map(str::trim) {
        Some("unassigned") => {
            sql.push(" AND p.owner_id IS NULL");
        }
        Some(owner) => {
            if let Ok(owner) = owner.parse::<i32>() {
                sql.push(" AND p.owner_id = ").push_bind(owner);
            }
        }
        None => {}
    }
    if let Some(q) = query.q.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
        let term: String = q.chars().take(255).collect();
        let like = format!("%{}%", crate::sql::escape_like(&term));
        sql.push(" AND (p.title LIKE ")
            .push_bind(like.clone())
            .push(" OR c.name LIKE ")
            .push_bind(like.clone())
            .push(" OR p.project_address LIKE ")
            .push_bind(like)
            .push(")");
    }
    if query.hold {
        sql.push(" AND p.is_on_hold <> 0");
    }
    sql.push(format!(
        " ORDER BY p.position, p.id LIMIT {} OFFSET ",
        PAGE + 1
    ))
    .push_bind(i64::from(page - 1) * i64::from(PAGE));
    let mut projects: Vec<Project> = sql.build_query_as().fetch_all(&state.pool).await?;
    let has_next = projects.len() > PAGE as usize;
    projects.truncate(PAGE as usize);

    let mut conn = state.pool.acquire().await?;
    let owners = people(&mut conn, viewer.company_id).await?;
    let ids: Vec<i32> = projects.iter().map(|project| project.id).collect();
    let (tasks, appointments) = if ids.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        let mut tasks = QueryBuilder::<MySql>::new(TASK_SELECT);
        tasks
            .push(" WHERE t.company_id = ")
            .push_bind(viewer.company_id)
            .push(" AND t.deleted_at IS NULL AND t.completed_at IS NULL");
        crate::sql::push_in(&mut tasks, "t.project_id", &ids);
        tasks.push(
            " ORDER BY t.deadline IS NULL, t.deadline, FIELD(t.priority, 'high', 'medium', 'low'), t.id",
        );
        let tasks: Vec<ProjectTask> = tasks.build_query_as().fetch_all(&mut *conn).await?;
        let mut next = QueryBuilder::<MySql>::new(APPOINTMENT_SELECT);
        next.push(" AND cal.company_id = ")
            .push_bind(viewer.company_id)
            .push(" WHERE e.deleted_date IS NULL AND e.unscheduled = 0 AND e.start_date >= UTC_TIMESTAMP()");
        crate::sql::push_in(&mut next, "e.project_id", &ids);
        next.push(" ORDER BY e.start_date, e.id");
        let appointments: Vec<ProjectAppointment> =
            next.build_query_as().fetch_all(&mut *conn).await?;
        (tasks, appointments)
    };
    Ok(Json(ProjectsBoard {
        projects,
        stages: Stage::ALL
            .into_iter()
            .filter(|stage| viewer.stages.contains(stage) || viewer.own)
            .collect(),
        owners,
        appointments,
        tasks,
        permissions: viewer.permissions,
        page,
        has_next,
    }))
}

/// A project's page
///
/// The project, its tasks, notes, files, stage history, appointments and the
/// customer's transactions it may link to. Anyone with an open task on it may
/// open it too.
#[crate::openapi::path(
    get,
    path = "/v1/projects/{id}",
    operation_id = "get_project",
    tag = "projects",
    params(("id" = i32, Path, description = "Project id")),
    responses(
        (status = 200, description = "The project", body = ProjectWorkspace),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such project the caller may see, or Projects is off", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<ProjectWorkspace>> {
    let viewer = Viewer::load(&state.pool, &user).await?;
    let mut conn = state.pool.acquire().await?;
    let project = visible_project(&mut conn, &viewer, id).await?;
    let company = viewer.company_id;
    let task_sql = format!(
        "{TASK_SELECT} WHERE t.project_id = ? AND t.company_id = ? AND t.deleted_at IS NULL
          ORDER BY t.completed_at IS NOT NULL, t.deadline IS NULL, t.deadline, t.id"
    );
    let tasks: Vec<ProjectTask> = sqlx::query_as(sqlx::AssertSqlSafe(task_sql))
        .bind(id)
        .bind(company)
        .fetch_all(&mut *conn)
        .await?;
    let notes: Vec<ProjectNote> = sqlx::query_as(
        "SELECT n.id, n.content, n.created_by, u.name AS author,
                DATE_FORMAT(n.created_at, '%Y-%m-%dT%H:%i:%sZ') AS created_at
           FROM project_notes n LEFT JOIN users u ON u.id = n.created_by
          WHERE n.project_id = ? AND n.company_id = ? AND n.deleted_at IS NULL
          ORDER BY n.created_at DESC, n.id DESC",
    )
    .bind(id)
    .bind(company)
    .fetch_all(&mut *conn)
    .await?;
    let mut files: Vec<ProjectFile> = sqlx::query_as(
        "SELECT id, name, url, content_type, created_by,
                DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%sZ') AS created_at
           FROM project_files
          WHERE project_id = ? AND company_id = ? AND deleted_at IS NULL ORDER BY id DESC",
    )
    .bind(id)
    .bind(company)
    .fetch_all(&mut *conn)
    .await?;
    for file in &mut files {
        file.url = state
            .storage
            .readable_url(&file.url, Some(file.content_type.as_str()))
            .await;
    }
    let history: Vec<StageChange> = sqlx::query_as(
        "SELECT h.id, h.from_stage, h.to_stage, h.source_action, h.reason, u.name AS author,
                DATE_FORMAT(h.created_at, '%Y-%m-%dT%H:%i:%sZ') AS created_at
           FROM project_stage_history h LEFT JOIN users u ON u.id = h.actor_id
          WHERE h.project_id = ? AND h.company_id = ? ORDER BY h.id DESC LIMIT 100",
    )
    .bind(id)
    .bind(company)
    .fetch_all(&mut *conn)
    .await?;
    let sales: Vec<LinkableSale> = if project.sale_id.is_some() {
        Vec::new()
    } else {
        sqlx::query_as(
            "SELECT s.id, CAST(s.price AS DOUBLE) AS price, s.project_address,
                    DATE_FORMAT(s.installed_date, '%Y-%m-%dT%H:%i:%sZ') AS installed_date,
                    DATE_FORMAT(s.paid_date, '%Y-%m-%dT%H:%i:%sZ') AS paid_date
               FROM sales s
              WHERE s.company_id = ? AND s.customer_id = ? AND s.cancelled_date IS NULL
                AND NOT EXISTS (SELECT 1 FROM projects lp WHERE lp.sale_id = s.id)
              ORDER BY s.id DESC LIMIT 100",
        )
        .bind(company)
        .bind(project.customer_id)
        .fetch_all(&mut *conn)
        .await?
    };
    let calendars: Vec<Calendar> = sqlx::query_as(
        "SELECT id, slug, name FROM calendars
          WHERE company_id = ? AND deleted_at IS NULL AND slug IN ('templates', 'installations')
          ORDER BY slug = 'installations', id",
    )
    .bind(company)
    .fetch_all(&mut *conn)
    .await?;
    let appointment_sql = format!(
        "{APPOINTMENT_SELECT} AND cal.company_id = ?
          WHERE e.project_id = ? AND e.deleted_date IS NULL
          ORDER BY e.unscheduled DESC, e.start_date DESC, e.id DESC LIMIT 100"
    );
    let appointments: Vec<ProjectAppointment> =
        sqlx::query_as(sqlx::AssertSqlSafe(appointment_sql))
            .bind(company)
            .bind(id)
            .fetch_all(&mut *conn)
            .await?;
    Ok(Json(ProjectWorkspace {
        project,
        tasks,
        notes,
        files,
        history,
        sales,
        people: people(&mut conn, company).await?,
        calendars,
        appointments,
        permissions: viewer.permissions,
        user_id: viewer.user_id,
    }))
}

/// Act on a project
///
/// Moves it forward, links its transaction, confirms payment, picks the
/// owner, puts it on hold, cancels, archives, or corrects its stage or
/// payment with a reason. A stale `version` gets a 409 saying to refresh.
#[crate::openapi::path(
    post,
    path = "/v1/projects/{id}/actions",
    operation_id = "act_on_project",
    tag = "projects",
    params(("id" = i32, Path, description = "Project id")),
    request_body = ProjectAction,
    responses(
        (status = 200, description = "The project after the action", body = Project),
        (status = 400, description = "Bad date, missing reason, or a move with its own button", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "The caller's position can't do this", body = ErrorBody),
        (status = 404, description = "No such project the caller may see", body = ErrorBody),
        (status = 409, description = "Changed since the page loaded, or not possible now (the message says why)", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn act(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    JsonBody(action): JsonBody<ProjectAction>,
) -> ApiResult<Json<Project>> {
    let viewer = Viewer::load(&state.pool, &user).await?;
    service::apply(&state.pool, &viewer, id, action)
        .await
        .map(Json)
}

/// A deal's project
///
/// The won deal's project, if any, and whether the caller may start one.
#[crate::openapi::path(
    get,
    path = "/v1/deals/{id}/project",
    operation_id = "get_deal_project",
    tag = "projects",
    params(("id" = i64, Path, description = "Deal id")),
    responses(
        (status = 200, description = "The deal's project", body = DealProject),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "Projects is off for the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn deal_project(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<DealProject>> {
    let viewer = Viewer::load(&state.pool, &user).await?;
    let mut conn = state.pool.acquire().await?;
    let project_id = service::deal_project(&mut conn, viewer.company_id, id).await?;
    Ok(Json(DealProject {
        project_id,
        can_convert: viewer.permissions.convert,
    }))
}

/// Start a deal's project
///
/// Turns a won deal into a project (Awaiting Template). Won deals start one
/// on their own; this is for deals won before Projects was on. Answers the
/// existing project when the deal has one.
#[crate::openapi::path(
    post,
    path = "/v1/deals/{id}/project",
    operation_id = "create_deal_project",
    tag = "projects",
    params(("id" = i64, Path, description = "Deal id")),
    responses(
        (status = 200, description = "The deal's project", body = ConvertedDeal),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "The caller's position can't start projects", body = ErrorBody),
        (status = 404, description = "No such deal the caller may see, or Projects is off", body = ErrorBody),
        (status = 409, description = "The deal isn't won", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn convert_deal(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<ConvertedDeal>> {
    let viewer = Viewer::load(&state.pool, &user).await?;
    let (project_id, created) = service::convert_deal(&state.pool, &viewer, id).await?;
    Ok(Json(ConvertedDeal {
        project_id,
        created,
    }))
}
