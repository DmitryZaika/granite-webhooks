//! Writes on stones: create, edit, delete (admins) and sample counts
//! (employees and shop staff).
//!
//! Remix: the actions of `admin.stones.add.tsx`,
//! `admin.stones.edit.$stone.information.tsx`, `admin.stones.delete.$stone.tsx`
//! and `api.stones.$stoneId.samples.tsx`.

use crate::auth::{AdminUser, ShopOrEmployeeUser};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::routes::uploads::{self, schemas::UploadFolder};
use crate::sql::push_in;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use sqlx::{MySql, MySqlConnection, QueryBuilder};
use utoipa::{IntoParams, ToSchema};

/// Surface finishes a stone can have.
pub const FINISHES: &[&str] = &["polished", "leathered", "honed"];

/// Body of `POST /v1/stones` and `PUT /v1/stones/{id}`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct StoneInput {
    /// Display name; required.
    pub name: String,
    /// Material; one of the company's stone types (`GET /v1/stone-types`).
    #[serde(rename = "type")]
    #[schema(rename = "type")]
    pub kind: String,
    /// `polished`, `leathered` or `honed`.
    pub finishing: String,
    /// Image: the `url` of an upload to the `stones` folder
    /// (`POST /v1/uploads`). Required when creating; when editing, omit it
    /// to keep the current image.
    #[serde(default)]
    pub image_url: Option<String>,
    /// Shown in the catalogs.
    #[serde(default)]
    pub is_display: bool,
    /// Marked as on sale.
    #[serde(default)]
    pub on_sale: bool,
    /// Always orderable, never sold out.
    #[serde(default)]
    pub regular_stock: bool,
    /// Supplier id of the company (`GET /v1/suppliers`); null for none.
    #[serde(default)]
    pub supplier_id: Option<i32>,
    /// Typical slab length in inches.
    #[serde(default)]
    pub length: Option<f64>,
    /// Typical slab width in inches.
    #[serde(default)]
    pub width: Option<f64>,
    /// Cost per square foot, whole dollars.
    #[serde(default)]
    pub cost_per_sqft: Option<i32>,
    /// Retail price per square foot, whole dollars; 0 = priced by slab.
    #[serde(default)]
    pub retail_price: Option<i32>,
    /// Price level 1 to 7; null for none.
    #[serde(default)]
    pub level: Option<i32>,
    /// Supplier bundle number.
    #[serde(default)]
    pub bundle_number: Option<String>,
    /// Where the bundle is in the yard.
    #[serde(default)]
    pub bundle_location: Option<String>,
    /// Expected delivery, `YYYY-MM-DD`; null for none.
    #[serde(default)]
    pub delivery_date: Option<String>,
    /// Colors to tag the stone with (`GET /v1/colors`); replaces the old ones.
    #[serde(default)]
    pub color_ids: Vec<i32>,
}

/// Response of `POST /v1/stones`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
pub struct CreatedStone {
    /// Id of the new stone.
    pub id: i32,
}

/// Query string of `DELETE /v1/stones/{id}`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct DeleteStoneQuery {
    /// `true` also hides the stone's slabs in the yard. Without it, a stone
    /// that still has slabs is not deleted (409).
    #[serde(default)]
    pub include_slabs: bool,
}

fn non_empty(value: Option<&String>) -> Option<&str> {
    value.map(|v| v.trim()).filter(|v| !v.is_empty())
}

async fn validate(
    conn: &mut MySqlConnection,
    company_id: i32,
    input: &StoneInput,
) -> Result<(), ApiError> {
    if input.name.trim().is_empty() || input.name.trim().chars().count() > 100 {
        return Err(ApiError::BadRequest(
            "Name is required (100 characters at most)".into(),
        ));
    }
    if !FINISHES.contains(&input.finishing.as_str()) {
        return Err(ApiError::BadRequest(format!(
            "invalid finishing: {}",
            input.finishing
        )));
    }
    if input.level.is_some_and(|level| !(1..=7).contains(&level)) {
        return Err(ApiError::BadRequest("level must be 1 to 7".into()));
    }
    for (name, value) in [("length", input.length), ("width", input.width)] {
        if value.is_some_and(|v| !v.is_finite() || v < 0.0) {
            return Err(ApiError::BadRequest(format!(
                "{name} must be a non-negative number"
            )));
        }
    }
    if let Some(date) = non_empty(input.delivery_date.as_ref())
        && chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").is_err()
    {
        return Err(ApiError::BadRequest(format!(
            "invalid delivery_date: {date}"
        )));
    }
    let has_type: Option<i64> = sqlx::query_scalar(
        "SELECT CAST(id AS SIGNED) FROM stone_types WHERE company_id = ? AND LOWER(name) = LOWER(?) LIMIT 1",
    )
    .bind(company_id)
    .bind(input.kind.trim())
    .fetch_optional(&mut *conn)
    .await?;
    if has_type.is_none() {
        return Err(ApiError::BadRequest("Select a valid stone type.".into()));
    }
    if let Some(supplier_id) = input.supplier_id {
        let found: Option<i32> =
            sqlx::query_scalar("SELECT id FROM suppliers WHERE id = ? AND company_id = ?")
                .bind(supplier_id)
                .bind(company_id)
                .fetch_optional(&mut *conn)
                .await?;
        if found.is_none() {
            return Err(ApiError::BadRequest(format!(
                "supplier {supplier_id} not found"
            )));
        }
    }
    if !input.color_ids.is_empty() {
        let mut query = QueryBuilder::<MySql>::new("SELECT id FROM colors WHERE 1 = 1");
        push_in(&mut query, "id", &input.color_ids);
        let found: Vec<i32> = query.build_query_scalar().fetch_all(&mut *conn).await?;
        if let Some(missing) = input.color_ids.iter().find(|id| !found.contains(id)) {
            return Err(ApiError::BadRequest(format!("color {missing} not found")));
        }
    }
    Ok(())
}

async fn set_colors(
    conn: &mut MySqlConnection,
    stone_id: i32,
    color_ids: &[i32],
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM stone_colors WHERE stone_id = ?")
        .bind(stone_id)
        .execute(&mut *conn)
        .await?;
    let mut seen = Vec::new();
    for &color_id in color_ids {
        if seen.contains(&color_id) {
            continue;
        }
        seen.push(color_id);
        sqlx::query("INSERT INTO stone_colors (stone_id, color_id) VALUES (?, ?)")
            .bind(stone_id)
            .bind(color_id)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

/// Add a stone
///
/// Creates a stone of the caller's company with its image (an upload to the
/// `stones` folder, thumbnail made here) and colors. Add its slabs with
/// `POST /v1/stones/{id}/slabs`.
#[utoipa::path(
    post,
    path = "/v1/stones",
    operation_id = "create_stone",
    tag = "stones",
    request_body = StoneInput,
    responses(
        (status = 201, description = "Stone created", body = CreatedStone),
        (status = 400, description = "Missing name or image, unknown type, supplier or color, bad finishing, level, size or date", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database or storage error", body = ErrorBody),
        (status = 503, description = "Uploads are not configured", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<StoneInput>,
) -> ApiResult<(StatusCode, Json<CreatedStone>)> {
    let image_url = non_empty(input.image_url.as_ref())
        .ok_or_else(|| ApiError::BadRequest("An image is required".into()))?
        .to_string();
    let mut tx = state.pool.begin().await?;
    validate(&mut tx, user.company_id, &input).await?;
    uploads::attach(&state.storage, &image_url, UploadFolder::Stones).await?;
    let result = sqlx::query(
        "INSERT INTO stones (name, type, finishing, url, company_id, is_display, on_sale,
            regular_stock, supplier_id, width, length, cost_per_sqft, retail_price, level,
            bundle_number, bundle_location, delivery_date)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(input.name.trim())
    .bind(input.kind.trim())
    .bind(&input.finishing)
    .bind(&image_url)
    .bind(user.company_id)
    .bind(input.is_display)
    .bind(input.on_sale)
    .bind(input.regular_stock)
    .bind(input.supplier_id)
    .bind(input.width.unwrap_or(0.0))
    .bind(input.length.unwrap_or(0.0))
    .bind(input.cost_per_sqft.unwrap_or(0))
    .bind(input.retail_price.unwrap_or(0))
    .bind(input.level)
    .bind(non_empty(input.bundle_number.as_ref()))
    .bind(non_empty(input.bundle_location.as_ref()))
    .bind(non_empty(input.delivery_date.as_ref()))
    .execute(&mut *tx)
    .await?;
    let id = i32::try_from(result.last_insert_id()).unwrap_or(i32::MAX);
    set_colors(&mut tx, id, &input.color_ids).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(CreatedStone { id })))
}

/// Edit a stone
///
/// Saves every field of the body on a stone of the caller's company and
/// replaces its colors. With a new `image_url` the image is replaced and the
/// old file deleted; without one the image is kept.
#[utoipa::path(
    put,
    path = "/v1/stones/{id}",
    operation_id = "update_stone",
    tag = "stones",
    params(("id" = i32, Path, description = "Stone id")),
    request_body = StoneInput,
    responses(
        (status = 204, description = "Stone saved"),
        (status = 400, description = "Invalid field (see `create_stone`)", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such stone in the caller's company", body = ErrorBody),
        (status = 500, description = "Database or storage error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<StoneInput>,
) -> ApiResult<StatusCode> {
    let mut tx = state.pool.begin().await?;
    let current: Option<Option<String>> = sqlx::query_scalar(
        "SELECT url FROM stones WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(user.company_id)
    .fetch_optional(&mut *tx)
    .await?;
    let current_url = current.ok_or(ApiError::NotFound)?;
    validate(&mut tx, user.company_id, &input).await?;
    let new_url = non_empty(input.image_url.as_ref())
        .filter(|url| Some(*url) != current_url.as_deref())
        .map(str::to_string);
    if let Some(url) = &new_url {
        uploads::attach(&state.storage, url, UploadFolder::Stones).await?;
    }
    sqlx::query(
        "UPDATE stones SET name = ?, type = ?, finishing = ?, url = COALESCE(?, url),
            is_display = ?, regular_stock = ?, supplier_id = ?, length = ?, width = ?,
            on_sale = ?, cost_per_sqft = ?, level = ?, retail_price = ?, bundle_number = ?,
            bundle_location = ?, delivery_date = ?
         WHERE id = ? AND company_id = ?",
    )
    .bind(input.name.trim())
    .bind(input.kind.trim())
    .bind(&input.finishing)
    .bind(&new_url)
    .bind(input.is_display)
    .bind(input.regular_stock)
    .bind(input.supplier_id)
    .bind(input.length)
    .bind(input.width)
    .bind(input.on_sale)
    .bind(input.cost_per_sqft)
    .bind(input.level)
    .bind(input.retail_price)
    .bind(non_empty(input.bundle_number.as_ref()))
    .bind(non_empty(input.bundle_location.as_ref()))
    .bind(non_empty(input.delivery_date.as_ref()))
    .bind(id)
    .bind(user.company_id)
    .execute(&mut *tx)
    .await?;
    set_colors(&mut tx, id, &input.color_ids).await?;
    tx.commit().await?;
    if let (Some(_), Some(old)) = (&new_url, &current_url) {
        uploads::remove(&state.storage, old).await;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Delete a stone
///
/// Hides a stone of the caller's company from every list (soft delete;
/// nothing is erased). A stone that still has slabs in the yard needs
/// `include_slabs=true`, which hides those slabs too.
#[utoipa::path(
    delete,
    path = "/v1/stones/{id}",
    operation_id = "delete_stone",
    tag = "stones",
    params(("id" = i32, Path, description = "Stone id"), DeleteStoneQuery),
    responses(
        (status = 204, description = "Stone deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such stone in the caller's company", body = ErrorBody),
        (status = 409, description = "The stone has slabs and `include_slabs` is not set", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
    Query(query): Query<DeleteStoneQuery>,
) -> ApiResult<StatusCode> {
    let mut tx = state.pool.begin().await?;
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT id FROM stones WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(user.company_id)
    .fetch_optional(&mut *tx)
    .await?;
    found.ok_or(ApiError::NotFound)?;
    let slabs: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM slab_inventory WHERE stone_id = ? AND deleted_at IS NULL AND cut_date IS NULL",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    if slabs > 0 && !query.include_slabs {
        return Err(ApiError::Conflict(
            "Confirm removing associated slabs".into(),
        ));
    }
    sqlx::query("UPDATE slab_inventory SET deleted_at = CURRENT_TIMESTAMP WHERE stone_id = ? AND deleted_at IS NULL")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE stones SET deleted_at = CURRENT_TIMESTAMP WHERE id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Body of `PATCH /v1/stones/{id}/samples`; at least one field.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SamplesInput {
    /// Samples on hand, 0 or more.
    #[serde(default)]
    pub amount: Option<i32>,
    /// How many samples to keep: 1 (few) to 3 (many), 4 = none needed.
    #[serde(default)]
    pub importance: Option<i32>,
}

/// Set a stone's samples
///
/// Saves the samples on hand and/or how many to keep for a stone of the
/// caller's company.
#[utoipa::path(
    patch,
    path = "/v1/stones/{id}/samples",
    operation_id = "update_stone_samples",
    tag = "stones",
    params(("id" = i32, Path, description = "Stone id")),
    request_body = SamplesInput,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Negative amount, importance not 1-4, or nothing to save", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or shop staff", body = ErrorBody),
        (status = 404, description = "No such stone in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update_samples(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<SamplesInput>,
) -> ApiResult<StatusCode> {
    if input.amount.is_some_and(|amount| amount < 0) {
        return Err(ApiError::BadRequest("Invalid amount".into()));
    }
    if input
        .importance
        .is_some_and(|importance| !(1..=4).contains(&importance))
    {
        return Err(ApiError::BadRequest("Invalid importance".into()));
    }
    if input.amount.is_none() && input.importance.is_none() {
        return Err(ApiError::BadRequest("Nothing to update".into()));
    }
    let found = sqlx::query(
        "UPDATE stones SET samples_amount = COALESCE(?, samples_amount),
                samples_importance = COALESCE(?, samples_importance)
          WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(input.amount)
    .bind(input.importance)
    .bind(id)
    .bind(user.company_id)
    .execute(&state.pool)
    .await?
    .rows_affected();
    if found == 0 {
        let exists: Option<i32> = sqlx::query_scalar(
            "SELECT id FROM stones WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
        )
        .bind(id)
        .bind(user.company_id)
        .fetch_optional(&state.pool)
        .await?;
        exists.ok_or(ApiError::NotFound)?;
    }
    Ok(StatusCode::NO_CONTENT)
}
