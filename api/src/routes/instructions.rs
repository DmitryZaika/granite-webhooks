//! The company's instructions (how-to pages), kept as a tree whose siblings
//! are chained by `after_id`.
//!
//! - `GET    /v1/instructions`       → the tree's rows the caller may read
//! - `POST   /v1/instructions`       → add one
//! - `PUT    /v1/instructions/{id}`  → edit one
//! - `DELETE /v1/instructions/{id}`  → delete one
//! - `PUT    /v1/instructions/order` → save a drag-and-drop order of siblings
//!
//! Replaces the Remix `api.instructions.reorder` route and the
//! `employee.instructions` and `admin.instructions*` loaders and actions.

use crate::auth::session::SessionUser;
use crate::auth::{AdminUser, EmployeeUser};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::{JsonBody, MultiQuery};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use sqlx::MySqlPool;
use std::collections::{HashMap, HashSet};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};

/// Positions an instruction can be limited to (all but check-in and super
/// admin: `INSTRUCTION_ACCESS_POSITION_IDS`).
const ACCESS_POSITIONS: [i32; 9] = [1, 2, 3, 4, 5, 6, 7, 10, 11];

/// The AI follow-up settings live in `instructions` but are not shown in the tree.
const AI_FOLLOWUP_TITLE: &str = "AI follow-up instruction";

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list, create))
        .routes(routes!(update, delete))
        .routes(routes!(order))
}

/// Which instructions to reorder.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum InstructionScope {
    /// The caller's company.
    #[default]
    Company,
    /// Shared by every company (super admins only; others get `company`).
    General,
}

/// Body of `PUT /v1/instructions/order`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct InstructionOrder {
    /// The parent whose children are reordered; null for the top level.
    pub parent_id: Option<i32>,
    /// Every child of that parent, in the new order.
    pub ordered_ids: Vec<i32>,
    /// `company` (default) or `general`.
    #[serde(default)]
    pub scope: InstructionScope,
}

/// Order instructions
///
/// Saves the order of one parent's children; the list must hold each of
/// them exactly once.
#[utoipa::path(
    put,
    path = "/v1/instructions/order",
    operation_id = "order_instructions",
    tag = "instructions",
    request_body = InstructionOrder,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Empty, repeated, missing or foreign ids", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn order(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<InstructionOrder>,
) -> ApiResult<StatusCode> {
    let company_id = if user.is_superuser && input.scope == InstructionScope::General {
        0
    } else {
        user.company_id
    };
    let ordered = &input.ordered_ids;
    let unique: HashSet<i32> = ordered.iter().copied().collect();
    if ordered.is_empty() || unique.len() != ordered.len() {
        return Err(ApiError::BadRequest(
            "List every instruction of the parent once".into(),
        ));
    }
    let siblings: HashSet<i32> = sqlx::query_scalar(
        "SELECT id FROM instructions
          WHERE company_id = ? AND (title IS NULL OR title <> ?) AND parent_id <=> ?",
    )
    .bind(company_id)
    .bind(AI_FOLLOWUP_TITLE)
    .bind(input.parent_id)
    .fetch_all(&state.pool)
    .await?
    .into_iter()
    .collect();
    if siblings != unique {
        return Err(ApiError::BadRequest(
            "List every instruction of the parent once".into(),
        ));
    }
    let mut tx = state.pool.begin().await?;
    let mut after = None;
    for id in ordered {
        sqlx::query("UPDATE instructions SET after_id = ? WHERE id = ? AND company_id = ?")
            .bind(after)
            .bind(id)
            .bind(company_id)
            .execute(&mut *tx)
            .await?;
        after = Some(*id);
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Query of `GET /v1/instructions`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListQuery {
    /// `company` (default) or `general` (shared by every company).
    #[serde(default)]
    #[param(inline)]
    pub scope: InstructionScope,
    /// Admins: every row with its access, for editing (general rows only for
    /// super admins). Otherwise only the rows the caller's positions open.
    #[serde(default)]
    pub manage: bool,
}

/// One instruction of the tree.
#[derive(Debug, Clone, Serialize, sqlx::FromRow, ToSchema)]
pub struct Instruction {
    /// Instruction id.
    pub id: i32,
    /// Title.
    pub title: Option<String>,
    /// Parent; null at the top level.
    pub parent_id: Option<i32>,
    /// The sibling it follows; null for the first.
    pub after_id: Option<i32>,
    /// Body HTML.
    pub rich_text: Option<String>,
    /// Positions that may read it; empty for everyone.
    #[sqlx(skip)]
    pub position_ids: Vec<i32>,
}

/// Body of `POST /v1/instructions` and `PUT /v1/instructions/{id}`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct InstructionInput {
    /// Title (required).
    pub title: String,
    /// Parent; null at the top level.
    #[serde(default)]
    pub parent_id: Option<i32>,
    /// The sibling it follows; null to come first.
    #[serde(default)]
    pub after_id: Option<i32>,
    /// Body HTML.
    #[serde(default)]
    pub rich_text: String,
    /// Positions that may read it (empty for everyone); others are ignored.
    #[serde(default)]
    pub position_ids: Vec<i32>,
}

/// `{ id }` of a new instruction.
#[derive(Debug, Serialize, ToSchema)]
pub struct CreatedInstruction {
    /// The new instruction.
    pub id: i32,
}

fn scope_company(user: &SessionUser, scope: InstructionScope) -> i32 {
    if scope == InstructionScope::General {
        0
    } else {
        user.company_id
    }
}

async fn access_map(pool: &MySqlPool, ids: &[i32]) -> Result<HashMap<i32, Vec<i32>>, sqlx::Error> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let marks = vec!["?"; ids.len()].join(", ");
    let sql = format!(
        "SELECT instruction_id, position_id FROM instruction_access WHERE instruction_id IN ({marks})"
    );
    let mut query = sqlx::query_as::<_, (i32, i32)>(sqlx::AssertSqlSafe(sql));
    for id in ids {
        query = query.bind(id);
    }
    let mut map: HashMap<i32, Vec<i32>> = HashMap::new();
    for (instruction, position) in query.fetch_all(pool).await? {
        map.entry(instruction).or_default().push(position);
    }
    Ok(map)
}

/// List instructions
///
/// The rows of the company's (or the general) instruction tree; the client
/// builds the tree from `parent_id` and `after_id`. An instruction limited to
/// some positions is left out unless the caller holds one of them, except
/// with `manage` (admins), which returns every row with its positions.
#[utoipa::path(
    get,
    path = "/v1/instructions",
    operation_id = "list_instructions",
    tag = "instructions",
    params(ListQuery),
    responses(
        (status = 200, description = "Instructions", body = Vec<Instruction>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee, or `manage` without being an admin (`general`: super admin)", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    MultiQuery(query): MultiQuery<ListQuery>,
) -> ApiResult<Json<Vec<Instruction>>> {
    if query.manage {
        let allowed = if query.scope == InstructionScope::General {
            user.is_superuser
        } else {
            user.is_admin || user.is_superuser
        };
        if !allowed {
            return Err(ApiError::Forbidden);
        }
    }
    let pool = &state.pool;
    let mut rows: Vec<Instruction> = sqlx::query_as(
        "SELECT id, title, parent_id, after_id, rich_text FROM instructions WHERE company_id = ?",
    )
    .bind(scope_company(&user, query.scope))
    .fetch_all(pool)
    .await?;
    let ids: Vec<i32> = rows.iter().map(|row| row.id).collect();
    let mut access = access_map(pool, &ids).await?;
    for row in &mut rows {
        row.position_ids = access.remove(&row.id).unwrap_or_default();
    }
    if !query.manage {
        let held: HashSet<i32> = sqlx::query_scalar(
            "SELECT DISTINCT position_id FROM users_positions WHERE user_id = ? AND company_id = ?",
        )
        .bind(user.id)
        .bind(user.company_id)
        .fetch_all(pool)
        .await?
        .into_iter()
        .collect();
        rows.retain(|row| {
            row.position_ids.is_empty() || row.position_ids.iter().any(|id| held.contains(id))
        });
    }
    Ok(Json(rows))
}

fn checked(input: &InstructionInput) -> ApiResult<(String, Vec<i32>)> {
    let title = input.title.trim();
    if title.is_empty() {
        return Err(ApiError::BadRequest("Title is required".into()));
    }
    let mut positions: Vec<i32> = input
        .position_ids
        .iter()
        .copied()
        .filter(|id| ACCESS_POSITIONS.contains(id))
        .collect();
    positions.sort_unstable();
    positions.dedup();
    Ok((title.to_string(), positions))
}

/// Parent and previous sibling must be instructions of the same tree.
async fn check_links(pool: &MySqlPool, company_id: i32, input: &InstructionInput) -> ApiResult<()> {
    for id in [input.parent_id, input.after_id].into_iter().flatten() {
        let found: Option<i32> =
            sqlx::query_scalar("SELECT id FROM instructions WHERE id = ? AND company_id = ?")
                .bind(id)
                .bind(company_id)
                .fetch_optional(pool)
                .await?;
        if found.is_none() {
            return Err(ApiError::BadRequest(format!(
                "No instruction {id} to link to"
            )));
        }
    }
    Ok(())
}

async fn replace_access(
    tx: &mut sqlx::MySqlConnection,
    id: i32,
    positions: &[i32],
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM instruction_access WHERE instruction_id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    for position in positions {
        sqlx::query("INSERT INTO instruction_access (instruction_id, position_id) VALUES (?, ?)")
            .bind(id)
            .bind(position)
            .execute(&mut *tx)
            .await?;
    }
    Ok(())
}

/// The tree an admin may change an instruction of: the company's, or the
/// general one for super admins.
async fn editable_company(pool: &MySqlPool, user: &SessionUser, id: i32) -> ApiResult<i32> {
    let company: Option<Option<i32>> =
        sqlx::query_scalar("SELECT company_id FROM instructions WHERE id = ?")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    match company.flatten() {
        Some(company) if company == user.company_id => Ok(company),
        Some(0) if user.is_superuser => Ok(0),
        _ => Err(ApiError::NotFound),
    }
}

/// Add an instruction
///
/// Puts it in the caller's company tree under `parent_id`, right after
/// `after_id` (the sibling that followed `after_id` now follows it).
#[utoipa::path(
    post,
    path = "/v1/instructions",
    operation_id = "create_instruction",
    tag = "instructions",
    request_body = InstructionInput,
    responses(
        (status = 201, description = "Added", body = CreatedInstruction),
        (status = 400, description = "No title, or a parent or sibling outside the tree", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<InstructionInput>,
) -> ApiResult<(StatusCode, Json<CreatedInstruction>)> {
    let (title, positions) = checked(&input)?;
    let company = user.company_id;
    check_links(&state.pool, company, &input).await?;
    let mut tx = state.pool.begin().await?;
    let id = sqlx::query(
        "INSERT INTO instructions (title, parent_id, after_id, rich_text, company_id)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&title)
    .bind(input.parent_id)
    .bind(input.after_id)
    .bind(&input.rich_text)
    .bind(company)
    .execute(&mut *tx)
    .await?
    .last_insert_id();
    let id = i32::try_from(id).unwrap_or(i32::MAX);
    replace_access(&mut tx, id, &positions).await?;
    sqlx::query(
        "UPDATE instructions SET after_id = ?
          WHERE after_id <=> ? AND id <> ? AND parent_id <=> ? AND company_id = ?",
    )
    .bind(id)
    .bind(input.after_id)
    .bind(id)
    .bind(input.parent_id)
    .bind(company)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(CreatedInstruction { id })))
}

/// Edit an instruction
///
/// Saves its title, place (`parent_id`, `after_id`), text and the positions
/// that may read it.
#[utoipa::path(
    put,
    path = "/v1/instructions/{id}",
    operation_id = "update_instruction",
    tag = "instructions",
    params(("id" = i32, Path, description = "Instruction id")),
    request_body = InstructionInput,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "No title, or a parent or sibling outside the tree", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such instruction the caller may edit", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<InstructionInput>,
) -> ApiResult<StatusCode> {
    let (title, positions) = checked(&input)?;
    let company = editable_company(&state.pool, &user, id).await?;
    check_links(&state.pool, company, &input).await?;
    let mut tx = state.pool.begin().await?;
    sqlx::query(
        "UPDATE instructions SET title = ?, parent_id = ?, after_id = ?, rich_text = ? WHERE id = ?",
    )
    .bind(&title)
    .bind(input.parent_id)
    .bind(input.after_id)
    .bind(&input.rich_text)
    .bind(id)
    .execute(&mut *tx)
    .await?;
    replace_access(&mut tx, id, &positions).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Delete an instruction
///
/// Removes it from its tree.
#[utoipa::path(
    delete,
    path = "/v1/instructions/{id}",
    operation_id = "delete_instruction",
    tag = "instructions",
    params(("id" = i32, Path, description = "Instruction id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such instruction the caller may delete", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    editable_company(&state.pool, &user, id).await?;
    sqlx::query("DELETE FROM instructions WHERE id = ?")
        .bind(id)
        .execute(&state.pool)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
