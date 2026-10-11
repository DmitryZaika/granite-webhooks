use super::queries;
use super::schemas::{
    Activity, ActivityInput, ActivityPatch, ActivitySuggestion, BoardDeal, BoardQuery,
    ByCustomerQuery, ContentInput, CustomerDeal, DealCreate, DealCreated, DealDetail, DealGroup,
    DealList, DealPatch, DealStatus, GroupsQuery, ListsQuery, Note, NotePatch, OutcomeBody,
    Recipient, RecipientsQuery, ReorderBody, StageVisit,
};
use super::service::{self, Notification};
use crate::auth::EmployeeUser;
use crate::auth::session::SessionUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::{JsonBody, MultiQuery};
use crate::routes::created::CreatedId;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::NaiveDate;

/// `positions.id` of a sales rep.
const SALES_REP_POSITION: i32 = 1;
const MAX_NOTE: usize = 5000;
const MAX_COMMENT: usize = 2000;
const MAX_ACTIVITY_NAME: usize = 255;

/// The user's name as written in `created_by` / audit columns: trimmed, at
/// most 100 characters, None when blank (`auditDisplayName`).
fn audit_name(user: &SessionUser) -> Option<String> {
    let name = user.name.as_deref().unwrap_or_default().trim();
    (!name.is_empty()).then(|| name.chars().take(100).collect())
}

const fn is_admin(user: &SessionUser) -> bool {
    user.is_admin || user.is_superuser
}

async fn require_deal(state: &AppState, user: &SessionUser, deal_id: i64) -> ApiResult<()> {
    if queries::deal_in_company(&state.pool, user.company_id, deal_id).await? {
        Ok(())
    } else {
        Err(ApiError::NotFound)
    }
}

/// List the deal pipelines (groups) of the board
///
/// Returns the company's displayed groups plus the shared default group, in
/// id order. `has_overdue_activity` flags groups where one of the caller's
/// deals with the given `status` has an unfinished activity whose deadline
/// day has passed. Fetch the stages with `GET /v1/deal-lists?group_id=` and
/// the cards with `GET /v1/deals?group_id=`.
#[utoipa::path(
    get,
    path = "/v1/deal-groups",
    operation_id = "list_deal_groups",
    tag = "deals",
    params(GroupsQuery),
    responses(
        (status = 200, description = "Groups in id order", body = Vec<DealGroup>),
        (status = 400, description = "Unknown `status`", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list_groups(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    MultiQuery(query): MultiQuery<GroupsQuery>,
) -> ApiResult<Json<Vec<DealGroup>>> {
    let status = query.status.unwrap_or_default();
    Ok(Json(
        queries::list_groups(&state.pool, user.company_id, user.id, status).await?,
    ))
}

/// List deal stages
///
/// With `group_id`: the live stages (board columns) of that group in board
/// order. Without it: every live stage of every displayed group the company
/// can use, ordered by group name then position (for pickers such as the bulk
/// email "send to a stage" list).
#[utoipa::path(
    get,
    path = "/v1/deal-lists",
    operation_id = "list_deal_lists",
    tag = "deals",
    params(ListsQuery),
    responses(
        (status = 200, description = "Stages", body = Vec<DealList>),
        (status = 400, description = "`group_id` is not an integer", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list_lists(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    MultiQuery(query): MultiQuery<ListsQuery>,
) -> ApiResult<Json<Vec<DealList>>> {
    Ok(Json(
        queries::list_lists(&state.pool, user.company_id, query.group_id).await?,
    ))
}

/// List the caller's deals on a board
///
/// Returns the caller's live deals in the live stages of `group_id` with the
/// given `status` (default `open`), ordered by stage, then position. Each card
/// carries its customer (null when deleted), whether it has photos, emails
/// and notes, its unfinished activities (count, earliest deadline, the one
/// due first) and the phone of a running SMS follow-up flow.
#[utoipa::path(
    get,
    path = "/v1/deals",
    operation_id = "list_board_deals",
    tag = "deals",
    params(BoardQuery),
    responses(
        (status = 200, description = "Deal cards", body = Vec<BoardDeal>),
        (status = 400, description = "Missing or malformed `group_id`, unknown `status`", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn board(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    MultiQuery(query): MultiQuery<BoardQuery>,
) -> ApiResult<Json<Vec<BoardDeal>>> {
    let status = query.status.unwrap_or_default();
    Ok(Json(
        queries::board(
            &state.pool,
            user.company_id,
            user.id,
            query.group_id,
            status,
        )
        .await?,
    ))
}

/// Get one deal
///
/// Returns a live deal of the company: stage, group, outcome, amounts, the
/// customer and how many deals they have, and how many files are attached.
/// Related reads: `/stage-history`, `/activities`, `/notes`.
#[utoipa::path(
    get,
    path = "/v1/deals/{id}",
    operation_id = "get_deal",
    tag = "deals",
    params(("id" = i64, Path, description = "Deal id")),
    responses(
        (status = 200, description = "The deal", body = DealDetail),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such live deal in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<DealDetail>> {
    queries::find_deal(&state.pool, user.company_id, id)
        .await?
        .map(Json)
        .ok_or(ApiError::NotFound)
}

/// Get the stage history of a deal
///
/// Every stage the deal entered, oldest first; `exited_at` is null for the
/// current stay. The last entry of a closed deal tells when it was closed.
#[utoipa::path(
    get,
    path = "/v1/deals/{id}/stage-history",
    operation_id = "get_deal_stage_history",
    tag = "deals",
    params(("id" = i64, Path, description = "Deal id")),
    responses(
        (status = 200, description = "Stage visits, oldest first", body = Vec<StageVisit>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such live deal in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn stage_history(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<Vec<StageVisit>>> {
    require_deal(&state, &user, id).await?;
    Ok(Json(queries::stage_history(&state.pool, id).await?))
}

/// Create a deal
///
/// Puts the customer in `list_id`. When the customer already has an open
/// deal, that deal is updated and moved instead (`created: false`). The deal
/// belongs to the customer's sales rep, or the caller when there is none; a
/// caller who is a sales rep becomes the rep of a customer without one. Drip
/// emails and SMS of the stage are scheduled for lead customers.
#[utoipa::path(
    post,
    path = "/v1/deals",
    operation_id = "create_deal",
    tag = "deals",
    request_body = DealCreate,
    responses(
        (status = 201, description = "Deal created or reused", body = DealCreated),
        (status = 400, description = "Unknown stage or customer, malformed body", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
#[allow(clippy::too_many_lines)] // One transaction, ported step by step from Remix.
pub async fn create(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<DealCreate>,
) -> ApiResult<(StatusCode, Json<DealCreated>)> {
    let stage_name = queries::list_is_visible(&state.pool, user.company_id, input.list_id)
        .await?
        .ok_or_else(|| ApiError::BadRequest(format!("unknown list_id {}", input.list_id)))?;
    let is_won = match input.outcome.unwrap_or_default() {
        DealStatus::Open => None,
        DealStatus::Won => Some(1),
        DealStatus::Lost => Some(0),
    };
    let position = input.position.unwrap_or(1);
    let created_by = audit_name(&user);

    let mut tx = state.pool.begin().await?;
    let customer: Option<(i32, Option<i32>)> = sqlx::query_as(
        "SELECT id, sales_rep FROM customers
         WHERE id = ? AND company_id = ? AND deleted_at IS NULL LIMIT 1 FOR UPDATE",
    )
    .bind(input.customer_id)
    .bind(user.company_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((_, customer_rep)) = customer else {
        return Err(ApiError::BadRequest(format!(
            "unknown customer_id {}",
            input.customer_id
        )));
    };
    // Only pipeline deals are reused: the board hides closed ones.
    let open_deal: Option<(i64, i32)> = sqlx::query_as(
        "SELECT CAST(id AS SIGNED), list_id FROM deals
         WHERE customer_id = ? AND deleted_at IS NULL AND is_won IS NULL
         ORDER BY id LIMIT 1",
    )
    .bind(input.customer_id)
    .fetch_optional(&mut *tx)
    .await?;

    let (deal_id, created, moved) = if let Some((deal_id, previous_list)) = open_deal {
        sqlx::query(
            "UPDATE deals SET amount = ?, title = ?, status = ?, list_id = ?, position = ?, is_won = ?
             WHERE id = ?",
        )
        .bind(input.amount)
        .bind(&input.title)
        .bind(&stage_name)
        .bind(input.list_id)
        .bind(position)
        .bind(is_won)
        .bind(deal_id)
        .execute(&mut *tx)
        .await?;
        let moved = previous_list != input.list_id;
        if moved {
            service::enter_stage(&mut tx, deal_id, input.list_id).await?;
        }
        (deal_id, false, moved)
    } else {
        let result = sqlx::query(
            "INSERT INTO deals (customer_id, amount, title, status, list_id, position, user_id, is_won, created_by)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(input.customer_id)
        .bind(input.amount)
        .bind(&input.title)
        .bind(&stage_name)
        .bind(input.list_id)
        .bind(position)
        .bind(customer_rep.unwrap_or(user.id))
        .bind(is_won)
        .bind(&created_by)
        .execute(&mut *tx)
        .await?;
        let deal_id = i64::try_from(result.last_insert_id()).unwrap_or_default();
        service::enter_stage(&mut tx, deal_id, input.list_id).await?;
        (deal_id, true, false)
    };
    tx.commit().await?;

    if created || moved {
        service::sync_drip(&state.pool, deal_id, input.list_id, created).await?;
    }

    if customer_rep.is_none() {
        let is_sales_rep: Option<i32> = sqlx::query_scalar(
            "SELECT 1 FROM users_positions WHERE user_id = ? AND position_id = ? LIMIT 1",
        )
        .bind(user.id)
        .bind(SALES_REP_POSITION)
        .fetch_optional(&state.pool)
        .await?;
        if is_sales_rep.is_some() {
            sqlx::query(
                "UPDATE customers SET sales_rep = ?, assigned_date = COALESCE(assigned_date, NOW())
                 WHERE id = ?",
            )
            .bind(user.id)
            .bind(input.customer_id)
            .execute(&state.pool)
            .await?;
            if let Some(name) = &created_by {
                sqlx::query(
                    "INSERT INTO customers_history (customer_id, reassigned_by, reassigned_to) VALUES (?, ?, ?)",
                )
                .bind(input.customer_id)
                .bind(name)
                .bind(name)
                .execute(&state.pool)
                .await?;
            }
        }
    }

    Ok((
        StatusCode::CREATED,
        Json(DealCreated {
            id: deal_id,
            created,
        }),
    ))
}

fn parse_due_date(raw: &str) -> ApiResult<NaiveDate> {
    NaiveDate::parse_from_str(raw.trim(), "%Y-%m-%d")
        .map_err(|_| ApiError::BadRequest(format!("invalid due_date: {raw}")))
}

/// Update a deal
///
/// Changes only the fields sent. Moving a deal to another stage (`list_id`,
/// or `group_id` for the first stage of another pipeline) records the stage
/// history, schedules that stage's drip emails and SMS, and reopens it when
/// it was won or lost. Changing `customer_id` gives the deal to another
/// customer of the company.
#[utoipa::path(
    patch,
    path = "/v1/deals/{id}",
    operation_id = "update_deal",
    tag = "deals",
    params(("id" = i64, Path, description = "Deal id")),
    request_body = DealPatch,
    responses(
        (status = 204, description = "Updated"),
        (status = 400, description = "Unknown stage or customer, bad date, malformed body", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such live deal in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i64>,
    JsonBody(patch): JsonBody<DealPatch>,
) -> ApiResult<StatusCode> {
    let deal = queries::find_deal(&state.pool, user.company_id, id)
        .await?
        .ok_or(ApiError::NotFound)?;

    let mut sets: Vec<&str> = Vec::new();
    let mut query = sqlx::QueryBuilder::<sqlx::MySql>::new("UPDATE deals SET ");
    let mut separated = query.separated(", ");
    if let Some(title) = &patch.title {
        separated
            .push("title = ")
            .push_bind_unseparated(title.clone());
        sets.push("title");
    }
    if let Some(amount) = patch.amount {
        separated.push("amount = ").push_bind_unseparated(amount);
        sets.push("amount");
    }
    if let Some(due_date) = &patch.due_date {
        let due_date = due_date.as_deref().map(parse_due_date).transpose()?;
        separated
            .push("due_date = ")
            .push_bind_unseparated(due_date);
        sets.push("due_date");
    }
    if let Some(position) = patch.position {
        separated
            .push("position = ")
            .push_bind_unseparated(position);
        sets.push("position");
    }
    if let Some(customer_id) = patch.customer_id {
        let found: Option<i32> = sqlx::query_scalar(
            "SELECT id FROM customers WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
        )
        .bind(customer_id)
        .bind(user.company_id)
        .fetch_optional(&state.pool)
        .await?;
        if found.is_none() {
            return Err(ApiError::BadRequest(format!(
                "unknown customer_id {customer_id}"
            )));
        }
        separated
            .push("customer_id = ")
            .push_bind_unseparated(customer_id);
        sets.push("customer_id");
    }
    let target_list = match (patch.list_id, patch.group_id) {
        (Some(list_id), _) => Some(list_id),
        (None, Some(group_id)) if group_id != deal.group_id => Some(
            queries::first_list_of_group(&state.pool, user.company_id, group_id)
                .await?
                .ok_or_else(|| ApiError::BadRequest(format!("group {group_id} has no stages")))?,
        ),
        _ => None,
    };
    let moved = match target_list {
        Some(list_id) if list_id != deal.list_id => {
            let name = queries::list_is_visible(&state.pool, user.company_id, list_id)
                .await?
                .ok_or_else(|| ApiError::BadRequest(format!("unknown list_id {list_id}")))?;
            separated.push("list_id = ").push_bind_unseparated(list_id);
            separated.push("status = ").push_bind_unseparated(name);
            separated.push("updated_at = NOW()");
            if deal.outcome != DealStatus::Open {
                separated.push("is_won = NULL");
                separated.push("lost_reason = NULL");
            }
            sets.push("list_id");
            Some(list_id)
        }
        _ => None,
    };
    if sets.is_empty() {
        return Ok(StatusCode::NO_CONTENT);
    }
    query.push(" WHERE id = ").push_bind(id);
    query.build().execute(&state.pool).await?;

    if let Some(list_id) = moved {
        service::transition_stage(&state.pool, id, list_id).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Save the order of the board after a drag
///
/// Sets stage and position of every card sent. Cards that changed stage get
/// the same treatment as `PATCH /v1/deals/{id}` with `list_id` (history,
/// drips, reopening a closed deal). Every deal and stage must belong to the
/// company, otherwise nothing is saved.
#[utoipa::path(
    post,
    path = "/v1/deals/reorder",
    operation_id = "reorder_deals",
    tag = "deals",
    request_body = ReorderBody,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Empty list, unknown stage, malformed body", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "A deal is not a live deal of the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn reorder(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(body): JsonBody<ReorderBody>,
) -> ApiResult<StatusCode> {
    if body.updates.is_empty() {
        return Err(ApiError::BadRequest("no updates".into()));
    }
    let mut plan = Vec::with_capacity(body.updates.len());
    for item in &body.updates {
        let deal = queries::find_deal(&state.pool, user.company_id, item.id)
            .await?
            .ok_or(ApiError::NotFound)?;
        let name = queries::list_is_visible(&state.pool, user.company_id, item.list_id)
            .await?
            .ok_or_else(|| ApiError::BadRequest(format!("unknown list_id {}", item.list_id)))?;
        plan.push((item, deal, name));
    }
    let mut moved = Vec::new();
    let mut tx = state.pool.begin().await?;
    for (item, deal, name) in plan {
        let across = deal.list_id != item.list_id;
        let reopen = across && deal.outcome != DealStatus::Open;
        sqlx::query(
            "UPDATE deals SET list_id = ?, status = ?, position = ?,
               lost_reason = IF(?, NULL, lost_reason), is_won = IF(?, NULL, is_won),
               updated_at = IF(?, NOW(), updated_at)
             WHERE id = ?",
        )
        .bind(item.list_id)
        .bind(name)
        .bind(item.position)
        .bind(reopen)
        .bind(reopen)
        .bind(across)
        .bind(item.id)
        .execute(&mut *tx)
        .await?;
        if across {
            moved.push((item.id, item.list_id));
        }
    }
    tx.commit().await?;
    for (deal_id, list_id) in moved {
        service::transition_stage(&state.pool, deal_id, list_id).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Close or reopen a deal
///
/// `won` and `lost` close the deal (clearing the expected close date; `lost`
/// stores `lost_reason`). `open` reopens a closed deal in its stage (or its
/// last live stage) and schedules the stage's drips again.
#[utoipa::path(
    put,
    path = "/v1/deals/{id}/outcome",
    operation_id = "set_deal_outcome",
    tag = "deals",
    params(("id" = i64, Path, description = "Deal id")),
    request_body = OutcomeBody,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Malformed body", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such live deal in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn set_outcome(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i64>,
    JsonBody(body): JsonBody<OutcomeBody>,
) -> ApiResult<StatusCode> {
    require_deal(&state, &user, id).await?;
    // `updated_at` has no ON UPDATE: monthly totals read it, so bump it here.
    match body.outcome {
        DealStatus::Won => {
            sqlx::query(
                "UPDATE deals SET is_won = 1, lost_reason = NULL, due_date = NULL, updated_at = NOW() WHERE id = ?",
            )
            .bind(id)
            .execute(&state.pool)
            .await?;
        }
        DealStatus::Lost => {
            let reason = body
                .lost_reason
                .map(|r| r.trim().to_string())
                .filter(|r| !r.is_empty());
            sqlx::query(
                "UPDATE deals SET is_won = 0, lost_reason = ?, due_date = NULL, updated_at = NOW() WHERE id = ?",
            )
            .bind(reason)
            .bind(id)
            .execute(&state.pool)
            .await?;
        }
        DealStatus::Open => {
            sqlx::query(
                "UPDATE deals SET is_won = NULL, lost_reason = NULL, updated_at = NOW() WHERE id = ?",
            )
            .bind(id)
            .execute(&state.pool)
            .await?;
            service::reopen(&state.pool, id).await?;
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Delete a deal
///
/// Soft-deletes the deal and records who deleted it in the customer's history.
#[utoipa::path(
    delete,
    path = "/v1/deals/{id}",
    operation_id = "delete_deal",
    tag = "deals",
    params(("id" = i64, Path, description = "Deal id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such live deal in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i64>,
) -> ApiResult<StatusCode> {
    let deal = queries::find_deal(&state.pool, user.company_id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let mut tx = state.pool.begin().await?;
    sqlx::query("UPDATE deals SET deleted_at = NOW() WHERE id = ? AND deleted_at IS NULL")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    if let Some(name) = audit_name(&user) {
        sqlx::query(
            "INSERT INTO customers_history (customer_id, reassigned_by, reassigned_to) VALUES (?, ?, ?)",
        )
        .bind(deal.customer_id)
        .bind(name)
        .bind(format!("deal_deleted:{id}"))
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// List a customer's deals
///
/// Live deals of a customer of the company, found by `customer_id`, or by
/// any of their email addresses or their exact name. The caller's own deals
/// come first, then newest. Each deal carries up to two notes and two
/// activities as a preview.
#[utoipa::path(
    get,
    path = "/v1/deals/by-customer",
    operation_id = "list_customer_deals",
    tag = "deals",
    params(ByCustomerQuery),
    responses(
        (status = 200, description = "The customer's deals", body = Vec<CustomerDeal>),
        (status = 400, description = "None of `customer_id`, `customer_email`, `customer_name`", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn by_customer(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    MultiQuery(query): MultiQuery<ByCustomerQuery>,
) -> ApiResult<Json<Vec<CustomerDeal>>> {
    let email = query.customer_email.as_deref().unwrap_or_default().trim();
    let name = query.customer_name.as_deref().unwrap_or_default().trim();
    let customer_id = query.customer_id.filter(|id| *id > 0);
    if customer_id.is_none() && email.is_empty() && name.is_empty() {
        return Err(ApiError::BadRequest(
            "customer_id, customer_email or customer_name is required".into(),
        ));
    }
    Ok(Json(
        queries::customer_deals(
            &state.pool,
            user.company_id,
            user.id,
            customer_id,
            email,
            name,
        )
        .await?,
    ))
}

/// List the email recipients of a stage
///
/// Email addresses of the caller's customers (they are the sales rep) with a
/// live open deal in stage `id`; won and lost deals count when asked. Each
/// address once, lowercased, sorted.
#[utoipa::path(
    get,
    path = "/v1/deal-lists/{id}/recipients",
    operation_id = "list_deal_list_recipients",
    tag = "deals",
    params(("id" = i32, Path, description = "Stage id"), RecipientsQuery),
    responses(
        (status = 200, description = "Recipients", body = Vec<Recipient>),
        (status = 400, description = "Malformed params", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list_recipients(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    MultiQuery(query): MultiQuery<RecipientsQuery>,
) -> ApiResult<Json<Vec<Recipient>>> {
    Ok(Json(
        queries::list_recipients(
            &state.pool,
            user.id,
            id,
            query.include_won,
            query.include_lost,
        )
        .await?,
    ))
}

/// Parsed activity deadline: the value stored (store wall time) and the raw
/// text the reminder is computed from.
struct Deadline {
    stored: String,
    raw: String,
}

/// `YYYY-MM-DD`, `YYYY-MM-DD HH:MM:SS` or `YYYY-MM-DDTHH:MM:SS...` (the
/// digits are kept as they are: deadlines are store wall times).
fn parse_deadline(raw: &str) -> ApiResult<Option<Deadline>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    let invalid = || ApiError::BadRequest("Invalid deadline date format".into());
    let date = NaiveDate::parse_from_str(raw.get(..10).ok_or_else(invalid)?, "%Y-%m-%d")
        .map_err(|_| invalid())?;
    let stored = if raw.len() == 10 {
        format!("{date} 00:00:00")
    } else {
        let time = raw.get(11..19).ok_or_else(invalid)?;
        let separator = &raw[10..11];
        if separator != "T" && separator != " " {
            return Err(invalid());
        }
        chrono::NaiveTime::parse_from_str(time, "%H:%M:%S").map_err(|_| invalid())?;
        format!("{date} {time}")
    };
    Ok(Some(Deadline {
        stored,
        raw: raw.to_string(),
    }))
}

fn activity_name(name: &str) -> ApiResult<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(ApiError::BadRequest("Activity name is required".into()));
    }
    if name.chars().count() > MAX_ACTIVITY_NAME {
        return Err(ApiError::BadRequest(
            "Activity name must be 255 characters or less".into(),
        ));
    }
    Ok(name.to_string())
}

/// List a deal's activities
///
/// Live activities of the deal, newest first.
#[utoipa::path(
    get,
    path = "/v1/deals/{id}/activities",
    operation_id = "list_deal_activities",
    tag = "deals",
    params(("id" = i64, Path, description = "Deal id")),
    responses(
        (status = 200, description = "Activities, newest first", body = Vec<Activity>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such live deal in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list_activities(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<Vec<Activity>>> {
    require_deal(&state, &user, id).await?;
    Ok(Json(
        queries::list_activities(&state.pool, user.company_id, id).await?,
    ))
}

/// Add an activity to a deal
///
/// Creates a to-do. The deal owner and the customer's sales rep are notified
/// (in the app and on Telegram), and a deadline with a time schedules a
/// reminder for the owner at that time.
#[utoipa::path(
    post,
    path = "/v1/deals/{id}/activities",
    operation_id = "create_deal_activity",
    tag = "deals",
    params(("id" = i64, Path, description = "Deal id")),
    request_body = ActivityInput,
    responses(
        (status = 201, description = "Created", body = CreatedId),
        (status = 400, description = "Empty or too long name, bad deadline, malformed body", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such live deal in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create_activity(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i64>,
    JsonBody(input): JsonBody<ActivityInput>,
) -> ApiResult<(StatusCode, Json<CreatedId>)> {
    let name = activity_name(&input.name)?;
    let deadline = input
        .deadline
        .as_deref()
        .map(parse_deadline)
        .transpose()?
        .flatten();
    let priority = input.priority.unwrap_or_default();
    require_deal(&state, &user, id).await?;

    let result = sqlx::query(
        "INSERT INTO deal_activities (deal_id, company_id, name, deadline, priority, created_by)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(user.company_id)
    .bind(&name)
    .bind(deadline.as_ref().map(|d| d.stored.clone()))
    .bind(priority.as_str())
    .bind(&user.name)
    .execute(&state.pool)
    .await?;

    if let Some(actor) = audit_name(&user) {
        service::notify_assignees(
            &state,
            id,
            user.id,
            &actor,
            &name,
            Notification::ActivityAdded,
        )
        .await;
    }
    service::schedule_activity_reminder(
        &state.pool,
        id,
        user.id,
        &name,
        deadline.as_ref().map(|d| d.raw.as_str()),
        None,
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(CreatedId {
            id: i64::try_from(result.last_insert_id()).unwrap_or_default(),
        }),
    ))
}

/// Update an activity
///
/// Changes the fields sent. Editing name, deadline or priority notifies the
/// deal's people and reschedules the reminder; `is_completed: true` marks it
/// done (and drops its reminder), `false` reopens it.
#[utoipa::path(
    patch,
    path = "/v1/deals/{id}/activities/{activity_id}",
    operation_id = "update_deal_activity",
    tag = "deals",
    params(
        ("id" = i64, Path, description = "Deal id"),
        ("activity_id" = i32, Path, description = "Activity id"),
    ),
    request_body = ActivityPatch,
    responses(
        (status = 204, description = "Updated"),
        (status = 400, description = "Empty or too long name, bad deadline, malformed body", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such activity on a live deal of the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update_activity(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((id, activity_id)): Path<(i64, i32)>,
    JsonBody(patch): JsonBody<ActivityPatch>,
) -> ApiResult<StatusCode> {
    let current = queries::find_activity(&state.pool, user.company_id, id, activity_id)
        .await?
        .ok_or(ApiError::NotFound)?;

    let edits = patch.name.is_some() || patch.deadline.is_some() || patch.priority.is_some();
    if edits {
        let name = match &patch.name {
            Some(name) => activity_name(name)?,
            None => current.name.clone(),
        };
        let deadline = match &patch.deadline {
            Some(Some(raw)) => parse_deadline(raw)?,
            Some(None) => None,
            None => current
                .deadline
                .as_deref()
                .map(parse_deadline)
                .transpose()?
                .flatten(),
        };
        let priority = patch
            .priority
            .map_or_else(|| current.priority.clone(), |p| p.as_str().to_string());
        sqlx::query("UPDATE deal_activities SET name = ?, deadline = ?, priority = ? WHERE id = ?")
            .bind(&name)
            .bind(deadline.as_ref().map(|d| d.stored.clone()))
            .bind(priority)
            .bind(activity_id)
            .execute(&state.pool)
            .await?;
        if let Some(actor) = audit_name(&user) {
            service::notify_assignees(
                &state,
                id,
                user.id,
                &actor,
                &name,
                Notification::ActivityEdited,
            )
            .await;
        }
        let completed = patch.is_completed.unwrap_or(current.is_completed);
        service::schedule_activity_reminder(
            &state.pool,
            id,
            user.id,
            &name,
            if completed {
                None
            } else {
                deadline.as_ref().map(|d| d.raw.as_str())
            },
            Some(&current.name),
        )
        .await?;
    }
    if let Some(done) = patch.is_completed
        && done != current.is_completed
    {
        sqlx::query(
            "UPDATE deal_activities SET is_completed = ?, completed_at = IF(?, NOW(), NULL) WHERE id = ?",
        )
        .bind(done)
        .bind(done)
        .bind(activity_id)
        .execute(&state.pool)
        .await?;
        if done {
            let name = patch
                .name
                .as_deref()
                .map_or(current.name.as_str(), str::trim);
            service::remove_activity_reminder(&state.pool, id, name).await?;
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Delete an activity
///
/// Soft-deletes it, drops its reminder and notifies the deal's people.
#[utoipa::path(
    delete,
    path = "/v1/deals/{id}/activities/{activity_id}",
    operation_id = "delete_deal_activity",
    tag = "deals",
    params(
        ("id" = i64, Path, description = "Deal id"),
        ("activity_id" = i32, Path, description = "Activity id"),
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such activity on a live deal of the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_activity(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((id, activity_id)): Path<(i64, i32)>,
) -> ApiResult<StatusCode> {
    let current = queries::find_activity(&state.pool, user.company_id, id, activity_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    sqlx::query("UPDATE deal_activities SET deleted_at = NOW() WHERE id = ?")
        .bind(activity_id)
        .execute(&state.pool)
        .await?;
    service::remove_activity_reminder(&state.pool, id, &current.name).await?;
    if let Some(actor) = audit_name(&user) {
        service::notify_assignees(
            &state,
            id,
            user.id,
            &actor,
            &current.name,
            Notification::ActivityDeleted,
        )
        .await;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// List the caller's frequent activity names
///
/// Up to 50 names of live activities the caller created, most used first,
/// then most recently used; for the activity name input's suggestions.
#[utoipa::path(
    get,
    path = "/v1/deal-activities/suggestions",
    operation_id = "list_activity_suggestions",
    tag = "deals",
    responses(
        (status = 200, description = "Suggestions", body = Vec<ActivitySuggestion>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn activity_suggestions(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<Vec<ActivitySuggestion>>> {
    let Some(name) = user.name.as_deref() else {
        return Ok(Json(Vec::new()));
    };
    Ok(Json(
        queries::activity_suggestions(&state.pool, user.company_id, name).await?,
    ))
}

fn note_text(content: &str, max: usize, what: &str) -> ApiResult<String> {
    let content = content.trim();
    if content.is_empty() {
        return Err(ApiError::BadRequest(format!("{what} content is required")));
    }
    if content.chars().count() > max {
        return Err(ApiError::BadRequest(format!(
            "{what} must be {max} characters or less"
        )));
    }
    Ok(content.to_string())
}

async fn require_note(
    state: &AppState,
    user: &SessionUser,
    deal_id: i64,
    note_id: i64,
) -> ApiResult<String> {
    sqlx::query_scalar(
        "SELECT content FROM deal_notes
         WHERE id = ? AND deal_id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(note_id)
    .bind(deal_id)
    .bind(user.company_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or(ApiError::NotFound)
}

/// List a deal's notes
///
/// Live notes, newest first, each with its live comments oldest first.
#[utoipa::path(
    get,
    path = "/v1/deals/{id}/notes",
    operation_id = "list_deal_notes",
    tag = "deals",
    params(("id" = i64, Path, description = "Deal id")),
    responses(
        (status = 200, description = "Notes, newest first", body = Vec<Note>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such live deal in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list_notes(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<Vec<Note>>> {
    require_deal(&state, &user, id).await?;
    Ok(Json(
        queries::list_notes(&state.pool, user.company_id, id).await?,
    ))
}

/// Add a note to a deal
///
/// Up to 5000 characters. Notifies the deal's people.
#[utoipa::path(
    post,
    path = "/v1/deals/{id}/notes",
    operation_id = "create_deal_note",
    tag = "deals",
    params(("id" = i64, Path, description = "Deal id")),
    request_body = ContentInput,
    responses(
        (status = 201, description = "Created", body = CreatedId),
        (status = 400, description = "Empty or too long, malformed body", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such live deal in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create_note(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i64>,
    JsonBody(input): JsonBody<ContentInput>,
) -> ApiResult<(StatusCode, Json<CreatedId>)> {
    let content = note_text(&input.content, MAX_NOTE, "Note")?;
    require_deal(&state, &user, id).await?;
    let result = sqlx::query(
        "INSERT INTO deal_notes (deal_id, company_id, content, created_by) VALUES (?, ?, ?, ?)",
    )
    .bind(id)
    .bind(user.company_id)
    .bind(&content)
    .bind(&user.name)
    .execute(&state.pool)
    .await?;
    if let Some(actor) = audit_name(&user) {
        service::notify_assignees(
            &state,
            id,
            user.id,
            &actor,
            &content,
            Notification::NoteAdded,
        )
        .await;
    }
    Ok((
        StatusCode::CREATED,
        Json(CreatedId {
            id: i64::try_from(result.last_insert_id()).unwrap_or_default(),
        }),
    ))
}

/// Edit or pin a note
///
/// Changes the text (notifying the deal's people) and/or pins it.
#[utoipa::path(
    patch,
    path = "/v1/deals/{id}/notes/{note_id}",
    operation_id = "update_deal_note",
    tag = "deals",
    params(
        ("id" = i64, Path, description = "Deal id"),
        ("note_id" = i64, Path, description = "Note id"),
    ),
    request_body = NotePatch,
    responses(
        (status = 204, description = "Updated"),
        (status = 400, description = "Empty or too long, malformed body", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such note on a live deal of the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update_note(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((id, note_id)): Path<(i64, i64)>,
    JsonBody(patch): JsonBody<NotePatch>,
) -> ApiResult<StatusCode> {
    let content = patch
        .content
        .as_deref()
        .map(|c| note_text(c, MAX_NOTE, "Note"))
        .transpose()?;
    require_note(&state, &user, id, note_id).await?;
    if let Some(pinned) = patch.is_pinned {
        sqlx::query("UPDATE deal_notes SET is_pinned = ? WHERE id = ?")
            .bind(pinned)
            .bind(note_id)
            .execute(&state.pool)
            .await?;
    }
    if let Some(content) = content {
        sqlx::query("UPDATE deal_notes SET content = ? WHERE id = ?")
            .bind(&content)
            .bind(note_id)
            .execute(&state.pool)
            .await?;
        if let Some(actor) = audit_name(&user) {
            service::notify_assignees(
                &state,
                id,
                user.id,
                &actor,
                &content,
                Notification::NoteEdited,
            )
            .await;
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Delete a note
///
/// Soft-deletes it. When an admin deletes it, the deal's people are notified.
#[utoipa::path(
    delete,
    path = "/v1/deals/{id}/notes/{note_id}",
    operation_id = "delete_deal_note",
    tag = "deals",
    params(
        ("id" = i64, Path, description = "Deal id"),
        ("note_id" = i64, Path, description = "Note id"),
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such note on a live deal of the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_note(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((id, note_id)): Path<(i64, i64)>,
) -> ApiResult<StatusCode> {
    let content = require_note(&state, &user, id, note_id).await?;
    sqlx::query("UPDATE deal_notes SET deleted_at = NOW() WHERE id = ?")
        .bind(note_id)
        .execute(&state.pool)
        .await?;
    if is_admin(&user) {
        let actor = user.name.clone().unwrap_or_default();
        service::notify_assignees(
            &state,
            id,
            user.id,
            &actor,
            &content,
            Notification::NoteDeleted,
        )
        .await;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Comment on a note
///
/// Up to 2000 characters. Only admins' comments carry their name, and only
/// those notify the deal's people.
#[utoipa::path(
    post,
    path = "/v1/deals/{id}/notes/{note_id}/comments",
    operation_id = "create_deal_note_comment",
    tag = "deals",
    params(
        ("id" = i64, Path, description = "Deal id"),
        ("note_id" = i64, Path, description = "Note id"),
    ),
    request_body = ContentInput,
    responses(
        (status = 201, description = "Created", body = CreatedId),
        (status = 400, description = "Empty or too long, malformed body", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such note on a live deal of the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create_comment(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((id, note_id)): Path<(i64, i64)>,
    JsonBody(input): JsonBody<ContentInput>,
) -> ApiResult<(StatusCode, Json<CreatedId>)> {
    let content = note_text(&input.content, MAX_COMMENT, "Comment")?;
    require_note(&state, &user, id, note_id).await?;
    let author = if is_admin(&user) {
        user.name.clone()
    } else {
        None
    };
    let result = sqlx::query(
        "INSERT INTO deal_note_comments (note_id, company_id, content, created_by) VALUES (?, ?, ?, ?)",
    )
    .bind(note_id)
    .bind(user.company_id)
    .bind(&content)
    .bind(&author)
    .execute(&state.pool)
    .await?;
    if let Some(actor) = author.filter(|a| !a.trim().is_empty()) {
        service::notify_assignees(
            &state,
            id,
            user.id,
            &actor,
            &content,
            Notification::CommentAdded,
        )
        .await;
    }
    Ok((
        StatusCode::CREATED,
        Json(CreatedId {
            id: i64::try_from(result.last_insert_id()).unwrap_or_default(),
        }),
    ))
}

/// Delete a comment
///
/// Soft-deletes it. When an admin deletes it, the deal's people are notified.
#[utoipa::path(
    delete,
    path = "/v1/deals/{id}/notes/{note_id}/comments/{comment_id}",
    operation_id = "delete_deal_note_comment",
    tag = "deals",
    params(
        ("id" = i64, Path, description = "Deal id"),
        ("note_id" = i64, Path, description = "Note id"),
        ("comment_id" = i64, Path, description = "Comment id"),
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such comment on a live note of the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_comment(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((id, note_id, comment_id)): Path<(i64, i64, i64)>,
) -> ApiResult<StatusCode> {
    let content: String = sqlx::query_scalar(
        "SELECT dnc.content FROM deal_note_comments dnc
         JOIN deal_notes dn ON dn.id = dnc.note_id
         WHERE dnc.id = ? AND dnc.note_id = ? AND dn.deal_id = ? AND dnc.company_id = ?
           AND dnc.deleted_at IS NULL AND dn.deleted_at IS NULL",
    )
    .bind(comment_id)
    .bind(note_id)
    .bind(id)
    .bind(user.company_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or(ApiError::NotFound)?;
    sqlx::query("UPDATE deal_note_comments SET deleted_at = NOW() WHERE id = ?")
        .bind(comment_id)
        .execute(&state.pool)
        .await?;
    if is_admin(&user) {
        let actor = user.name.clone().unwrap_or_default();
        service::notify_assignees(
            &state,
            id,
            user.id,
            &actor,
            &content,
            Notification::CommentDeleted,
        )
        .await;
    }
    Ok(StatusCode::NO_CONTENT)
}
