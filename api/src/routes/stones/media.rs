//! Admin "Edit Stone" tabs: installed photos, photo links, slab links and
//! adding slabs. Remix: `admin.stones.edit.$stone.images.tsx` and
//! `admin.stones.edit.$stone.slabs.tsx`.

use super::schemas::StoneRef;
use crate::auth::{AdminUser, EmployeeUser, ShopOrEmployeeUser};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::{JsonBody, MultiQuery};
use crate::routes::uploads::{self, schemas::UploadFolder};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, MySqlConnection, MySqlPool};
use utoipa::{IntoParams, ToSchema};

/// Most photos one request may add.
pub const MAX_IMAGES_PER_REQUEST: usize = 50;

/// An installed (project) photo shown for a stone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct StoneImage {
    /// Photo id.
    pub id: i32,
    /// Photo URL.
    pub url: Option<String>,
    /// Set for photos of a stone this one links photos from; null for the
    /// stone's own photos (only those can be deleted here).
    pub source_stone: Option<StoneRef>,
}

#[derive(FromRow)]
struct ImageRow {
    id: i32,
    url: Option<String>,
    stone_id: i32,
    stone_name: Option<String>,
}

/// Body of `POST /v1/stones/{id}/images`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AddImages {
    /// URLs of uploads to the `stones` folder (`POST /v1/uploads`), 1 to 50.
    pub urls: Vec<String>,
}

/// Body of the link routes.
#[derive(Debug, Clone, Copy, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct LinkInput {
    /// The other stone of the company.
    pub source_stone_id: i32,
}

/// Which stones link to which.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct StoneLinks {
    /// Stones this stone draws slabs from (their slabs count as its own).
    pub slab_sources: Vec<StoneRef>,
    /// Stones that draw slabs from this one.
    pub slab_dependents: Vec<StoneRef>,
    /// Stones whose installed photos this stone shows.
    pub image_sources: Vec<StoneRef>,
}

/// What a stone must have to be listed by `GET /v1/stones/names`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum StoneHas {
    /// At least one installed photo.
    Images,
    /// At least one slab in the yard (not cut, not deleted).
    Slabs,
}

/// Query string of `GET /v1/stones/names`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct StoneNamesQuery {
    /// `images` or `slabs`: only stones that have some.
    #[param(inline)]
    pub has: StoneHas,
}

/// Body of `POST /v1/stones/{id}/slabs`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewSlab {
    /// Bundle label, e.g. "B-12-7"; required.
    pub bundle: String,
    /// Length in inches; with width 0 or missing, the stone's size is used.
    #[serde(default)]
    pub length: Option<f64>,
    /// Width in inches.
    #[serde(default)]
    pub width: Option<f64>,
    /// Photo of the slab: an upload to the `stones` folder; null for none
    /// (the stone image is shown).
    #[serde(default)]
    pub image_url: Option<String>,
}

/// A slab just added.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct CreatedSlab {
    /// Slab id.
    pub id: i64,
    /// Bundle label.
    pub bundle: String,
    /// Photo URL; null when none.
    pub url: Option<String>,
    /// Length in inches as stored, e.g. "110".
    pub length: String,
    /// Width in inches as stored.
    pub width: String,
}

async fn require_stone(
    conn: &mut MySqlConnection,
    company_id: i32,
    id: i32,
) -> Result<(), ApiError> {
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT id FROM stones WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(company_id)
    .fetch_optional(conn)
    .await?;
    found.map(|_| ()).ok_or(ApiError::NotFound)
}

async fn require_other_stone(
    conn: &mut MySqlConnection,
    company_id: i32,
    id: i32,
    source: i32,
) -> Result<(), ApiError> {
    if source == id {
        return Err(ApiError::BadRequest("a stone cannot link to itself".into()));
    }
    require_stone(&mut *conn, company_id, source)
        .await
        .map_err(|_| ApiError::BadRequest(format!("stone {source} not found")))
}

async fn stone_refs(
    pool: &MySqlPool,
    sql: &'static str,
    id: i32,
    company_id: i32,
) -> Result<Vec<StoneRef>, sqlx::Error> {
    let rows: Vec<(i32, Option<String>)> = sqlx::query_as(sql)
        .bind(id)
        .bind(company_id)
        .fetch_all(pool)
        .await?;
    Ok(rows
        .into_iter()
        .map(|(id, name)| StoneRef { id, name })
        .collect())
}

/// List a stone's installed photos
///
/// Returns the stone's own installed (project) photos, by id, then those of
/// the stones it links photos from, by stone name (marked with
/// `source_stone`).
#[utoipa::path(
    get,
    path = "/v1/stones/{id}/images",
    operation_id = "list_stone_images",
    tag = "stones",
    params(("id" = i32, Path, description = "Stone id")),
    responses(
        (status = 200, description = "Own photos, then linked ones", body = Vec<StoneImage>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee, admin or of the shop", body = ErrorBody),
        (status = 404, description = "No such stone in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn images(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<Vec<StoneImage>>> {
    let mut conn = state.pool.acquire().await?;
    require_stone(&mut conn, user.company_id, id).await?;
    let rows: Vec<ImageRow> = sqlx::query_as(
        "SELECT image.id, image.url, stones.id AS stone_id, stones.name AS stone_name
         FROM installed_stones image
         JOIN stones ON stones.id = image.stone_id AND stones.company_id = ?
         WHERE image.stone_id = ?
            OR image.stone_id IN (SELECT source_stone_id FROM stone_image_links WHERE stone_id = ?)
         ORDER BY image.stone_id <> ?, stones.name ASC, image.id ASC",
    )
    .bind(user.company_id)
    .bind(id)
    .bind(id)
    .bind(id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|row| StoneImage {
                id: row.id,
                url: row.url,
                source_stone: (row.stone_id != id).then_some(StoneRef {
                    id: row.stone_id,
                    name: row.stone_name,
                }),
            })
            .collect(),
    ))
}

/// Add installed photos to a stone
///
/// Saves uploads (`POST /v1/uploads`, folder `stones`) as installed photos
/// of the stone and stores their thumbnails. Returns the new photos.
#[utoipa::path(
    post,
    path = "/v1/stones/{id}/images",
    operation_id = "add_stone_images",
    tag = "stones",
    params(("id" = i32, Path, description = "Stone id")),
    request_body = AddImages,
    responses(
        (status = 201, description = "Photos added", body = Vec<StoneImage>),
        (status = 400, description = "No URLs, more than 50, or a URL that is not an upload", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such stone in the caller's company", body = ErrorBody),
        (status = 500, description = "Database or storage error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn add_images(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
    JsonBody(body): JsonBody<AddImages>,
) -> ApiResult<(StatusCode, Json<Vec<StoneImage>>)> {
    if body.urls.is_empty() || body.urls.len() > MAX_IMAGES_PER_REQUEST {
        return Err(ApiError::BadRequest(format!(
            "send 1 to {MAX_IMAGES_PER_REQUEST} urls"
        )));
    }
    let mut tx = state.pool.begin().await?;
    require_stone(&mut tx, user.company_id, id).await?;
    for url in &body.urls {
        uploads::attach(&state.storage, url, UploadFolder::Stones).await?;
    }
    let mut added = Vec::with_capacity(body.urls.len());
    for url in body.urls {
        let result = sqlx::query("INSERT INTO installed_stones (url, stone_id) VALUES (?, ?)")
            .bind(&url)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        added.push(StoneImage {
            id: i32::try_from(result.last_insert_id()).unwrap_or(i32::MAX),
            url: Some(url),
            source_stone: None,
        });
    }
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(added)))
}

/// Delete an installed photo
///
/// Deletes one of the stone's own photos and its stored file.
#[utoipa::path(
    delete,
    path = "/v1/stones/{id}/images/{image_id}",
    operation_id = "delete_stone_image",
    tag = "stones",
    params(
        ("id" = i32, Path, description = "Stone id"),
        ("image_id" = i32, Path, description = "Photo id"),
    ),
    responses(
        (status = 204, description = "Photo deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such photo on this stone of the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_image(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path((id, image_id)): Path<(i32, i32)>,
) -> ApiResult<StatusCode> {
    let mut tx = state.pool.begin().await?;
    require_stone(&mut tx, user.company_id, id).await?;
    let url: Option<Option<String>> =
        sqlx::query_scalar("SELECT url FROM installed_stones WHERE id = ? AND stone_id = ?")
            .bind(image_id)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    let url = url.ok_or(ApiError::NotFound)?;
    sqlx::query("DELETE FROM installed_stones WHERE id = ?")
        .bind(image_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    if let Some(url) = url {
        uploads::remove(&state.storage, &url).await;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Show another stone's photos on this stone
///
/// Links the installed photos of `source_stone_id` (same company) to the
/// stone: they appear in `GET /v1/stones/{id}/images`. Linking twice is a
/// no-op.
#[utoipa::path(
    post,
    path = "/v1/stones/{id}/image-links",
    operation_id = "link_stone_images",
    tag = "stones",
    params(("id" = i32, Path, description = "Stone id")),
    request_body = LinkInput,
    responses(
        (status = 204, description = "Linked"),
        (status = 400, description = "Unknown source stone, or the stone itself", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such stone in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn link_images(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
    JsonBody(body): JsonBody<LinkInput>,
) -> ApiResult<StatusCode> {
    link(
        &state.pool,
        "stone_image_links",
        user.company_id,
        id,
        body.source_stone_id,
    )
    .await
}

/// Stop showing another stone's photos
///
/// Removes the photo link from `source_stone_id`; its photos stay on that
/// stone.
#[utoipa::path(
    delete,
    path = "/v1/stones/{id}/image-links/{source_stone_id}",
    operation_id = "unlink_stone_images",
    tag = "stones",
    params(
        ("id" = i32, Path, description = "Stone id"),
        ("source_stone_id" = i32, Path, description = "Stone whose photos were linked"),
    ),
    responses(
        (status = 204, description = "Unlinked (also when there was no link)"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such stone in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn unlink_images(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path((id, source)): Path<(i32, i32)>,
) -> ApiResult<StatusCode> {
    unlink(
        &state.pool,
        "stone_image_links",
        user.company_id,
        id,
        source,
    )
    .await
}

/// Draw slabs from another stone
///
/// Links `source_stone_id` (same company): its slabs count as this stone's
/// (lists, slabs dialog, sales). A stone cannot draw from itself or from a
/// stone that already draws from it. Linking twice is a no-op.
#[utoipa::path(
    post,
    path = "/v1/stones/{id}/slab-links",
    operation_id = "link_stone_slabs",
    tag = "stones",
    params(("id" = i32, Path, description = "Stone id")),
    request_body = LinkInput,
    responses(
        (status = 204, description = "Linked"),
        (status = 400, description = "Unknown source stone, the stone itself, or a reverse link exists", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such stone in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn link_slabs(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
    JsonBody(body): JsonBody<LinkInput>,
) -> ApiResult<StatusCode> {
    let reverse: Option<i32> = sqlx::query_scalar(
        "SELECT id FROM stone_slab_links WHERE stone_id = ? AND source_stone_id = ? LIMIT 1",
    )
    .bind(body.source_stone_id)
    .bind(id)
    .fetch_optional(&state.pool)
    .await?;
    if reverse.is_some() {
        return Err(ApiError::BadRequest(format!(
            "stone {} already draws slabs from this stone",
            body.source_stone_id
        )));
    }
    link(
        &state.pool,
        "stone_slab_links",
        user.company_id,
        id,
        body.source_stone_id,
    )
    .await
}

/// Stop drawing slabs from another stone
///
/// Removes the slab link: the source stone's slabs no longer count as this
/// stone's. Sales already made are not touched.
#[utoipa::path(
    delete,
    path = "/v1/stones/{id}/slab-links/{source_stone_id}",
    operation_id = "unlink_stone_slabs",
    tag = "stones",
    params(
        ("id" = i32, Path, description = "Stone id"),
        ("source_stone_id" = i32, Path, description = "Stone the slabs came from"),
    ),
    responses(
        (status = 204, description = "Unlinked (also when there was no link)"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such stone in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn unlink_slabs(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path((id, source)): Path<(i32, i32)>,
) -> ApiResult<StatusCode> {
    unlink(&state.pool, "stone_slab_links", user.company_id, id, source).await
}

async fn link(
    pool: &MySqlPool,
    table: &str,
    company_id: i32,
    id: i32,
    source: i32,
) -> ApiResult<StatusCode> {
    let mut tx = pool.begin().await?;
    require_stone(&mut tx, company_id, id).await?;
    require_other_stone(&mut tx, company_id, id, source).await?;
    let existing: Option<i32> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT id FROM {table} WHERE stone_id = ? AND source_stone_id = ? LIMIT 1"
    )))
    .bind(id)
    .bind(source)
    .fetch_optional(&mut *tx)
    .await?;
    if existing.is_none() {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT INTO {table} (stone_id, source_stone_id) VALUES (?, ?)"
        )))
        .bind(id)
        .bind(source)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn unlink(
    pool: &MySqlPool,
    table: &str,
    company_id: i32,
    id: i32,
    source: i32,
) -> ApiResult<StatusCode> {
    let mut conn = pool.acquire().await?;
    require_stone(&mut conn, company_id, id).await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DELETE FROM {table} WHERE stone_id = ? AND source_stone_id = ?"
    )))
    .bind(id)
    .bind(source)
    .execute(&mut *conn)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// List a stone's links
///
/// Returns the stones this one draws slabs from, the stones that draw slabs
/// from it, and the stones whose photos it shows, each by name.
#[utoipa::path(
    get,
    path = "/v1/stones/{id}/links",
    operation_id = "get_stone_links",
    tag = "stones",
    params(("id" = i32, Path, description = "Stone id")),
    responses(
        (status = 200, description = "Links of the stone", body = StoneLinks),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such stone in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn links(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<StoneLinks>> {
    require_stone(&mut *state.pool.acquire().await?, user.company_id, id).await?;
    let pool = &state.pool;
    let company = user.company_id;
    Ok(Json(StoneLinks {
        slab_sources: stone_refs(
            pool,
            "SELECT DISTINCT s.id, s.name FROM stone_slab_links l JOIN stones s ON s.id = l.source_stone_id
             WHERE l.stone_id = ? AND s.company_id = ? ORDER BY s.name, s.id",
            id,
            company,
        )
        .await?,
        slab_dependents: stone_refs(
            pool,
            "SELECT DISTINCT s.id, s.name FROM stone_slab_links l JOIN stones s ON s.id = l.stone_id
             WHERE l.source_stone_id = ? AND s.company_id = ? ORDER BY s.name, s.id",
            id,
            company,
        )
        .await?,
        image_sources: stone_refs(
            pool,
            "SELECT DISTINCT s.id, s.name FROM stone_image_links l JOIN stones s ON s.id = l.source_stone_id
             WHERE l.stone_id = ? AND s.company_id = ? ORDER BY s.name, s.id",
            id,
            company,
        )
        .await?,
    }))
}

/// List stone names
///
/// Returns the live stones of the caller's company that have installed
/// photos (`has=images`) or slabs in the yard (`has=slabs`), by name: the
/// choices for linking photos or slabs.
#[utoipa::path(
    get,
    path = "/v1/stones/names",
    operation_id = "list_stone_names",
    tag = "stones",
    params(StoneNamesQuery),
    responses(
        (status = 200, description = "Stones by name", body = Vec<StoneRef>),
        (status = 400, description = "Missing or unknown `has`", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn names(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    MultiQuery(query): MultiQuery<StoneNamesQuery>,
) -> ApiResult<Json<Vec<StoneRef>>> {
    let condition = match query.has {
        StoneHas::Images => "EXISTS (SELECT 1 FROM installed_stones i WHERE i.stone_id = s.id)",
        StoneHas::Slabs => {
            "EXISTS (SELECT 1 FROM slab_inventory si WHERE si.stone_id = s.id
               AND si.cut_date IS NULL AND si.deleted_at IS NULL)"
        }
    };
    let rows: Vec<(i32, Option<String>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT s.id, s.name FROM stones s
         WHERE s.company_id = ? AND s.deleted_at IS NULL AND {condition}
         ORDER BY s.name ASC, s.id ASC"
    )))
    .bind(user.company_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|(id, name)| StoneRef { id, name })
            .collect(),
    ))
}

/// Add a slab to a stone
///
/// Adds a slab in the yard with a bundle label, a size (the stone's size
/// when length and width are 0 or missing) and an optional photo (an upload
/// to the `stones` folder).
#[utoipa::path(
    post,
    path = "/v1/stones/{id}/slabs",
    operation_id = "add_stone_slab",
    tag = "stones",
    params(("id" = i32, Path, description = "Stone id")),
    request_body = NewSlab,
    responses(
        (status = 201, description = "Slab added", body = CreatedSlab),
        (status = 400, description = "Empty bundle, negative size, or a photo URL that is not an upload", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such stone in the caller's company", body = ErrorBody),
        (status = 500, description = "Database or storage error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn add_slab(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
    JsonBody(body): JsonBody<NewSlab>,
) -> ApiResult<(StatusCode, Json<CreatedSlab>)> {
    let bundle = body.bundle.trim().to_string();
    if bundle.is_empty() {
        return Err(ApiError::BadRequest("Bundle is required".into()));
    }
    for value in [body.length, body.width].into_iter().flatten() {
        if !value.is_finite() || value < 0.0 {
            return Err(ApiError::BadRequest(
                "Size must be a non-negative number".into(),
            ));
        }
    }
    let image_url = body
        .image_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .map(str::to_string);
    let mut tx = state.pool.begin().await?;
    let stone: Option<(Option<f32>, Option<f32>)> = sqlx::query_as(
        "SELECT length, width FROM stones WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(user.company_id)
    .fetch_optional(&mut *tx)
    .await?;
    let (stone_length, stone_width) = stone.ok_or(ApiError::NotFound)?;
    let (mut length, mut width) = (body.length.unwrap_or(0.0), body.width.unwrap_or(0.0));
    if length == 0.0 && width == 0.0 {
        length = f64::from(stone_length.unwrap_or(0.0));
        width = f64::from(stone_width.unwrap_or(0.0));
    }
    if let Some(url) = &image_url {
        uploads::attach(&state.storage, url, UploadFolder::Stones).await?;
    }
    let (length, width) = (length.to_string(), width.to_string());
    let result = sqlx::query(
        "INSERT INTO slab_inventory (bundle, stone_id, url, width, length) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&bundle)
    .bind(id)
    .bind(&image_url)
    .bind(&width)
    .bind(&length)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(CreatedSlab {
            id: i64::try_from(result.last_insert_id()).unwrap_or(i64::MAX),
            bundle,
            url: image_url,
            length,
            width,
        }),
    ))
}

/// Query of `GET /v1/stones/image-proxy`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ImageProxyQuery {
    /// Public URL of an image in the company bucket.
    pub url: String,
}

/// Fetch a stone image through the API
///
/// Returns the bytes of an image in the storage bucket so the browser can
/// draw it on a canvas (the bucket sends no CORS headers). Other URLs and
/// files that are not images are refused.
#[utoipa::path(
    get,
    path = "/v1/stones/image-proxy",
    operation_id = "proxy_stone_image",
    tag = "stones",
    params(ImageProxyQuery),
    responses(
        (status = 200, description = "The image", content_type = "image/*", body = Vec<u8>),
        (status = 400, description = "Not an image in the bucket", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such file", body = ErrorBody),
        (status = 500, description = "Storage error", body = ErrorBody),
        (status = 503, description = "No bucket configured", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn image_proxy(
    State(state): State<AppState>,
    _admin: AdminUser,
    Query(query): Query<ImageProxyQuery>,
) -> ApiResult<impl IntoResponse> {
    let disallowed = || ApiError::BadRequest("Invalid or disallowed URL".into());
    let key = state
        .storage
        .key_for_url(&query.url)
        .ok_or_else(disallowed)?;
    let key = percent_decode_str(&key).decode_utf8_lossy().into_owned();
    let object = state.storage.get(&key).await?.ok_or(ApiError::NotFound)?;
    if !object
        .content_type
        .to_ascii_lowercase()
        .starts_with("image/")
    {
        return Err(disallowed());
    }
    Ok((
        [
            (header::CONTENT_TYPE, object.content_type),
            (header::CACHE_CONTROL, "private, max-age=3600".to_string()),
        ],
        object.bytes,
    ))
}
