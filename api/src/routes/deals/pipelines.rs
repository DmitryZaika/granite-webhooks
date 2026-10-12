//! Admins set up the company's pipelines (`groups_list`) and their stages
//! (`deals_list`). The shared starter pipeline (id 1) belongs to every
//! company: it can be hidden or made the default, never edited.

use crate::auth::AdminUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::routes::created::CreatedId;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, MySqlPool};
use utoipa::ToSchema;

/// The shared starter pipeline.
const SHARED_GROUP: i32 = 1;

/// Id, name, displayed and default flags of a `groups_list` row.
type GroupRow = (i32, Option<String>, Option<bool>, Option<bool>);

/// A pipeline with its stages, as the admin settings page shows it.
#[derive(Debug, Serialize, ToSchema)]
pub struct Pipeline {
    /// Group id.
    pub id: i32,
    /// Name.
    pub name: String,
    /// Shown on the boards.
    pub is_displayed: bool,
    /// New deals start in it.
    pub is_default: bool,
    /// The shared starter pipeline: its stages cannot change.
    pub is_shared: bool,
    /// Live stages, in board order.
    pub lists: Vec<PipelineStage>,
}

/// A stage of a pipeline.
#[derive(Debug, Serialize, FromRow, ToSchema)]
pub struct PipelineStage {
    /// List id.
    pub id: i32,
    /// Name.
    pub name: String,
    /// Order on the board.
    pub position: i32,
}

/// Body of `POST /v1/deal-pipelines` and `POST /v1/deal-pipelines/{id}/stages`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NameInput {
    /// Name (1–255 characters).
    pub name: String,
}

/// Body of `PATCH /v1/deal-pipelines/{id}`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PipelinePatch {
    /// Show or hide it on the boards.
    pub is_displayed: Option<bool>,
    /// `true` makes it the company's default (only `true` is accepted).
    pub is_default: Option<bool>,
}

/// Body of `PUT /v1/deal-pipelines/{id}/stage-order`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct StageOrder {
    /// Every stage of the pipeline, first to last.
    pub list_ids: Vec<i32>,
}

fn clean_name(input: &NameInput) -> ApiResult<String> {
    let name = input.name.trim();
    if name.is_empty() || name.chars().count() > 255 {
        return Err(ApiError::BadRequest("Name is required".into()));
    }
    Ok(name.to_string())
}

/// A pipeline the company may change: its own, or (when `shared_ok`) the shared one.
async fn check_group(
    pool: &MySqlPool,
    company_id: i32,
    group_id: i32,
    shared_ok: bool,
) -> ApiResult<()> {
    if group_id == SHARED_GROUP {
        return if shared_ok {
            Ok(())
        } else {
            Err(ApiError::BadRequest(
                "The shared pipeline cannot be changed".into(),
            ))
        };
    }
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM groups_list WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(group_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    found.map(|_| ()).ok_or(ApiError::NotFound)
}

/// List pipelines
///
/// Every pipeline of the company (shown or hidden) and the shared one, with
/// their stages. The shared pipeline is the default while none of the
/// company's own is.
#[utoipa::path(
    get,
    path = "/v1/deal-pipelines",
    operation_id = "list_deal_pipelines",
    tag = "deals",
    responses(
        (status = 200, description = "Pipelines", body = Vec<Pipeline>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
) -> ApiResult<Json<Vec<Pipeline>>> {
    let groups: Vec<GroupRow> = sqlx::query_as(
        "SELECT id, name, is_displayed, is_default FROM groups_list
         WHERE deleted_at IS NULL AND (company_id = ? OR id = 1)
         ORDER BY created_at, id",
    )
    .bind(user.company_id)
    .fetch_all(&state.pool)
    .await?;
    let own_default = groups
        .iter()
        .any(|(id, _, _, default)| *id != SHARED_GROUP && default.unwrap_or(false));
    let mut pipelines = Vec::with_capacity(groups.len());
    for (id, name, displayed, default) in groups {
        let lists: Vec<PipelineStage> = sqlx::query_as(
            "SELECT id, name, position FROM deals_list
             WHERE group_id = ? AND deleted_at IS NULL ORDER BY position, id",
        )
        .bind(id)
        .fetch_all(&state.pool)
        .await?;
        pipelines.push(Pipeline {
            id,
            name: name.unwrap_or_default(),
            is_displayed: displayed.unwrap_or(false),
            is_default: if id == SHARED_GROUP {
                !own_default
            } else {
                default.unwrap_or(false)
            },
            is_shared: id == SHARED_GROUP,
            lists,
        });
    }
    Ok(Json(pipelines))
}

/// Create a pipeline
///
/// Adds an empty pipeline to the company.
#[utoipa::path(
    post,
    path = "/v1/deal-pipelines",
    operation_id = "create_deal_pipeline",
    tag = "deals",
    request_body = NameInput,
    responses(
        (status = 201, description = "Created", body = CreatedId),
        (status = 400, description = "Missing name", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<NameInput>,
) -> ApiResult<(StatusCode, Json<CreatedId>)> {
    let name = clean_name(&input)?;
    let id =
        sqlx::query("INSERT INTO groups_list (name, created_at, company_id) VALUES (?, NOW(), ?)")
            .bind(name)
            .bind(user.company_id)
            .execute(&state.pool)
            .await?
            .last_insert_id();
    Ok((
        StatusCode::CREATED,
        Json(CreatedId {
            id: i64::try_from(id).unwrap_or(i64::MAX),
        }),
    ))
}

/// Change a pipeline
///
/// Shows or hides a pipeline, or makes it the company's default (the
/// shared pipeline is the default when none of the company's own is).
#[utoipa::path(
    patch,
    path = "/v1/deal-pipelines/{id}",
    operation_id = "update_deal_pipeline",
    tag = "deals",
    params(("id" = i32, Path, description = "Group id")),
    request_body = PipelinePatch,
    responses(
        (status = 204, description = "Changed"),
        (status = 400, description = "`is_default: false`, or hiding the shared pipeline", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such pipeline of the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
    JsonBody(patch): JsonBody<PipelinePatch>,
) -> ApiResult<StatusCode> {
    let pool = &state.pool;
    check_group(pool, user.company_id, id, true).await?;
    if patch.is_default == Some(false) {
        return Err(ApiError::BadRequest(
            "make another pipeline the default instead".into(),
        ));
    }
    if let Some(displayed) = patch.is_displayed {
        // Every company sees the shared pipeline: it is not hidden for one.
        if id == SHARED_GROUP {
            return Err(ApiError::BadRequest(
                "The shared pipeline cannot be changed".into(),
            ));
        }
        sqlx::query("UPDATE groups_list SET is_displayed = ? WHERE id = ? AND company_id = ?")
            .bind(displayed)
            .bind(id)
            .bind(user.company_id)
            .execute(pool)
            .await?;
    }
    if patch.is_default == Some(true) {
        let mut tx = pool.begin().await?;
        sqlx::query("UPDATE groups_list SET is_default = 0 WHERE company_id = ?")
            .bind(user.company_id)
            .execute(&mut *tx)
            .await?;
        if id != SHARED_GROUP {
            sqlx::query("UPDATE groups_list SET is_default = 1 WHERE id = ? AND company_id = ?")
                .bind(id)
                .bind(user.company_id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Delete a pipeline
///
/// Deletes one of the company's pipelines and its stages.
#[utoipa::path(
    delete,
    path = "/v1/deal-pipelines/{id}",
    operation_id = "delete_deal_pipeline",
    tag = "deals",
    params(("id" = i32, Path, description = "Group id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 400, description = "The shared pipeline", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such pipeline of the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    check_group(&state.pool, user.company_id, id, false).await?;
    let mut tx = state.pool.begin().await?;
    sqlx::query("UPDATE groups_list SET deleted_at = NOW() WHERE id = ? AND company_id = ?")
        .bind(id)
        .bind(user.company_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE deals_list SET deleted_at = NOW() WHERE group_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Add a stage
///
/// Adds a stage at the end of one of the company's pipelines.
#[utoipa::path(
    post,
    path = "/v1/deal-pipelines/{id}/stages",
    operation_id = "create_deal_stage",
    tag = "deals",
    params(("id" = i32, Path, description = "Group id")),
    request_body = NameInput,
    responses(
        (status = 201, description = "Created", body = CreatedId),
        (status = 400, description = "Missing name, or the shared pipeline", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such pipeline of the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create_stage(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<NameInput>,
) -> ApiResult<(StatusCode, Json<CreatedId>)> {
    let name = clean_name(&input)?;
    check_group(&state.pool, user.company_id, id, false).await?;
    let next: i32 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(position), -1) + 1 FROM deals_list WHERE group_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .fetch_one(&state.pool)
    .await?;
    let list_id = sqlx::query("INSERT INTO deals_list (name, group_id, position) VALUES (?, ?, ?)")
        .bind(name)
        .bind(id)
        .bind(next)
        .execute(&state.pool)
        .await?
        .last_insert_id();
    Ok((
        StatusCode::CREATED,
        Json(CreatedId {
            id: i64::try_from(list_id).unwrap_or(i64::MAX),
        }),
    ))
}

/// Order a pipeline's stages
///
/// Saves the order of the stages (every stage of the pipeline, first to last).
#[utoipa::path(
    put,
    path = "/v1/deal-pipelines/{id}/stage-order",
    operation_id = "order_deal_stages",
    tag = "deals",
    params(("id" = i32, Path, description = "Group id")),
    request_body = StageOrder,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Not exactly the pipeline's stages, or the shared pipeline", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such pipeline of the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn order_stages(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
    JsonBody(body): JsonBody<StageOrder>,
) -> ApiResult<StatusCode> {
    check_group(&state.pool, user.company_id, id, false).await?;
    let mut current: Vec<i32> =
        sqlx::query_scalar("SELECT id FROM deals_list WHERE group_id = ? AND deleted_at IS NULL")
            .bind(id)
            .fetch_all(&state.pool)
            .await?;
    let mut wanted = body.list_ids.clone();
    current.sort_unstable();
    wanted.sort_unstable();
    if current != wanted {
        return Err(ApiError::BadRequest(
            "list_ids must be every stage of the pipeline".into(),
        ));
    }
    let mut tx = state.pool.begin().await?;
    for (position, list_id) in body.list_ids.iter().enumerate() {
        sqlx::query("UPDATE deals_list SET position = ? WHERE id = ? AND group_id = ?")
            .bind(i32::try_from(position).unwrap_or(i32::MAX))
            .bind(list_id)
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Delete a stage
///
/// Deletes a stage of one of the company's pipelines.
#[utoipa::path(
    delete,
    path = "/v1/deal-stages/{id}",
    operation_id = "delete_deal_stage",
    tag = "deals",
    params(("id" = i32, Path, description = "List id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 400, description = "A stage of the shared pipeline", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such stage of the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_stage(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    let group: Option<i32> =
        sqlx::query_scalar("SELECT group_id FROM deals_list WHERE id = ? AND deleted_at IS NULL")
            .bind(id)
            .fetch_optional(&state.pool)
            .await?;
    let group = group.ok_or(ApiError::NotFound)?;
    check_group(&state.pool, user.company_id, group, false).await?;
    sqlx::query("UPDATE deals_list SET deleted_at = NOW() WHERE id = ?")
        .bind(id)
        .execute(&state.pool)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
