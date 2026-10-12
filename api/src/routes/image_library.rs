//! The company's image library: images, sorted into folders by admins.
//!
//! - `GET    /v1/image-library`       → folders with their images, and the loose ones
//! - `POST   /v1/images`              → add an image (uploaded to `images/`)
//! - `PATCH  /v1/images/{id}`         → rename it, replace its file, or move it to a folder
//! - `DELETE /v1/images/{id}`         → remove it from the library
//! - `POST   /v1/image-folders`, `PATCH|DELETE /v1/image-folders/{id}` → folders
//!
//! Replaces the Remix `employee.images` loader and the `admin.images.*`
//! loaders and actions. `GET /v1/images` (search for pickers) is in
//! [`super::attachable`].

use super::customers::service::audit_name;
use super::uploads::{self, schemas::UploadFolder};
use crate::auth::{AdminUser, EmployeeUser};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use sqlx::MySqlPool;
use std::cmp::Ordering;
use std::collections::HashMap;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(library))
        .routes(routes!(create_image))
        .routes(routes!(update_image, delete_image))
        .routes(routes!(create_folder))
        .routes(routes!(update_folder, delete_folder))
}

/// An image of the library.
#[derive(Debug, Clone, Serialize, sqlx::FromRow, ToSchema)]
pub struct LibraryImage {
    /// Image id.
    pub id: i32,
    /// Its name.
    pub name: String,
    /// Where it is stored (its thumbnail has `-icon` before the extension).
    pub url: Option<String>,
    /// Folder it is in; null at the top level.
    pub folder_id: Option<i32>,
}

/// A folder with its images, by name.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ImageFolder {
    /// Folder id.
    pub id: i32,
    /// Its name.
    pub name: String,
    /// Its images, by name.
    pub images: Vec<LibraryImage>,
}

/// The whole library.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ImageLibrary {
    /// Folders by name.
    pub folders: Vec<ImageFolder>,
    /// Images in no folder, by name.
    pub root_images: Vec<LibraryImage>,
}

/// Body of `POST /v1/images`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewImage {
    /// Name (1–100 characters).
    pub name: String,
    /// URL of the file uploaded to the `images` folder.
    pub url: String,
    /// Folder to put it in; an unknown folder means none.
    #[serde(default)]
    pub folder_id: Option<i32>,
}

/// Body of `PATCH /v1/images/{id}`: what to change.
#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ImageChanges {
    /// New name (1–100 characters).
    #[serde(default)]
    pub name: Option<String>,
    /// A newly uploaded file replacing the old one (which is deleted).
    #[serde(default)]
    pub url: Option<String>,
    /// Folder to move it to.
    #[serde(default)]
    pub folder_id: Option<i32>,
}

/// Body of `POST /v1/image-folders` and `PATCH /v1/image-folders/{id}`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FolderInput {
    /// Name (1–255 characters).
    pub name: String,
}

/// A folder (without its images).
#[derive(Debug, Clone, Serialize, sqlx::FromRow, ToSchema)]
pub struct Folder {
    /// Folder id.
    pub id: i32,
    /// Its name.
    pub name: String,
}

/// `String.prototype.localeCompare` for names, near enough: letters before
/// case, then the exact text.
fn by_name(a: &str, b: &str) -> Ordering {
    a.to_lowercase()
        .cmp(&b.to_lowercase())
        .then_with(|| a.cmp(b))
}

fn checked_name(name: &str, max: usize) -> ApiResult<String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > max {
        return Err(ApiError::BadRequest(format!(
            "Name is required (at most {max} characters)"
        )));
    }
    Ok(name.to_string())
}

async fn live_folder(
    pool: &MySqlPool,
    company_id: i32,
    id: i32,
) -> Result<Option<Folder>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id, name FROM images_folders WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(company_id)
    .fetch_optional(pool)
    .await
}

async fn find_image(pool: &MySqlPool, company_id: i32, id: i32) -> ApiResult<LibraryImage> {
    sqlx::query_as(
        "SELECT id, COALESCE(name, '') AS name, url, folder_id FROM images
          WHERE id = ? AND company_id = ?",
    )
    .bind(id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?
    .ok_or(ApiError::NotFound)
}

/// Get the image library
///
/// Every image of the company: the live folders by name, each with its
/// images by name, and the images in no folder. Images of a deleted folder
/// are not shown.
#[utoipa::path(
    get,
    path = "/v1/image-library",
    operation_id = "get_image_library",
    tag = "catalog",
    responses(
        (status = 200, description = "The library", body = ImageLibrary),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn library(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<ImageLibrary>> {
    let folders: Vec<Folder> = sqlx::query_as(
        "SELECT id, name FROM images_folders WHERE company_id = ? AND deleted_at IS NULL",
    )
    .bind(user.company_id)
    .fetch_all(&state.pool)
    .await?;
    let images: Vec<LibraryImage> = sqlx::query_as(
        "SELECT id, COALESCE(name, '') AS name, url, folder_id FROM images WHERE company_id = ?",
    )
    .bind(user.company_id)
    .fetch_all(&state.pool)
    .await?;
    let mut by_folder: HashMap<i32, Vec<LibraryImage>> = HashMap::new();
    let mut root_images = Vec::new();
    for image in images {
        match image.folder_id {
            Some(folder) => by_folder.entry(folder).or_default().push(image),
            None => root_images.push(image),
        }
    }
    let sort = |images: &mut Vec<LibraryImage>| images.sort_by(|a, b| by_name(&a.name, &b.name));
    sort(&mut root_images);
    let mut folders: Vec<ImageFolder> = folders
        .into_iter()
        .map(|folder| {
            let mut images = by_folder.remove(&folder.id).unwrap_or_default();
            sort(&mut images);
            ImageFolder {
                id: folder.id,
                name: folder.name,
                images,
            }
        })
        .collect();
    folders.sort_by(|a, b| by_name(&a.name, &b.name));
    Ok(Json(ImageLibrary {
        folders,
        root_images,
    }))
}

/// Add an image
///
/// Puts a picture uploaded to the `images` folder (`POST /v1/uploads`) in
/// the library (its thumbnail is made now), in `folder_id` when that is a
/// live folder of the company.
#[utoipa::path(
    post,
    path = "/v1/images",
    operation_id = "create_image",
    tag = "catalog",
    request_body = NewImage,
    responses(
        (status = 201, description = "Added", body = LibraryImage),
        (status = 400, description = "No name, or not an uploaded image", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create_image(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<NewImage>,
) -> ApiResult<(StatusCode, Json<LibraryImage>)> {
    let name = checked_name(&input.name, 100)?;
    uploads::attach(&state.storage, &input.url, UploadFolder::Images).await?;
    let folder_id = match input.folder_id {
        Some(id) => live_folder(&state.pool, user.company_id, id)
            .await?
            .map(|folder| folder.id),
        None => None,
    };
    let id = sqlx::query(
        "INSERT INTO images (name, url, company_id, created_by, folder_id) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&name)
    .bind(&input.url)
    .bind(user.company_id)
    .bind(audit_name(user.name.as_deref()))
    .bind(folder_id)
    .execute(&state.pool)
    .await?
    .last_insert_id();
    Ok((
        StatusCode::CREATED,
        Json(LibraryImage {
            id: i32::try_from(id).unwrap_or(i32::MAX),
            name,
            url: Some(input.url),
            folder_id,
        }),
    ))
}

/// Edit an image
///
/// Renames it, replaces its file with a new upload (the old file and its
/// thumbnail are deleted), and/or moves it to another folder.
#[utoipa::path(
    patch,
    path = "/v1/images/{id}",
    operation_id = "update_image",
    tag = "catalog",
    params(("id" = i32, Path, description = "Image id")),
    request_body = ImageChanges,
    responses(
        (status = 200, description = "Saved", body = LibraryImage),
        (status = 400, description = "Empty name, or not an uploaded image", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such image or folder in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update_image(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
    JsonBody(changes): JsonBody<ImageChanges>,
) -> ApiResult<Json<LibraryImage>> {
    let pool = &state.pool;
    let old = find_image(pool, user.company_id, id).await?;
    let name = changes
        .name
        .as_deref()
        .map(|name| checked_name(name, 100))
        .transpose()?;
    if let Some(folder) = changes.folder_id {
        live_folder(pool, user.company_id, folder)
            .await?
            .ok_or(ApiError::NotFound)?;
    }
    let new_url = changes.url.filter(|url| Some(url) != old.url.as_ref());
    if let Some(url) = &new_url {
        uploads::attach(&state.storage, url, UploadFolder::Images).await?;
    }
    sqlx::query(
        "UPDATE images SET name = COALESCE(?, name), url = COALESCE(?, url),
                folder_id = COALESCE(?, folder_id)
          WHERE id = ? AND company_id = ?",
    )
    .bind(&name)
    .bind(&new_url)
    .bind(changes.folder_id)
    .bind(id)
    .bind(user.company_id)
    .execute(pool)
    .await?;
    if let (Some(_), Some(old_url)) = (&new_url, &old.url) {
        uploads::remove(&state.storage, old_url).await;
    }
    Ok(Json(find_image(pool, user.company_id, id).await?))
}

/// Delete an image
///
/// Takes it out of the library; the file stays in storage (emails and
/// quotes may show it).
#[utoipa::path(
    delete,
    path = "/v1/images/{id}",
    operation_id = "delete_image",
    tag = "catalog",
    params(("id" = i32, Path, description = "Image id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such image in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_image(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    let deleted = sqlx::query("DELETE FROM images WHERE id = ? AND company_id = ?")
        .bind(id)
        .bind(user.company_id)
        .execute(&state.pool)
        .await?
        .rows_affected();
    if deleted == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Add an image folder
///
/// An empty folder at the top of the library; images go in with
/// `PATCH /v1/images/{id}`.
#[utoipa::path(
    post,
    path = "/v1/image-folders",
    operation_id = "create_image_folder",
    tag = "catalog",
    request_body = FolderInput,
    responses(
        (status = 201, description = "Added", body = Folder),
        (status = 400, description = "No name", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create_folder(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<FolderInput>,
) -> ApiResult<(StatusCode, Json<Folder>)> {
    let name = checked_name(&input.name, 255)?;
    let id = sqlx::query(
        "INSERT INTO images_folders (name, url, company_id, created_by) VALUES (?, '', ?, ?)",
    )
    .bind(&name)
    .bind(user.company_id)
    .bind(audit_name(user.name.as_deref()).unwrap_or_default())
    .execute(&state.pool)
    .await?
    .last_insert_id();
    Ok((
        StatusCode::CREATED,
        Json(Folder {
            id: i32::try_from(id).unwrap_or(i32::MAX),
            name,
        }),
    ))
}

/// Rename an image folder
///
/// Its images stay in it.
#[utoipa::path(
    patch,
    path = "/v1/image-folders/{id}",
    operation_id = "update_image_folder",
    tag = "catalog",
    params(("id" = i32, Path, description = "Folder id")),
    request_body = FolderInput,
    responses(
        (status = 200, description = "Renamed", body = Folder),
        (status = 400, description = "No name", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such live folder in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update_folder(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<FolderInput>,
) -> ApiResult<Json<Folder>> {
    let name = checked_name(&input.name, 255)?;
    live_folder(&state.pool, user.company_id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    sqlx::query("UPDATE images_folders SET name = ? WHERE id = ? AND company_id = ?")
        .bind(&name)
        .bind(id)
        .bind(user.company_id)
        .execute(&state.pool)
        .await?;
    Ok(Json(Folder { id, name }))
}

/// Delete an image folder
///
/// Soft-deletes it; its images stay in the database but are no longer
/// listed.
#[utoipa::path(
    delete,
    path = "/v1/image-folders/{id}",
    operation_id = "delete_image_folder",
    tag = "catalog",
    params(("id" = i32, Path, description = "Folder id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such live folder in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_folder(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    let deleted = sqlx::query(
        "UPDATE images_folders SET deleted_at = NOW()
          WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(user.company_id)
    .execute(&state.pool)
    .await?
    .rows_affected();
    if deleted == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}
