//! `GET /v1/public/hold-music/{file}`: Granite Phone's built-in hold music,
//! fetched by Telnyx while a caller is on hold and by the app's preview
//! (original CC0 music, `api/data/hold-music/`).

use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::phone::hold_music;
use axum::extract::Path;
use axum::http::header;
use axum::response::IntoResponse;

/// Download a hold music track
///
/// The MP3 of a built-in hold music track (`calm.mp3`, `bright.mp3`,
/// `ambient.mp3`; see `GET /v1/phone/hold-music`). 404 for any other name.
#[crate::openapi::path(
    get,
    path = "/v1/public/hold-music/{file}",
    operation_id = "download_hold_music_track",
    tag = "public",
    params(("file" = String, Path, description = "The track's id with `.mp3`, e.g. `calm.mp3`")),
    responses(
        (status = 200, description = "The MP3", content_type = "audio/mpeg", body = Vec<u8>),
        (status = 404, description = "No such track", body = ErrorBody),
    )
)]
pub async fn hold_music_track(Path(file): Path<String>) -> ApiResult<impl IntoResponse> {
    let track = file
        .strip_suffix(".mp3")
        .and_then(hold_music::track)
        .ok_or(ApiError::NotFound)?;
    Ok((
        [
            (header::CONTENT_TYPE, "audio/mpeg"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        track.bytes,
    ))
}
