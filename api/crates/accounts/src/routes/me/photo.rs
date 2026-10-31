//! `PUT`/`DELETE /v1/me/photo`: the user's own picture (My Account, and the
//! phone app's profile). The file is uploaded first with `POST /v1/uploads`
//! (folder `user-photos`), shrunk by the app as every picture is.

use crate::auth::ShopOrEmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::routes::uploads::{self, schemas::UploadFolder};
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use sqlx::MySqlPool;

/// Body of `PUT /v1/me/photo`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PhotoInput {
    /// The picture uploaded to the `user-photos` folder.
    pub url: String,
}

/// The user's picture now.
#[derive(Debug, Serialize, ToSchema)]
pub struct Photo {
    /// Public URL; null without one.
    pub photo_url: Option<String>,
}

async fn current(pool: &MySqlPool, user_id: i32) -> ApiResult<Option<String>> {
    let row: Option<Option<String>> =
        sqlx::query_scalar("SELECT photo_url FROM users WHERE id = ? AND is_deleted = 0")
            .bind(user_id)
            .fetch_optional(pool)
            .await?;
    row.ok_or_else(|| ApiError::Unauthenticated("Sign in again.".into()))
}

/// Set my picture
///
/// Takes a picture uploaded to `user-photos`; the one it replaces is deleted.
#[crate::openapi::path(
    put,
    path = "/v1/me/photo",
    operation_id = "set_my_photo",
    tag = "me",
    request_body = PhotoInput,
    responses(
        (status = 200, description = "Saved", body = Photo),
        (status = 400, description = "Not a picture uploaded to user-photos", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or of the shop", body = ErrorBody),
        (status = 500, description = "Database or storage error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn set_photo(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
    JsonBody(input): JsonBody<PhotoInput>,
) -> ApiResult<Json<Photo>> {
    let url = input.url.trim().to_string();
    uploads::attach(
        &state.storage,
        &url,
        UploadFolder::UserPhotos,
        user.company_id,
    )
    .await?;
    let before = current(&state.pool, user.id).await?;
    sqlx::query("UPDATE users SET photo_url = ? WHERE id = ? AND is_deleted = 0")
        .bind(&url)
        .bind(user.id)
        .execute(&state.pool)
        .await?;
    if let Some(old) = before.filter(|old| *old != url) {
        uploads::remove(&state.storage, &old, user.company_id).await;
    }
    Ok(Json(Photo {
        photo_url: Some(url),
    }))
}

/// Remove my picture
///
/// The avatar shows the initials again.
#[crate::openapi::path(
    delete,
    path = "/v1/me/photo",
    operation_id = "delete_my_photo",
    tag = "me",
    responses(
        (status = 200, description = "Removed", body = Photo),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or of the shop", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_photo(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
) -> ApiResult<Json<Photo>> {
    let before = current(&state.pool, user.id).await?;
    sqlx::query("UPDATE users SET photo_url = NULL WHERE id = ? AND is_deleted = 0")
        .bind(user.id)
        .execute(&state.pool)
        .await?;
    if let Some(old) = before {
        uploads::remove(&state.storage, &old, user.company_id).await;
    }
    Ok(Json(Photo { photo_url: None }))
}
