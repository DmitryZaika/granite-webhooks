use super::queries::{
    company_has_stone_type, find_stone, list_stone_slabs, list_stones, quick_add_stone,
    search_stones,
};
use super::schemas::{
    ListStonesQuery, QuickStone, QuickStoneInput, SearchStonesQuery, Stone, StoneDetail, StoneSlab,
};
use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::{JsonBody, MultiQuery};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;

/// List the company's stones with slab availability
///
/// Returns the stones of the caller's company that are on display (admins may
/// add hidden ones with `include_hidden=true`), sorted by name, each with counts of its slabs in the yard (`slabs`). Sold out stones
/// (no available slab and not regular stock) are hidden unless
/// `include_sold_out=true`; hidden and deleted stones are never returned.
/// Filters combine with AND; list filters take the parameter repeated and
/// match any value. Color ids come from the colors list, supplier ids from
/// the suppliers list.
#[utoipa::path(
    get,
    path = "/v1/stones",
    operation_id = "list_stones",
    tag = "stones",
    params(ListStonesQuery),
    responses(
        (status = 200, description = "Stones sorted by name", body = Vec<Stone>),
        (status = 400, description = "A filter has the wrong type, e.g. `supplier_id=abc`", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin, or asks for hidden stones without being an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    MultiQuery(query): MultiQuery<ListStonesQuery>,
) -> ApiResult<Json<Vec<Stone>>> {
    if query.include_hidden && !(user.is_admin || user.is_superuser) {
        return Err(ApiError::Forbidden);
    }
    Ok(Json(
        list_stones(&state.pool, user.company_id, &query).await?,
    ))
}

/// Get one stone
///
/// Returns every field of a stone of the caller's company, including hidden
/// (not displayed) stones, plus the ids of its colors. Deleted stones and
/// stones of other companies are 404. List its slabs with
/// `GET /v1/stones/{id}/slabs`.
#[utoipa::path(
    get,
    path = "/v1/stones/{id}",
    operation_id = "get_stone",
    tag = "stones",
    params(("id" = i32, Path, description = "Stone id")),
    responses(
        (status = 200, description = "The stone", body = StoneDetail),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such stone in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<StoneDetail>> {
    find_stone(&state.pool, user.company_id, id)
        .await?
        .map(Json)
        .ok_or(ApiError::NotFound)
}

/// List a stone's slabs
///
/// Returns the slabs and remnants in the yard for a stone: its own plus
/// those of the stones it draws from (linked stones, marked with
/// `source_stone`). Cut and deleted slabs are left out. Sold slabs carry
/// their sale (customer, seller, date, notes, room, sinks); remnants carry
/// the sale of the slab they were cut from. Ordered by slab id; group by
/// `bundle` for display. Sell an available slab with the sales routes,
/// resize one with `PATCH /v1/slabs/{id}`.
#[utoipa::path(
    get,
    path = "/v1/stones/{id}/slabs",
    operation_id = "list_stone_slabs",
    tag = "stones",
    params(("id" = i32, Path, description = "Stone id")),
    responses(
        (status = 200, description = "Slabs by id", body = Vec<StoneSlab>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such stone in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn slabs(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<Vec<StoneSlab>>> {
    if find_stone(&state.pool, user.company_id, id)
        .await?
        .is_none()
    {
        return Err(ApiError::NotFound);
    }
    Ok(Json(
        list_stone_slabs(&state.pool, user.company_id, id).await?,
    ))
}

/// Search the company's stones by name
///
/// Returns up to `limit` stones whose name contains `name`, best matches
/// first (name starts with it, then contains it as a word), each with the
/// same fields and slab counts as `GET /v1/stones`. By default only
/// displayed stones that are regular stock or have an available whole slab;
/// see `include_sold_out` and `include_hidden`.
#[utoipa::path(
    get,
    path = "/v1/stones/search",
    operation_id = "search_stones",
    tag = "stones",
    params(SearchStonesQuery),
    responses(
        (status = 200, description = "Matching stones, best first", body = Vec<Stone>),
        (status = 400, description = "A parameter has the wrong type", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn search(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    MultiQuery(query): MultiQuery<SearchStonesQuery>,
) -> ApiResult<Json<Vec<Stone>>> {
    Ok(Json(
        search_stones(&state.pool, user.company_id, &query).await?,
    ))
}

/// Add a stone with its slabs in one step
///
/// Creates a displayed stone and either one leftover slab (bundle
/// `LO-YYYYMMDD-XXXX`) or one slab per bundle label, all with the given
/// size. Used while selling a slab that is not in the system yet. Returns
/// the stone and the new slab ids, ready to put on a sale.
#[utoipa::path(
    post,
    path = "/v1/stones/quick",
    operation_id = "quick_add_stone",
    tag = "stones",
    request_body = QuickStoneInput,
    responses(
        (status = 201, description = "Stone and slabs created", body = QuickStone),
        (status = 400, description = "Missing name, non-positive price, negative size, unknown stone type, or no bundles", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn quick_add(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<QuickStoneInput>,
) -> ApiResult<(StatusCode, Json<QuickStone>)> {
    if input.name.trim().is_empty() {
        return Err(ApiError::BadRequest("Name is required".into()));
    }
    if !(input.retail_price.is_finite() && input.retail_price > 0.0) {
        return Err(ApiError::BadRequest("Price is required".into()));
    }
    for size in [input.length, input.width].into_iter().flatten() {
        if !(size.is_finite() && size >= 0.0) {
            return Err(ApiError::BadRequest(
                "Size must be a non-negative number".into(),
            ));
        }
    }
    if !company_has_stone_type(&state.pool, user.company_id, input.kind.trim()).await? {
        return Err(ApiError::BadRequest("Select a valid stone type".into()));
    }
    let bundles: Vec<String> = if input.leftover {
        vec![leftover_bundle()]
    } else {
        let bundles: Vec<String> = input.bundles.iter().map(|b| b.trim().to_string()).collect();
        if bundles.is_empty() || bundles.iter().any(String::is_empty) {
            return Err(ApiError::BadRequest("Bundle number is required".into()));
        }
        bundles
    };
    let (id, slab_ids) = quick_add_stone(&state.pool, user.company_id, &input, &bundles).await?;
    Ok((
        StatusCode::CREATED,
        Json(QuickStone {
            id,
            name: input.name.trim().to_string(),
            kind: input.kind.trim().to_string(),
            retail_price: input.retail_price,
            slab_ids,
        }),
    ))
}

/// `LO-YYYYMMDD-XXXX`: leftover bundle label, `XXXX` 4 hex digits from the clock.
fn leftover_bundle() -> String {
    let now = chrono::Utc::now();
    let noise = now.timestamp_subsec_nanos() ^ u32::try_from(now.timestamp() & 0xFFFF).unwrap_or(0);
    format!("LO-{}-{:04X}", now.format("%Y%m%d"), noise & 0xFFFF)
}
