//! One transaction (sale) as the Transactions pages open it, and the work
//! on its slabs: cutting, recording remnants, swapping and adding slabs.
//!
//! - `GET  /v1/transactions/{id}`                         → the sale, its slabs and sinks
//! - `POST /v1/transactions/{id}/slabs/{slab_id}/cut`     → mark a slab cut
//! - `POST /v1/transactions/{id}/slabs/{slab_id}/uncut`   → and back
//! - `POST /v1/transactions/{id}/slabs/{slab_id}/partial-cut` → record a cut piece
//! - `POST /v1/transactions/{id}/slabs/{slab_id}/remove`  → take a slab off the sale
//! - `POST /v1/transactions/{id}/slabs/{slab_id}/replace` → swap it for a free slab
//! - `PUT  /v1/transactions/{id}/slabs/{slab_id}/square-feet` → a room's square feet
//! - `POST /v1/transactions/{id}/slabs`                   → add a free slab to a room
//! - `POST /v1/transactions/{id}/rooms/remove`            → clear a room name
//!
//! Cutting from the employee page only marks the slab; the admin and shop
//! pages (`with_remnants`) also tidy the slab's remnants as the Remix
//! `admin|shop.transactions.edit` actions did. Everything but cut and uncut
//! is for admins and the shop.

use crate::auth::ShopOrEmployeeUser;
use crate::auth::session::{SessionUser, user_positions};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::serde_helpers::js_date;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{MySqlConnection, MySqlPool};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(detail))
        .routes(routes!(cut))
        .routes(routes!(uncut))
        .routes(routes!(partial_cut))
        .routes(routes!(remove))
        .routes(routes!(replace))
        .routes(routes!(square_feet))
        .routes(routes!(add))
        .routes(routes!(remove_room))
}

/// Shop manager and shop worker positions.
const SHOP_POSITIONS: [i32; 2] = [3, 4];

async fn can_rework(pool: &MySqlPool, user: &SessionUser) -> Result<bool, sqlx::Error> {
    if user.is_admin || user.is_superuser {
        return Ok(true);
    }
    Ok(user_positions(pool, user.id)
        .await?
        .iter()
        .any(|row| row.company_id == user.company_id && SHOP_POSITIONS.contains(&row.position_id)))
}

async fn rework_only(pool: &MySqlPool, user: &SessionUser) -> ApiResult<()> {
    if can_rework(pool, user).await? {
        Ok(())
    } else {
        Err(ApiError::Forbidden)
    }
}

async fn own_sale(pool: &MySqlPool, user: &SessionUser, sale_id: i32) -> ApiResult<()> {
    let found: Option<i32> =
        sqlx::query_scalar("SELECT id FROM sales WHERE id = ? AND company_id = ?")
            .bind(sale_id)
            .bind(user.company_id)
            .fetch_optional(pool)
            .await?;
    found.map(|_| ()).ok_or(ApiError::NotFound)
}

/// The sale's header.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct SaleHeader {
    /// Sale id.
    pub id: i32,
    /// Customer id.
    pub customer_id: i32,
    /// Customer name.
    pub customer_name: Option<String>,
    /// When it was sold.
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub sale_date: Option<DateTime<Utc>>,
    /// Seller id.
    pub seller_id: i32,
    /// Seller name.
    pub seller_name: Option<String>,
    /// Project address.
    pub project_address: Option<String>,
}

/// A slab on the sale.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct SaleSlabRow {
    /// Slab id.
    pub id: i64,
    /// Stone id.
    pub stone_id: i32,
    /// Bundle.
    pub bundle: Option<String>,
    /// Stone name.
    pub stone_name: Option<String>,
    /// When it was cut.
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub cut_date: Option<DateTime<Utc>>,
    /// Notes.
    pub notes: Option<String>,
    /// Square feet of its room.
    pub square_feet: Option<f64>,
    /// Length (inches, as entered).
    pub length: Option<String>,
    /// Width (inches, as entered).
    pub width: Option<String>,
    /// Room id (hex).
    pub room_uuid: Option<String>,
    /// Room name.
    pub room: Option<String>,
    /// The slab it was cut from.
    pub parent_id: Option<i64>,
    /// Live pieces cut from it.
    pub child_count: i64,
}

/// A sink on the sale.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct SaleSinkRow {
    /// Sink unit id.
    pub id: i64,
    /// Sink model id.
    pub sink_type_id: i32,
    /// Model name.
    pub name: Option<String>,
    /// Price.
    pub price: Option<f64>,
    /// Always false (deleted units are left out).
    pub is_deleted: bool,
    /// Slab it is cut into.
    pub slab_id: i64,
    /// That slab's room.
    pub room: Option<String>,
    /// That slab's room id (hex).
    pub room_uuid: Option<String>,
}

/// A transaction as its dialog shows it.
#[derive(Debug, Serialize, ToSchema)]
pub struct TransactionDetail {
    /// The sale.
    pub sale: SaleHeader,
    /// Its slabs, in order.
    pub slabs: Vec<SaleSlabRow>,
    /// Its sinks.
    pub sinks: Vec<SaleSinkRow>,
}

/// Get a transaction
///
/// The sale with its customer and seller, its slabs (with their cut state,
/// size, room and remnants) and its sinks.
#[utoipa::path(
    get,
    path = "/v1/transactions/{id}",
    operation_id = "get_transaction",
    tag = "sales",
    params(("id" = i32, Path, description = "Sale id")),
    responses(
        (status = 200, description = "The transaction", body = TransactionDetail),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or of the shop", body = ErrorBody),
        (status = 404, description = "No such sale (with a customer and seller) in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn detail(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<TransactionDetail>> {
    let pool = &state.pool;
    let sale: SaleHeader = sqlx::query_as(
        "SELECT s.id, s.customer_id, c.name AS customer_name, s.sale_date, s.seller_id,
                u.name AS seller_name, s.project_address
           FROM sales s
           JOIN customers c ON s.customer_id = c.id
           JOIN users u ON s.seller_id = u.id
          WHERE s.id = ? AND s.company_id = ?",
    )
    .bind(id)
    .bind(user.company_id)
    .fetch_optional(pool)
    .await?
    .ok_or(ApiError::NotFound)?;
    let slabs = sqlx::query_as(
        "SELECT CAST(si.id AS SIGNED) AS id, si.stone_id, si.bundle, st.name AS stone_name, si.cut_date, si.notes,
                CAST(si.square_feet AS DOUBLE) AS square_feet, si.length, si.width,
                HEX(si.room_uuid) AS room_uuid, si.room, CAST(si.parent_id AS SIGNED) AS parent_id,
                (SELECT COUNT(*) FROM slab_inventory c WHERE c.parent_id = si.id AND c.deleted_at IS NULL) AS child_count
           FROM slab_inventory si JOIN stones st ON si.stone_id = st.id
          WHERE si.sale_id = ?
          ORDER BY si.id",
    )
    .bind(id)
    .fetch_all(pool)
    .await?;
    let sinks = sqlx::query_as(
        "SELECT CAST(sk.id AS SIGNED) AS id, sk.sink_type_id, t.name, CAST(sk.price AS DOUBLE) AS price,
                sk.is_deleted <> 0 AS is_deleted, CAST(sk.slab_id AS SIGNED) AS slab_id, si.room, HEX(si.room_uuid) AS room_uuid
           FROM sinks sk
           JOIN sink_type t ON sk.sink_type_id = t.id
           JOIN slab_inventory si ON sk.slab_id = si.id
          WHERE si.sale_id = ? AND sk.is_deleted = 0
          ORDER BY sk.id",
    )
    .bind(id)
    .fetch_all(pool)
    .await?;
    Ok(Json(TransactionDetail { sale, slabs, sinks }))
}

/// `partially cut` while a live slab of the sale is uncut, else `cut`.
async fn refresh_status(tx: &mut MySqlConnection, sale_id: i32) -> Result<(), sqlx::Error> {
    let uncut: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM slab_inventory WHERE sale_id = ? AND cut_date IS NULL AND deleted_at IS NULL",
    )
    .bind(sale_id)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("UPDATE sales SET status = ? WHERE id = ?")
        .bind(if uncut > 0 { "partially cut" } else { "cut" })
        .bind(sale_id)
        .execute(&mut *tx)
        .await?;
    Ok(())
}

/// The slab of the sale: its parent, size and stone.
#[derive(Debug, sqlx::FromRow)]
struct SlabRow {
    cut: bool,
    stone_id: i32,
    parent_id: Option<i64>,
    length: Option<String>,
    width: Option<String>,
}

async fn slab_of_sale(tx: &mut MySqlConnection, sale_id: i32, slab_id: i32) -> ApiResult<SlabRow> {
    sqlx::query_as(
        "SELECT cut_date IS NOT NULL AS cut, stone_id, CAST(parent_id AS SIGNED) AS parent_id, length, width
           FROM slab_inventory WHERE id = ? AND sale_id = ?",
    )
    .bind(slab_id)
    .bind(sale_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(ApiError::NotFound)
}

/// Body of the cut and uncut operations.
#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CutOptions {
    /// Admins and the shop: on cut, drop the slab's unsold remnants; on
    /// uncut, give the whole size back to its parent and remnants.
    #[serde(default)]
    pub with_remnants: bool,
}

/// Mark a slab cut
///
/// Stamps the slab's cut date (once) and updates the sale's status; with
/// `with_remnants` (admins and the shop) its unsold remnants are deleted.
#[utoipa::path(
    post,
    path = "/v1/transactions/{id}/slabs/{slab_id}/cut",
    operation_id = "cut_sale_slab",
    tag = "sales",
    params(("id" = i32, Path, description = "Sale id"), ("slab_id" = i32, Path, description = "Slab id")),
    request_body = CutOptions,
    responses(
        (status = 204, description = "Cut"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or of the shop (`with_remnants`: an admin or of the shop)", body = ErrorBody),
        (status = 404, description = "No such slab on a sale of the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn cut(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
    Path((id, slab_id)): Path<(i32, i32)>,
    JsonBody(options): JsonBody<CutOptions>,
) -> ApiResult<StatusCode> {
    let pool = &state.pool;
    own_sale(pool, &user, id).await?;
    if options.with_remnants {
        rework_only(pool, &user).await?;
    }
    let mut tx = pool.begin().await?;
    let slab = slab_of_sale(&mut tx, id, slab_id).await?;
    if !slab.cut {
        sqlx::query("UPDATE slab_inventory SET cut_date = CURRENT_TIMESTAMP WHERE id = ?")
            .bind(slab_id)
            .execute(&mut *tx)
            .await?;
        if options.with_remnants {
            sqlx::query(
                "DELETE FROM slab_inventory WHERE parent_id = ? AND sale_id IS NULL AND deleted_at IS NULL",
            )
            .bind(slab_id)
            .execute(&mut *tx)
            .await?;
        }
    }
    refresh_status(&mut tx, id).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Mark a slab uncut
///
/// Clears the slab's cut date and updates the sale's status. With
/// `with_remnants` (admins and the shop) the slab's full size (its own when
/// it is a remnant, else its stone's) goes back to its parent and to its
/// pieces: sold pieces take it and unsold ones are deleted, or else the
/// first unsold piece takes it and the others are deleted.
#[utoipa::path(
    post,
    path = "/v1/transactions/{id}/slabs/{slab_id}/uncut",
    operation_id = "uncut_sale_slab",
    tag = "sales",
    params(("id" = i32, Path, description = "Sale id"), ("slab_id" = i32, Path, description = "Slab id")),
    request_body = CutOptions,
    responses(
        (status = 204, description = "Uncut"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or of the shop (`with_remnants`: an admin or of the shop)", body = ErrorBody),
        (status = 404, description = "No such slab on a sale of the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn uncut(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
    Path((id, slab_id)): Path<(i32, i32)>,
    JsonBody(options): JsonBody<CutOptions>,
) -> ApiResult<StatusCode> {
    let pool = &state.pool;
    own_sale(pool, &user, id).await?;
    if options.with_remnants {
        rework_only(pool, &user).await?;
    }
    let mut tx = pool.begin().await?;
    let slab = slab_of_sale(&mut tx, id, slab_id).await?;
    if slab.cut {
        sqlx::query("UPDATE slab_inventory SET cut_date = NULL WHERE id = ?")
            .bind(slab_id)
            .execute(&mut *tx)
            .await?;
        if options.with_remnants {
            restore_size(&mut tx, slab_id, &slab).await?;
        }
    }
    refresh_status(&mut tx, id).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The uncut size bookkeeping of `admin.transactions.edit`'s `uncut-slab`.
async fn restore_size(
    tx: &mut MySqlConnection,
    slab_id: i32,
    slab: &SlabRow,
) -> Result<(), sqlx::Error> {
    let (length, width) = if let Some(parent) = slab.parent_id {
        sqlx::query("UPDATE slab_inventory SET length = ?, width = ? WHERE id = ?")
            .bind(&slab.length)
            .bind(&slab.width)
            .bind(parent)
            .execute(&mut *tx)
            .await?;
        (slab.length.clone(), slab.width.clone())
    } else {
        let stone: Option<(Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT CAST(length AS CHAR), CAST(width AS CHAR) FROM stones WHERE id = ?",
        )
        .bind(slab.stone_id)
        .fetch_optional(&mut *tx)
        .await?;
        stone.unwrap_or_else(|| (slab.length.clone(), slab.width.clone()))
    };
    let children: Vec<(i64, bool)> = sqlx::query_as(
        "SELECT CAST(id AS SIGNED), sale_id IS NOT NULL FROM slab_inventory WHERE parent_id = ? ORDER BY id",
    )
    .bind(slab_id)
    .fetch_all(&mut *tx)
    .await?;
    let sold: Vec<i64> = children.iter().filter(|c| c.1).map(|c| c.0).collect();
    let unsold: Vec<i64> = children.iter().filter(|c| !c.1).map(|c| c.0).collect();
    let resize = |child: i64| {
        sqlx::query("UPDATE slab_inventory SET length = ?, width = ? WHERE id = ?")
            .bind(length.clone())
            .bind(width.clone())
            .bind(child)
    };
    if sold.is_empty() {
        if let Some(first) = unsold.first() {
            resize(*first).execute(&mut *tx).await?;
            if unsold.len() > 1 {
                sqlx::query("DELETE FROM slab_inventory WHERE parent_id = ? AND sale_id IS NULL AND id <> ?")
                    .bind(slab_id)
                    .bind(first)
                    .execute(&mut *tx)
                    .await?;
            }
        }
    } else {
        for child in sold {
            resize(child).execute(&mut *tx).await?;
        }
        if !unsold.is_empty() {
            sqlx::query("DELETE FROM slab_inventory WHERE parent_id = ? AND sale_id IS NULL")
                .bind(slab_id)
                .execute(&mut *tx)
                .await?;
        }
    }
    Ok(())
}

/// Every column of a slab a piece cut from it copies.
#[derive(Debug, Clone, sqlx::FromRow)]
struct FullSlab {
    id: i64,
    stone_id: i32,
    bundle: Option<String>,
    url: Option<String>,
    parent_id: Option<i64>,
    sale_id: Option<i32>,
    notes: Option<String>,
    price: Option<String>,
    square_feet: Option<String>,
    room: Option<String>,
    room_uuid: Option<Vec<u8>>,
    seam: Option<String>,
    backsplash: Option<String>,
    tear_out: Option<String>,
    stove: Option<String>,
    ten_year_sealer: Option<i64>,
    waterfall: Option<String>,
    corbels: Option<i64>,
    extras: Option<String>,
    edge: Option<String>,
    cut_date: Option<DateTime<Utc>>,
}

const FULL_COLUMNS: &str = "CAST(si.id AS SIGNED) AS id, si.stone_id, si.bundle, si.url, CAST(si.parent_id AS SIGNED) AS parent_id,
    si.sale_id, si.notes, CAST(si.price AS CHAR) AS price, CAST(si.square_feet AS CHAR) AS square_feet,
    si.room, si.room_uuid, si.seam, si.backsplash, si.tear_out, si.stove,
    CAST(si.ten_year_sealer AS SIGNED) AS ten_year_sealer, si.waterfall,
    CAST(si.corbels AS SIGNED) AS corbels, CAST(si.extras AS CHAR) AS extras, si.edge, si.cut_date";

async fn full_slab(
    tx: &mut MySqlConnection,
    condition: &str,
    id: i64,
) -> Result<Option<FullSlab>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {FULL_COLUMNS} FROM slab_inventory si WHERE {condition} LIMIT 1"
    )))
    .bind(id)
    .fetch_optional(&mut *tx)
    .await
}

/// Body of `POST /v1/transactions/{id}/slabs/{slab_id}/partial-cut`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PartialCut {
    /// Length of the piece.
    pub length: f64,
    /// Width of the piece.
    pub width: f64,
    /// More pieces follow: the slab is not marked cut yet.
    #[serde(default)]
    pub add_another: bool,
    /// Resize the first piece instead of adding one.
    #[serde(default)]
    pub replace_first: bool,
}

/// Record a cut piece
///
/// Records the size of a piece cut from the slab (the
/// `admin|shop.transactions.edit` `partial-cut` bookkeeping): it resizes the
/// first piece with `replace_first`, else adds a piece copying the slab's
/// sale details; unless `add_another`, the slab is then marked cut.
#[utoipa::path(
    post,
    path = "/v1/transactions/{id}/slabs/{slab_id}/partial-cut",
    operation_id = "partial_cut_sale_slab",
    tag = "sales",
    params(("id" = i32, Path, description = "Sale id"), ("slab_id" = i32, Path, description = "Slab id")),
    request_body = PartialCut,
    responses(
        (status = 204, description = "Recorded"),
        (status = 400, description = "Length or width not above zero", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin or of the shop", body = ErrorBody),
        (status = 404, description = "No such slab on a sale of the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
#[allow(clippy::too_many_lines)]
pub async fn partial_cut(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
    Path((id, slab_id)): Path<(i32, i32)>,
    JsonBody(input): JsonBody<PartialCut>,
) -> ApiResult<StatusCode> {
    let pool = &state.pool;
    own_sale(pool, &user, id).await?;
    rework_only(pool, &user).await?;
    if !(input.length.is_finite()
        && input.width.is_finite()
        && input.length > 0.0
        && input.width > 0.0)
    {
        return Err(ApiError::BadRequest(
            "Length and width must be above zero".into(),
        ));
    }
    let mut tx = pool.begin().await?;
    let slab: Option<FullSlab> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {FULL_COLUMNS} FROM slab_inventory si WHERE si.id = ? AND si.sale_id = ?"
    )))
    .bind(slab_id)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let slab = slab.ok_or(ApiError::NotFound)?;
    let sold_child = full_slab(
        &mut tx,
        "si.parent_id = ? AND si.deleted_at IS NULL AND si.sale_id IS NOT NULL ORDER BY si.id",
        i64::from(slab_id),
    )
    .await?;
    let candidate = match &sold_child {
        Some(child) => Some(child.clone()),
        None => {
            full_slab(
                &mut tx,
                "si.parent_id = ? AND si.deleted_at IS NULL ORDER BY (si.sale_id IS NOT NULL) DESC, si.id",
                i64::from(slab_id),
            )
            .await?
        }
    };
    let parent_in_sale = match slab.parent_id {
        Some(parent) => {
            full_slab(
                &mut tx,
                "si.id = ? AND si.deleted_at IS NULL AND si.sale_id IS NOT NULL",
                parent,
            )
            .await?
        }
        None => None,
    };
    // The slab itself (on this sale) when nothing else fits.
    let target = sold_child
        .or(candidate)
        .or(parent_in_sale)
        .unwrap_or_else(|| slab.clone());
    let (length, width) = (input.length, input.width);
    let resize = |slab: i64| {
        sqlx::query("UPDATE slab_inventory SET length = ?, width = ? WHERE id = ?")
            .bind(length)
            .bind(width)
            .bind(slab)
    };
    if input.replace_first {
        resize(target.id).execute(&mut *tx).await?;
    } else {
        let parent_for_new = if target.sale_id.is_none() {
            target.parent_id.unwrap_or(target.id)
        } else {
            i64::from(slab_id)
        };
        sqlx::query(
            "INSERT INTO slab_inventory (stone_id, bundle, parent_id, sale_id, length, width, url, notes,
                    price, square_feet, room, room_uuid, seam, backsplash, tear_out, stove,
                    ten_year_sealer, waterfall, corbels, extras, edge)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(target.stone_id)
        .bind(&target.bundle)
        .bind(parent_for_new)
        .bind(target.sale_id)
        .bind(length)
        .bind(width)
        .bind(&target.url)
        .bind(&target.notes)
        .bind(&target.price)
        .bind(&target.square_feet)
        .bind(&target.room)
        .bind(&target.room_uuid)
        .bind(&target.seam)
        .bind(&target.backsplash)
        .bind(&target.tear_out)
        .bind(&target.stove)
        .bind(target.ten_year_sealer)
        .bind(&target.waterfall)
        .bind(target.corbels)
        .bind(&target.extras)
        .bind(&target.edge)
        .execute(&mut *tx)
        .await?;
    }
    if !input.add_another {
        sqlx::query(
            "UPDATE slab_inventory SET cut_date = CURRENT_TIMESTAMP WHERE id = ? AND sale_id = ?",
        )
        .bind(slab_id)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    refresh_status(&mut tx, id).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Clears a slab's sale details and takes it off the sale (`removeOldSlabFromSale`).
async fn off_sale(tx: &mut MySqlConnection, slab_id: i32, sale_id: i32) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE slab_inventory
            SET sale_id = NULL, notes = NULL, price = NULL, square_feet = NULL, cut_date = NULL,
                room = NULL, waterfall = NULL, corbels = NULL, seam = NULL, stove = NULL, extras = NULL,
                room_uuid = NULL, edge = NULL, backsplash = NULL, tear_out = NULL, ten_year_sealer = NULL
          WHERE id = ? AND sale_id = ?",
    )
    .bind(slab_id)
    .bind(sale_id)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// Take a slab off a sale
///
/// Deletes the slab's unsold remnants and frees it (its room and sale
/// details are cleared).
#[utoipa::path(
    post,
    path = "/v1/transactions/{id}/slabs/{slab_id}/remove",
    operation_id = "remove_sale_slab",
    tag = "sales",
    params(("id" = i32, Path, description = "Sale id"), ("slab_id" = i32, Path, description = "Slab id")),
    responses(
        (status = 204, description = "Removed"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin or of the shop", body = ErrorBody),
        (status = 404, description = "No such slab on a sale of the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn remove(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
    Path((id, slab_id)): Path<(i32, i32)>,
) -> ApiResult<StatusCode> {
    let pool = &state.pool;
    own_sale(pool, &user, id).await?;
    rework_only(pool, &user).await?;
    let mut tx = pool.begin().await?;
    slab_of_sale(&mut tx, id, slab_id).await?;
    sqlx::query("DELETE FROM slab_inventory WHERE parent_id = ? AND sale_id IS NULL")
        .bind(slab_id)
        .execute(&mut *tx)
        .await?;
    off_sale(&mut tx, slab_id, id).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// A free slab of the company's stones, for adding or swapping in.
async fn free_slab(tx: &mut MySqlConnection, company_id: i32, slab_id: i32) -> ApiResult<()> {
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM slab_inventory si JOIN stones st ON st.id = si.stone_id
          WHERE si.id = ? AND si.sale_id IS NULL AND si.deleted_at IS NULL AND st.company_id = ?",
    )
    .bind(slab_id)
    .bind(company_id)
    .fetch_optional(&mut *tx)
    .await?;
    found
        .map(|_| ())
        .ok_or_else(|| ApiError::BadRequest("Pick a free slab of the company".into()))
}

/// Body of `POST /v1/transactions/{id}/slabs/{slab_id}/replace`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Replacement {
    /// The free slab to put on the sale instead.
    pub new_slab_id: i32,
}

/// Swap a slab
///
/// Puts a free slab on the sale with the old slab's room and sale details
/// (and cut state), deletes the old slab's unsold remnants and frees it.
#[utoipa::path(
    post,
    path = "/v1/transactions/{id}/slabs/{slab_id}/replace",
    operation_id = "replace_sale_slab",
    tag = "sales",
    params(("id" = i32, Path, description = "Sale id"), ("slab_id" = i32, Path, description = "Slab id")),
    request_body = Replacement,
    responses(
        (status = 204, description = "Swapped"),
        (status = 400, description = "The new slab is not a free slab of the company", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin or of the shop", body = ErrorBody),
        (status = 404, description = "No such slab on a sale of the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn replace(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
    Path((id, slab_id)): Path<(i32, i32)>,
    JsonBody(input): JsonBody<Replacement>,
) -> ApiResult<StatusCode> {
    let pool = &state.pool;
    own_sale(pool, &user, id).await?;
    rework_only(pool, &user).await?;
    let mut tx = pool.begin().await?;
    let old: Option<FullSlab> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {FULL_COLUMNS} FROM slab_inventory si WHERE si.id = ? AND si.sale_id = ?"
    )))
    .bind(slab_id)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let old = old.ok_or(ApiError::NotFound)?;
    free_slab(&mut tx, user.company_id, input.new_slab_id).await?;
    sqlx::query(
        "UPDATE slab_inventory
            SET sale_id = ?, room = ?, room_uuid = ?, seam = ?, backsplash = ?, tear_out = ?,
                square_feet = ?, stove = ?, ten_year_sealer = ?, waterfall = ?, corbels = ?, price = ?,
                extras = ?, edge = ?, notes = ?, cut_date = ?
          WHERE id = ? AND sale_id IS NULL",
    )
    .bind(id)
    .bind(&old.room)
    .bind(&old.room_uuid)
    .bind(&old.seam)
    .bind(&old.backsplash)
    .bind(&old.tear_out)
    .bind(&old.square_feet)
    .bind(&old.stove)
    .bind(old.ten_year_sealer)
    .bind(&old.waterfall)
    .bind(old.corbels)
    .bind(&old.price)
    .bind(&old.extras)
    .bind(&old.edge)
    .bind(&old.notes)
    .bind(old.cut_date)
    .bind(input.new_slab_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM slab_inventory WHERE parent_id = ? AND sale_id IS NULL")
        .bind(slab_id)
        .execute(&mut *tx)
        .await?;
    off_sale(&mut tx, slab_id, id).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Body of `PUT /v1/transactions/{id}/slabs/{slab_id}/square-feet`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SquareFeet {
    /// The room's square feet; null to clear.
    pub square_feet: Option<f64>,
}

/// Set a slab's square feet
///
/// The square feet of the room the slab is in, as the shop measured it.
#[utoipa::path(
    put,
    path = "/v1/transactions/{id}/slabs/{slab_id}/square-feet",
    operation_id = "set_sale_slab_square_feet",
    tag = "sales",
    params(("id" = i32, Path, description = "Sale id"), ("slab_id" = i32, Path, description = "Slab id")),
    request_body = SquareFeet,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Not a number", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin or of the shop", body = ErrorBody),
        (status = 404, description = "No such slab on a sale of the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn square_feet(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
    Path((id, slab_id)): Path<(i32, i32)>,
    JsonBody(input): JsonBody<SquareFeet>,
) -> ApiResult<StatusCode> {
    let pool = &state.pool;
    own_sale(pool, &user, id).await?;
    rework_only(pool, &user).await?;
    if input.square_feet.is_some_and(|value| !value.is_finite()) {
        return Err(ApiError::BadRequest("Square feet must be a number".into()));
    }
    let mut tx = pool.begin().await?;
    slab_of_sale(&mut tx, id, slab_id).await?;
    sqlx::query("UPDATE slab_inventory SET square_feet = ? WHERE id = ? AND sale_id = ?")
        .bind(input.square_feet)
        .bind(slab_id)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    refresh_status(&mut tx, id).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Body of `POST /v1/transactions/{id}/slabs`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AddedSlab {
    /// A free slab of the company.
    pub slab_id: i32,
    /// The room it goes to.
    pub room: String,
    /// That room's id (hex), when it has one.
    #[serde(default)]
    pub room_uuid: Option<String>,
}

/// Add a slab to a room
///
/// Puts a free slab on the sale in the room, with the details of the
/// room's first slab (its seam, edge, price...).
#[utoipa::path(
    post,
    path = "/v1/transactions/{id}/slabs",
    operation_id = "add_sale_slab",
    tag = "sales",
    params(("id" = i32, Path, description = "Sale id")),
    request_body = AddedSlab,
    responses(
        (status = 204, description = "Added"),
        (status = 400, description = "No room, a bad room id, or not a free slab of the company", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin or of the shop", body = ErrorBody),
        (status = 404, description = "No such sale in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn add(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<AddedSlab>,
) -> ApiResult<StatusCode> {
    let pool = &state.pool;
    own_sale(pool, &user, id).await?;
    rework_only(pool, &user).await?;
    let room = input.room.trim();
    if room.is_empty() {
        return Err(ApiError::BadRequest("Pick a room".into()));
    }
    let room_uuid = match input
        .room_uuid
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
    {
        Some(hex) => {
            Some(hex_bytes(hex).ok_or_else(|| ApiError::BadRequest("Invalid room id".into()))?)
        }
        None => None,
    };
    let mut tx = pool.begin().await?;
    free_slab(&mut tx, user.company_id, input.slab_id).await?;
    let template: Option<FullSlab> = match &room_uuid {
        Some(uuid) => {
            sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "SELECT {FULL_COLUMNS} FROM slab_inventory si WHERE si.sale_id = ? AND si.room_uuid = ? ORDER BY si.id LIMIT 1"
            )))
            .bind(id)
            .bind(uuid)
            .fetch_optional(&mut *tx)
            .await?
        }
        None => {
            sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "SELECT {FULL_COLUMNS} FROM slab_inventory si WHERE si.sale_id = ? AND si.room = ? ORDER BY si.id LIMIT 1"
            )))
            .bind(id)
            .bind(room)
            .fetch_optional(&mut *tx)
            .await?
        }
    };
    let room_uuid = template
        .as_ref()
        .and_then(|t| t.room_uuid.clone())
        .or(room_uuid);
    let t = template.as_ref();
    sqlx::query(
        "UPDATE slab_inventory
            SET sale_id = ?, room = ?, room_uuid = ?, seam = ?, backsplash = ?, tear_out = ?,
                square_feet = ?, stove = ?, ten_year_sealer = ?, waterfall = ?, corbels = ?, price = ?,
                extras = ?, edge = ?, notes = NULL, cut_date = NULL
          WHERE id = ? AND sale_id IS NULL",
    )
    .bind(id)
    .bind(room)
    .bind(&room_uuid)
    .bind(t.and_then(|t| t.seam.clone()))
    .bind(t.and_then(|t| t.backsplash.clone()))
    .bind(t.and_then(|t| t.tear_out.clone()))
    .bind(t.and_then(|t| t.square_feet.clone()))
    .bind(t.and_then(|t| t.stove.clone()))
    .bind(t.and_then(|t| t.ten_year_sealer))
    .bind(t.and_then(|t| t.waterfall.clone()))
    .bind(t.and_then(|t| t.corbels))
    .bind(t.and_then(|t| t.price.clone()))
    .bind(t.and_then(|t| t.extras.clone()))
    .bind(t.and_then(|t| t.edge.clone()))
    .bind(input.slab_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

fn hex_bytes(hex: &str) -> Option<Vec<u8>> {
    if hex.len() != 32 {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
        .collect()
}

/// Body of `POST /v1/transactions/{id}/rooms/remove`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RoomName {
    /// The room name to clear from the sale's slabs.
    pub room: String,
}

/// Clear a room
///
/// Takes the room name off every slab of the sale in that room.
#[utoipa::path(
    post,
    path = "/v1/transactions/{id}/rooms/remove",
    operation_id = "remove_sale_room",
    tag = "sales",
    params(("id" = i32, Path, description = "Sale id")),
    request_body = RoomName,
    responses(
        (status = 204, description = "Cleared"),
        (status = 400, description = "No room", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin or of the shop", body = ErrorBody),
        (status = 404, description = "No such sale in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn remove_room(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<RoomName>,
) -> ApiResult<StatusCode> {
    let pool = &state.pool;
    own_sale(pool, &user, id).await?;
    rework_only(pool, &user).await?;
    if input.room.is_empty() {
        return Err(ApiError::BadRequest("Pick a room".into()));
    }
    sqlx::query("UPDATE slab_inventory SET room = NULL WHERE sale_id = ? AND room = ?")
        .bind(id)
        .bind(&input.room)
        .execute(pool)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
