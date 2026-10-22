//! Quote price lists (Admin > Quotes > Price lists): each save is a new
//! immutable revision; quotes keep the revision they were priced with.
//!
//! - `GET    /v1/quote-price-lists`               → the lists and the default for new quotes
//! - `POST   /v1/quote-price-lists`               → a new, empty list
//! - `PUT    /v1/quote-price-lists/default`       → star a list for new quotes
//! - `PUT    /v1/quote-price-lists/order`         → save a drag (the lists' sequence)
//! - `GET    /v1/quote-price-lists/{id}`          → the editor: the list, matched to inventory, and its history
//! - `PUT    /v1/quote-price-lists/{id}`          → save name, status and prices (a new revision)
//! - `PATCH  /v1/quote-price-lists/{id}`          → rename or (de)activate
//! - `DELETE /v1/quote-price-lists/{id}`
//! - `POST   /v1/quote-price-lists/{id}/copies`   → duplicate
//! - `POST   /v1/quote-price-lists/{id}/revisions/{revision_id}/restore`
//!
//! Port of `crud/priceLists.server.ts` and the `admin.quotes.settings`
//! routes. Employees read the list of lists (to pick one on a quote).

use crate::auth::{AdminUser, EmployeeUser};
use crate::entitlements::AddonKey;
use crate::error::{AddonRequiredBody, ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::quote_pricing::edit::{normalize, own_extras, validate};
use crate::quote_pricing::js::{arr, get, truthy};
use crate::quote_pricing::load_stock;
use crate::quote_pricing::price_list::{
    self, Stock, StockSink, StockStone, available_key, sync_with_inventory,
};
use crate::routes::created::CreatedId;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{MySql, MySqlPool, Transaction};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_price_lists, create_price_list))
        .routes(routes!(set_default_price_list))
        .routes(routes!(reorder_price_lists))
        .routes(routes!(
            get_price_list,
            save_price_list,
            update_price_list,
            delete_price_list
        ))
        .routes(routes!(copy_price_list))
        .routes(routes!(restore_revision))
}

/// A price list (`PriceListRow`).
#[derive(Debug, Clone, Serialize, sqlx::FromRow, ToSchema)]
pub struct PriceListRow {
    /// Price list id.
    pub id: i32,
    /// Company.
    pub company_id: i32,
    /// Name, unique in the company.
    pub name: String,
    /// `active` or `inactive`.
    pub status: String,
    /// Order in the lists.
    pub sequence: i32,
    /// The revision quotes price with now.
    pub current_revision_id: Option<i32>,
    /// Its number.
    pub revision_number: Option<i32>,
    /// When it was saved (ISO 8601).
    pub revision_created_at: Option<String>,
    /// Who saved it.
    pub revision_created_by: Option<String>,
    /// When the list was made (ISO 8601).
    pub created_at: Option<String>,
    /// Who made it.
    pub created_by_name: Option<String>,
    /// `in` or `mm`, from the current revision.
    pub units: Option<String>,
    /// Days a quote stays valid, from the current revision.
    pub expiration_days: Option<i64>,
}

const LIST_SELECT: &str = "SELECT pl.id, pl.company_id, pl.name, CAST(pl.status AS CHAR) AS status,
         pl.sequence, pl.current_revision_id, r.revision_number,
         DATE_FORMAT(r.created_at, '%Y-%m-%dT%H:%i:%sZ') AS revision_created_at,
         u.name AS revision_created_by,
         DATE_FORMAT(pl.created_at, '%Y-%m-%dT%H:%i:%sZ') AS created_at,
         cu.name AS created_by_name,
         JSON_UNQUOTE(JSON_EXTRACT(r.info_json, '$.units')) AS units,
         CAST(JSON_EXTRACT(r.info_json, '$.expirationDays') AS SIGNED) AS expiration_days
    FROM price_lists pl
    LEFT JOIN price_list_revisions r ON r.id = pl.current_revision_id
    LEFT JOIN users u ON u.id = r.created_by
    LEFT JOIN users cu ON cu.id = pl.created_by";

/// `listPriceLists`: active first, then by sequence and name.
pub async fn list_rows(pool: &MySqlPool, company_id: i32) -> ApiResult<Vec<PriceListRow>> {
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{LIST_SELECT}
          WHERE pl.company_id = ? AND pl.deleted_at IS NULL
          ORDER BY pl.status = 'active' DESC, pl.sequence, pl.name"
    )))
    .bind(company_id)
    .fetch_all(pool)
    .await?)
}

/// `getPriceList`: the row and its current revision's prices.
pub async fn find(
    pool: &MySqlPool,
    id: i32,
    company_id: i32,
) -> ApiResult<Option<(PriceListRow, Option<Value>)>> {
    let row: Option<PriceListRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{LIST_SELECT} WHERE pl.id = ? AND pl.company_id = ? AND pl.deleted_at IS NULL"
    )))
    .bind(id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else { return Ok(None) };
    let info = match row.current_revision_id {
        Some(revision) => sqlx::query_scalar::<_, Option<String>>(
            "SELECT CAST(info_json AS CHAR) FROM price_list_revisions WHERE id = ?",
        )
        .bind(revision)
        .fetch_optional(pool)
        .await?
        .flatten()
        .and_then(|text| price_list::parse(&Value::String(text))),
        None => None,
    };
    Ok(Some((row, info)))
}

/// `defaultPriceListId`: the starred list if usable, else the first active
/// list with prices.
pub async fn default_id(
    pool: &MySqlPool,
    company_id: i32,
    lists: &[PriceListRow],
) -> ApiResult<Option<i32>> {
    let usable: Vec<&PriceListRow> = lists
        .iter()
        .filter(|l| l.status == "active" && l.current_revision_id.is_some())
        .collect();
    let starred: Option<Option<i32>> = sqlx::query_scalar(
        "SELECT default_price_list_id FROM quote_settings WHERE company_id = ? LIMIT 1",
    )
    .bind(company_id)
    .fetch_optional(pool)
    .await
    .unwrap_or_default();
    let starred = starred.flatten();
    Ok(usable
        .iter()
        .find(|l| Some(l.id) == starred)
        .or_else(|| usable.first())
        .map(|l| l.id))
}

/// `savePriceListRevision`: a new revision, made current. `None` when the
/// list is not the company's.
pub async fn save_revision(
    pool: &MySqlPool,
    price_list_id: i32,
    company_id: i32,
    info: &Value,
    user_id: Option<i32>,
    notes: Option<&str>,
) -> ApiResult<Option<i64>> {
    let mut tx = pool.begin().await?;
    let id = save_revision_in(&mut tx, price_list_id, company_id, info, user_id, notes).await?;
    tx.commit().await?;
    Ok(id)
}

async fn save_revision_in(
    tx: &mut Transaction<'_, MySql>,
    price_list_id: i32,
    company_id: i32,
    info: &Value,
    user_id: Option<i32>,
    notes: Option<&str>,
) -> ApiResult<Option<i64>> {
    let owned: Option<i32> =
        sqlx::query_scalar("SELECT id FROM price_lists WHERE id = ? AND company_id = ? FOR UPDATE")
            .bind(price_list_id)
            .bind(company_id)
            .fetch_optional(&mut **tx)
            .await?;
    if owned.is_none() {
        return Ok(None);
    }
    let latest: Option<i64> = sqlx::query_scalar(
        "SELECT CAST(MAX(revision_number) AS SIGNED) FROM price_list_revisions WHERE price_list_id = ?",
    )
    .bind(price_list_id)
    .fetch_one(&mut **tx)
    .await?;
    let next = latest.unwrap_or(0) + 1;
    let mut info = info.clone();
    info["revision"] = json!(next);
    let id = sqlx::query(
        "INSERT INTO price_list_revisions (price_list_id, revision_number, info_json, notes, created_by)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(price_list_id)
    .bind(next)
    .bind(info.to_string())
    .bind(notes)
    .bind(user_id)
    .execute(&mut **tx)
    .await?
    .last_insert_id();
    sqlx::query("UPDATE price_lists SET current_revision_id = ? WHERE id = ?")
        .bind(id)
        .bind(price_list_id)
        .execute(&mut **tx)
        .await?;
    #[allow(clippy::cast_possible_wrap)]
    Ok(Some(id as i64))
}

/// `createPriceList`: the list and its first revision ("Created").
async fn create(
    pool: &MySqlPool,
    company_id: i32,
    name: &str,
    info: &Value,
    user_id: i32,
    sequence: i32,
) -> ApiResult<i64> {
    let mut tx = pool.begin().await?;
    let id = sqlx::query(
        "INSERT INTO price_lists (company_id, name, sequence, created_by) VALUES (?, ?, ?, ?)",
    )
    .bind(company_id)
    .bind(name)
    .bind(sequence)
    .bind(user_id)
    .execute(&mut *tx)
    .await?
    .last_insert_id();
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    let list_id = id as i32;
    save_revision_in(
        &mut tx,
        list_id,
        company_id,
        info,
        Some(user_id),
        Some("Created"),
    )
    .await?;
    tx.commit().await?;
    Ok(i64::from(list_id))
}

/// The company's price lists.
#[derive(Debug, Serialize, ToSchema)]
pub struct PriceLists {
    /// Active first, then by sequence and name.
    pub price_lists: Vec<PriceListRow>,
    /// The list new quotes get.
    pub default_id: Option<i32>,
}

/// List price lists
///
/// Every list of the company, with the one new quotes get.
#[utoipa::path(
    get,
    path = "/v1/quote-price-lists",
    operation_id = "list_quote_price_lists",
    tag = "quotes",
    responses(
        (status = 200, description = "The lists", body = PriceLists),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin, or the plan lacks the Quotes add-on (`addon`: `quotes`)", body = AddonRequiredBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list_price_lists(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<PriceLists>> {
    user.plan.require(AddonKey::Quotes)?;
    let price_lists = list_rows(&state.pool, user.company_id).await?;
    let default_id = default_id(&state.pool, user.company_id, &price_lists).await?;
    Ok(Json(PriceLists {
        price_lists,
        default_id,
    }))
}

/// A new price list.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewPriceList {
    /// Name, unique in the company.
    pub name: String,
}

/// Create a price list
///
/// An empty list (list-wide settings only), last in order.
#[utoipa::path(
    post,
    path = "/v1/quote-price-lists",
    operation_id = "create_quote_price_list",
    tag = "quotes",
    request_body = NewPriceList,
    responses(
        (status = 201, description = "Created", body = CreatedId),
        (status = 400, description = "No name", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin, or the plan lacks the Quotes add-on (`addon`: `quotes`)", body = AddonRequiredBody),
        (status = 409, description = "The name is taken", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create_price_list(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<NewPriceList>,
) -> ApiResult<(StatusCode, Json<CreatedId>)> {
    user.plan.require(AddonKey::Quotes)?;
    let name = input.name.trim();
    if name.is_empty() {
        return Err(ApiError::BadRequest("Give the price list a name.".into()));
    }
    let lists = list_rows(&state.pool, user.company_id).await?;
    if lists.iter().any(|l| l.name == name) {
        return Err(ApiError::Conflict(format!("\"{name}\" already exists.")));
    }
    let sequence = lists.iter().map(|l| l.sequence).max().unwrap_or(0).max(0) + 1;
    let id = create(
        &state.pool,
        user.company_id,
        name,
        &price_list::empty(),
        user.id,
        sequence,
    )
    .await?;
    Ok((StatusCode::CREATED, Json(CreatedId { id })))
}

/// Duplicate a price list
///
/// A copy named "… (copy)" with the current prices, right after it.
#[utoipa::path(
    post,
    path = "/v1/quote-price-lists/{id}/copies",
    operation_id = "copy_quote_price_list",
    tag = "quotes",
    params(("id" = i32, Path, description = "Price list id")),
    responses(
        (status = 201, description = "Created", body = CreatedId),
        (status = 400, description = "Nothing to copy", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin, or the plan lacks the Quotes add-on (`addon`: `quotes`)", body = AddonRequiredBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn copy_price_list(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
) -> ApiResult<(StatusCode, Json<CreatedId>)> {
    user.plan.require(AddonKey::Quotes)?;
    let Some((source, Some(info))) = find(&state.pool, id, user.company_id).await? else {
        return Err(ApiError::BadRequest(
            "That price list has nothing to copy yet.".into(),
        ));
    };
    let id = create(
        &state.pool,
        user.company_id,
        &format!("{} (copy)", source.name),
        &info,
        user.id,
        source.sequence + 1,
    )
    .await?;
    Ok((StatusCode::CREATED, Json(CreatedId { id })))
}

/// A list to star.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DefaultPriceList {
    /// Price list id.
    pub id: i32,
}

/// Choose the default price list
///
/// New quotes get this list; it is activated. The lists keep their order.
#[utoipa::path(
    put,
    path = "/v1/quote-price-lists/default",
    operation_id = "set_default_quote_price_list",
    tag = "quotes",
    request_body = DefaultPriceList,
    responses(
        (status = 204, description = "Saved"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin, or the plan lacks the Quotes add-on (`addon`: `quotes`)", body = AddonRequiredBody),
        (status = 404, description = "No such list", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn set_default_price_list(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<DefaultPriceList>,
) -> ApiResult<StatusCode> {
    user.plan.require(AddonKey::Quotes)?;
    let lists = list_rows(&state.pool, user.company_id).await?;
    if !lists.iter().any(|l| l.id == input.id) {
        return Err(ApiError::NotFound);
    }
    sqlx::query("UPDATE price_lists SET status = 'active' WHERE id = ? AND company_id = ?")
        .bind(input.id)
        .bind(user.company_id)
        .execute(&state.pool)
        .await?;
    sqlx::query(
        "INSERT INTO quote_settings (company_id, default_price_list_id, updated_by)
         VALUES (?, ?, ?)
         ON DUPLICATE KEY UPDATE default_price_list_id = VALUES(default_price_list_id),
                                 updated_by = VALUES(updated_by)",
    )
    .bind(user.company_id)
    .bind(input.id)
    .bind(user.id)
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Body of `PUT /v1/quote-price-lists/order`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PriceListOrder {
    /// The company's list ids in their new order.
    pub ids: Vec<i32>,
}

/// Reorder the price lists
///
/// Saves a drag on the lists table: each list's sequence becomes its place
/// in `ids` (1 = first). Other companies' ids are ignored; lists left out go
/// after the given ones, in their old order.
#[utoipa::path(
    put,
    path = "/v1/quote-price-lists/order",
    operation_id = "reorder_quote_price_lists",
    tag = "quotes",
    request_body = PriceListOrder,
    responses(
        (status = 204, description = "Saved"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin, or the plan lacks the Quotes add-on (`addon`: `quotes`)", body = AddonRequiredBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn reorder_price_lists(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<PriceListOrder>,
) -> ApiResult<StatusCode> {
    user.plan.require(AddonKey::Quotes)?;
    let lists = list_rows(&state.pool, user.company_id).await?;
    let mut order: Vec<i32> = Vec::with_capacity(lists.len());
    for id in input.ids.into_iter().chain(lists.iter().map(|l| l.id)) {
        if lists.iter().any(|l| l.id == id) && !order.contains(&id) {
            order.push(id);
        }
    }
    let mut tx = state.pool.begin().await?;
    for (sequence, id) in (1..).zip(order) {
        sqlx::query("UPDATE price_lists SET sequence = ? WHERE id = ? AND company_id = ?")
            .bind(sequence)
            .bind(id)
            .bind(user.company_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Name or status of a list.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PriceListMeta {
    /// New name.
    #[serde(default)]
    pub name: Option<String>,
    /// `active` or `inactive`.
    #[serde(default)]
    pub status: Option<String>,
}

fn status_of(status: &str) -> &'static str {
    if status == "active" {
        "active"
    } else {
        "inactive"
    }
}

async fn update_meta(
    pool: &MySqlPool,
    id: i32,
    company_id: i32,
    name: Option<&str>,
    status: Option<&str>,
) -> ApiResult<()> {
    sqlx::query(
        "UPDATE price_lists SET name = COALESCE(?, name), status = COALESCE(?, status)
          WHERE id = ? AND company_id = ?",
    )
    .bind(name)
    .bind(status)
    .bind(id)
    .bind(company_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn check_name(pool: &MySqlPool, id: i32, company_id: i32, name: &str) -> ApiResult<()> {
    let lists = list_rows(pool, company_id).await?;
    if lists.iter().any(|l| l.id != id && l.name == name) {
        return Err(ApiError::Conflict(format!(
            "Another price list is already called \"{name}\"."
        )));
    }
    Ok(())
}

/// Rename or (de)activate a price list
///
/// Without a new revision.
#[utoipa::path(
    patch,
    path = "/v1/quote-price-lists/{id}",
    operation_id = "update_quote_price_list",
    tag = "quotes",
    params(("id" = i32, Path, description = "Price list id")),
    request_body = PriceListMeta,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Empty name", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin, or the plan lacks the Quotes add-on (`addon`: `quotes`)", body = AddonRequiredBody),
        (status = 409, description = "The name is taken", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update_price_list(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<PriceListMeta>,
) -> ApiResult<StatusCode> {
    user.plan.require(AddonKey::Quotes)?;
    let name = input.name.as_deref().map(str::trim);
    if name == Some("") {
        return Err(ApiError::BadRequest("Give the price list a name.".into()));
    }
    if let Some(name) = name {
        check_name(&state.pool, id, user.company_id, name).await?;
    }
    update_meta(
        &state.pool,
        id,
        user.company_id,
        name,
        input.status.as_deref().map(status_of),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Delete a price list
///
/// Quotes made with it keep their prices; it can no longer be picked.
#[utoipa::path(
    delete,
    path = "/v1/quote-price-lists/{id}",
    operation_id = "delete_quote_price_list",
    tag = "quotes",
    params(("id" = i32, Path, description = "Price list id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin, or the plan lacks the Quotes add-on (`addon`: `quotes`)", body = AddonRequiredBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_price_list(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    user.plan.require(AddonKey::Quotes)?;
    sqlx::query("UPDATE price_lists SET deleted_at = NOW() WHERE id = ? AND company_id = ?")
        .bind(id)
        .bind(user.company_id)
        .execute(&state.pool)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `StockStone` as the editor reads it.
fn stone_json(stone: &StockStone) -> Value {
    json!({
        "name": stone.name,
        "type": stone.kind,
        "retailPrice": stone.retail_price,
        "level": stone.level,
        "length": stone.length,
        "width": stone.width,
        "isDisplay": stone.is_display,
        "hasImage": stone.has_image,
        "regularStock": stone.regular_stock,
        "slabsLeft": stone.slabs_left,
    })
}

/// `StockSink` as the editor reads it.
fn sink_json(sink: &StockSink) -> Value {
    let mut value = json!({
        "name": sink.name,
        "type": sink.kind,
        "length": sink.length,
        "width": sink.width,
        "retailPrice": sink.retail_price,
    });
    for (key, field) in [
        ("cutoutType", sink.cutout_type),
        ("cutoutShape", sink.cutout_shape),
        ("faucetHoleCount", sink.faucet_hole_count),
    ] {
        if let Some(number) = field {
            value[key] = json!(number);
        }
    }
    if let Some(display) = sink.is_display {
        value["isDisplay"] = json!(display);
    }
    value
}

/// Stones in stock, by name, as the editor offers them.
fn available_stones(stock: &Stock) -> Vec<Value> {
    let mut stones: Vec<&StockStone> = stock
        .stones
        .iter()
        .filter(|s| stock.available.contains(&available_key(s)))
        .collect();
    stones.sort_by_key(|s| s.name.to_lowercase());
    stones.into_iter().map(stone_json).collect()
}

/// Inventory without the extras catalog (price lists store only their own).
async fn inventory(pool: &MySqlPool, company_id: i32) -> ApiResult<Stock> {
    let mut stock = load_stock(pool, company_id).await?;
    stock.extras.clear();
    Ok(stock)
}

/// A revision of a list.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct PriceListRevision {
    /// Revision id.
    pub id: i32,
    /// Its number in the list.
    pub revision_number: i32,
    /// When it was saved (ISO 8601).
    pub created_at: Option<String>,
    /// Who saved it.
    pub created_by: Option<String>,
    /// What changed.
    pub notes: Option<String>,
}

/// A list in the editor.
#[derive(Debug, Serialize, ToSchema)]
pub struct PriceListEditor {
    /// The list.
    pub price_list: PriceListRow,
    /// Its current prices, as saved.
    #[schema(value_type = Object)]
    pub info: Value,
    /// The prices matched to today's inventory (what the editor starts from).
    #[schema(value_type = Object)]
    pub synced_info: Value,
    /// Stones in stock and sinks in stock.
    #[schema(value_type = Object)]
    pub stock: Value,
    /// Every revision, newest first.
    pub revisions: Vec<PriceListRevision>,
}

/// Open a price list
///
/// Its prices as saved and as matched to today's inventory, the stones and
/// sinks in stock, and its history.
#[utoipa::path(
    get,
    path = "/v1/quote-price-lists/{id}",
    operation_id = "get_quote_price_list",
    tag = "quotes",
    params(("id" = i32, Path, description = "Price list id")),
    responses(
        (status = 200, description = "The list", body = PriceListEditor),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin, or the plan lacks the Quotes add-on (`addon`: `quotes`)", body = AddonRequiredBody),
        (status = 404, description = "No such list", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get_price_list(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<PriceListEditor>> {
    user.plan.require(AddonKey::Quotes)?;
    let (price_list, info) = find(&state.pool, id, user.company_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let info = info.unwrap_or_else(price_list::empty);
    let stock = inventory(&state.pool, user.company_id).await?;
    let revisions: Vec<PriceListRevision> = sqlx::query_as(
        "SELECT r.id, r.revision_number,
                DATE_FORMAT(r.created_at, '%Y-%m-%dT%H:%i:%sZ') AS created_at,
                u.name AS created_by, r.notes
           FROM price_list_revisions r
           JOIN price_lists pl ON pl.id = r.price_list_id
           LEFT JOIN users u ON u.id = r.created_by
          WHERE r.price_list_id = ? AND pl.company_id = ?
          ORDER BY r.revision_number DESC",
    )
    .bind(id)
    .bind(user.company_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(PriceListEditor {
        synced_info: sync_with_inventory(&info, &stock),
        stock: json!({
            "stones": available_stones(&stock),
            "sinks": stock.sinks.iter().map(sink_json).collect::<Vec<_>>(),
        }),
        info,
        price_list,
        revisions,
    }))
}

/// Body of saving a list.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SavedPriceList {
    /// Name, unique in the company.
    pub name: String,
    /// `active` or `inactive`.
    pub status: String,
    /// The edited prices; left out when only the name or status changed.
    #[serde(default)]
    #[schema(value_type = Option<Object>)]
    pub info: Option<Value>,
    /// What changed (up to 500 characters).
    #[serde(default)]
    pub notes: Option<String>,
}

/// Why the prices were not saved.
#[derive(Debug, Serialize, ToSchema)]
pub struct PriceListProblems {
    /// The first problem.
    pub error: String,
    /// Every problem.
    pub errors: Vec<String>,
}

/// Saves sinks' cutout settings and display onto the sinks (`saveSinkCutouts`).
async fn save_sink_cutouts(pool: &MySqlPool, company_id: i32, info: &Value) {
    for model in arr(Some(info), "arrSinkModels") {
        if truthy(model.get("custom")) {
            continue;
        }
        let number = |key: &str| get(Some(model), key).map(crate::quote_pricing::js::to_number);
        let display = get(Some(model), "isDisplay").map(|v| i32::from(is_one(v)));
        let name = model
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let _ = sqlx::query(
            "UPDATE sink_type
                SET cutout_type = ?, cutout_shape = ?, faucet_hole_count = ?,
                    is_display = COALESCE(?, is_display)
              WHERE company_id = ? AND name = ? AND is_deleted = 0",
        )
        .bind(number("sinkType"))
        .bind(number("sinkShape"))
        .bind(number("faucetHoleCount").unwrap_or(0.0))
        .bind(display)
        .bind(company_id)
        .bind(name.trim())
        .execute(pool)
        .await;
    }
}

/// `Number(v) === 1`.
fn is_one(v: &Value) -> bool {
    (crate::quote_pricing::js::to_number(v) - 1.0).abs() < f64::EPSILON
}

/// Saves colors' catalog display onto the stones (`saveStoneDisplay`).
async fn save_stone_display(pool: &MySqlPool, company_id: i32, info: &Value) -> ApiResult<()> {
    let mut shown: Vec<String> = Vec::new();
    let mut hidden: Vec<String> = Vec::new();
    for variant in arr(Some(info), "arrProducts")
        .iter()
        .flat_map(|p| arr(Some(p), "arrVariants"))
        .filter(|v| !truthy(v.get("custom")))
    {
        let name = variant
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        let Some(display) = get(Some(variant), "isDisplay") else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let list = if is_one(display) {
            &mut shown
        } else {
            &mut hidden
        };
        if !list.contains(&name) {
            list.push(name);
        }
    }
    for (is_display, names) in [(1, shown), (0, hidden)] {
        if names.is_empty() {
            continue;
        }
        let mut sql = sqlx::QueryBuilder::new("UPDATE stones SET is_display = ");
        sql.push_bind(is_display)
            .push(" WHERE company_id = ")
            .push_bind(company_id)
            .push(" AND deleted_at IS NULL AND name IN (");
        let mut list = sql.separated(", ");
        for name in names {
            list.push_bind(name);
        }
        sql.push(")");
        sql.build().execute(pool).await?;
    }
    Ok(())
}

/// Save a price list
///
/// The name and status, and, with `info`, the prices as a new revision:
/// sinks' cutouts and colors' display are saved onto the inventory, the
/// list is matched to the inventory, keeps only its own extras, and must be
/// unambiguous (else 422 with every problem).
#[utoipa::path(
    put,
    path = "/v1/quote-price-lists/{id}",
    operation_id = "save_quote_price_list",
    tag = "quotes",
    params(("id" = i32, Path, description = "Price list id")),
    request_body = SavedPriceList,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "No name, or the prices could not be read", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin, or the plan lacks the Quotes add-on (`addon`: `quotes`)", body = AddonRequiredBody),
        (status = 404, description = "No such list", body = ErrorBody),
        (status = 409, description = "The name is taken", body = ErrorBody),
        (status = 422, description = "The prices are ambiguous", body = PriceListProblems),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn save_price_list(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<SavedPriceList>,
) -> ApiResult<Response> {
    user.plan.require(AddonKey::Quotes)?;
    let (current, _) = find(&state.pool, id, user.company_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let name = input.name.trim();
    if name.is_empty() {
        return Err(ApiError::BadRequest("Give the price list a name.".into()));
    }
    if name != current.name {
        check_name(&state.pool, id, user.company_id, name).await?;
    }
    update_meta(
        &state.pool,
        id,
        user.company_id,
        Some(name),
        Some(status_of(&input.status)),
    )
    .await?;
    if let Some(raw) = input.info {
        let parsed = price_list::parse(&raw)
            .ok_or_else(|| ApiError::BadRequest("The price list could not be read.".into()))?;
        save_sink_cutouts(&state.pool, user.company_id, &parsed).await;
        save_stone_display(&state.pool, user.company_id, &parsed).await?;
        let stock = inventory(&state.pool, user.company_id).await?;
        let mut info = sync_with_inventory(&normalize(&parsed), &stock);
        info["arrOtherItems"] = Value::Array(own_extras(&info));
        let errors = validate(&info);
        if !errors.is_empty() {
            return Ok((
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(PriceListProblems {
                    error: errors[0].clone(),
                    errors,
                }),
            )
                .into_response());
        }
        let notes: String = input
            .notes
            .unwrap_or_default()
            .trim()
            .chars()
            .take(500)
            .collect();
        save_revision(
            &state.pool,
            id,
            user.company_id,
            &info,
            Some(user.id),
            (!notes.is_empty()).then_some(notes.as_str()),
        )
        .await?;
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// A restored revision.
#[derive(Debug, Serialize, ToSchema)]
pub struct RestoredRevision {
    /// The number of the revision restored.
    pub revision_number: i32,
}

/// Restore a revision
///
/// The revision's prices become a new revision.
#[utoipa::path(
    post,
    path = "/v1/quote-price-lists/{id}/revisions/{revision_id}/restore",
    operation_id = "restore_quote_price_list_revision",
    tag = "quotes",
    params(
        ("id" = i32, Path, description = "Price list id"),
        ("revision_id" = i32, Path, description = "Revision id"),
    ),
    responses(
        (status = 200, description = "Restored", body = RestoredRevision),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin, or the plan lacks the Quotes add-on (`addon`: `quotes`)", body = AddonRequiredBody),
        (status = 404, description = "No such list or revision", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn restore_revision(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path((id, revision_id)): Path<(i32, i32)>,
) -> ApiResult<Json<RestoredRevision>> {
    user.plan.require(AddonKey::Quotes)?;
    let revision: Option<(i32, i32, Option<String>)> = sqlx::query_as(
        "SELECT r.price_list_id, r.revision_number, CAST(r.info_json AS CHAR)
           FROM price_list_revisions r
           JOIN price_lists pl ON pl.id = r.price_list_id
          WHERE r.id = ? AND pl.company_id = ? AND pl.deleted_at IS NULL",
    )
    .bind(revision_id)
    .bind(user.company_id)
    .fetch_optional(&state.pool)
    .await?;
    let Some((list_id, number, Some(info))) = revision else {
        return Err(ApiError::NotFound);
    };
    let info = price_list::parse(&Value::String(info)).ok_or(ApiError::NotFound)?;
    if list_id != id {
        return Err(ApiError::NotFound);
    }
    save_revision(
        &state.pool,
        id,
        user.company_id,
        &info,
        Some(user.id),
        Some(&format!("Restored revision {number}")),
    )
    .await?
    .ok_or(ApiError::NotFound)?;
    Ok(Json(RestoredRevision {
        revision_number: number,
    }))
}
