//! AI Design: the stones a kitchen can be shown in, and the designs.
//!
//! - `GET /v1/ai/design/stones` → in-stock stones with a picture
//! - `POST /v1/ai/design` → the kitchen with each stone installed, streamed
//!   as newline-delimited JSON (`status`, `design` / `stoneError` per stone,
//!   `ping` every 10 s, then `done` or `error`)
//!
//! A design takes a minute or more: the browser calls this on the streaming
//! Function URL. Replaces the Remix `api.ai-design` route.

use crate::ai::Caller;
use crate::auth::CurrentUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::kitchen_design::{self, DesignStone, MAX_KITCHEN_BYTES, MAX_STONES, Prepared};
use crate::state::AppState;
use axum::Json;
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;
use utoipa::{IntoParams, ToSchema};

/// Query of `GET /v1/ai/design/stones`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct StonesQuery {
    /// Part of the stone's name.
    pub q: Option<String>,
}

/// A stone AI Design can show.
#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct DesignStoneRow {
    /// The stone.
    pub id: i64,
    /// Its name.
    pub name: String,
    /// Its material (granite, quartz…).
    #[sqlx(rename = "type")]
    #[serde(rename = "type")]
    pub kind: Option<String>,
}

/// The stones.
#[derive(Debug, Serialize, ToSchema)]
pub struct DesignStones {
    /// By name.
    pub stones: Vec<DesignStoneRow>,
}

/// List stones for AI Design
///
/// The company's displayed stones that have a picture and are in stock
/// (a free slab, their own or a linked stone's, or regular stock).
#[utoipa::path(
    get,
    path = "/v1/ai/design/stones",
    operation_id = "list_ai_design_stones",
    tag = "ai",
    params(StonesQuery),
    responses(
        (status = 200, description = "Stones", body = DesignStones),
        (status = 401, description = "No valid session", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn stones(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(query): Query<StonesQuery>,
) -> ApiResult<Json<DesignStones>> {
    let search = query.q.as_deref().map(str::trim).unwrap_or_default();
    let filter = if search.is_empty() {
        ""
    } else {
        "AND UPPER(s.name) LIKE UPPER(?)"
    };
    let sql = format!(
        "SELECT CAST(s.id AS SIGNED) AS id, s.name, s.type
           FROM stones s
           LEFT JOIN (
             SELECT id AS stone_id, id AS source_stone_id FROM stones WHERE company_id = ?
             UNION
             SELECT stone_id, source_stone_id FROM stone_slab_links
           ) slab_sources ON slab_sources.stone_id = s.id
           LEFT JOIN slab_inventory AS si ON si.stone_id = slab_sources.source_stone_id
          WHERE s.company_id = ? AND s.deleted_at IS NULL AND s.is_display = 1
            AND s.url IS NOT NULL AND TRIM(s.url) != ''
            {filter}
          GROUP BY s.id, s.name, s.type, s.regular_stock
         HAVING (
           CAST(SUM(CASE WHEN si.id IS NOT NULL AND si.sale_id IS NULL AND si.cut_date IS NULL AND si.deleted_at IS NULL THEN 1 ELSE 0 END) AS UNSIGNED) > 0
           OR s.regular_stock = 1
         )
          ORDER BY s.name ASC"
    );
    let mut rows = sqlx::query_as::<_, DesignStoneRow>(sqlx::AssertSqlSafe(sql))
        .bind(user.company_id)
        .bind(user.company_id);
    if !search.is_empty() {
        rows = rows.bind(format!("%{search}%"));
    }
    Ok(Json(DesignStones {
        stones: rows.fetch_all(&state.pool).await?,
    }))
}

/// Body of `POST /v1/ai/design`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DesignInput {
    /// The kitchen photo as a `data:image/...;base64,` URL (at most 15 MB).
    pub kitchen: String,
    /// The stones to show it in (one to three).
    pub stone_ids: Vec<i64>,
    /// `countertops` (default), `fireplace`, `full_height_backsplash`.
    #[serde(default)]
    pub surfaces: Vec<String>,
    /// What else to change, in the user's words.
    #[serde(default)]
    pub instructions: String,
}

/// The kitchen photo's bytes from its data URL.
fn kitchen_bytes(data_url: &str) -> Result<Vec<u8>, ApiError> {
    let invalid = || ApiError::BadRequest("No kitchen image provided.".into());
    let rest = data_url.strip_prefix("data:").ok_or_else(invalid)?;
    let (meta, data) = rest.split_once(',').ok_or_else(invalid)?;
    let mime = meta.split(';').next().unwrap_or_default();
    if !mime.starts_with("image/") || !meta.ends_with(";base64") {
        return Err(ApiError::BadRequest("Only image files are allowed.".into()));
    }
    // Base64 of 15 MB.
    if data.len() > MAX_KITCHEN_BYTES / 3 * 4 + 4 {
        return Err(ApiError::BadRequest(
            "Image is too large (max 15MB).".into(),
        ));
    }
    let bytes = STANDARD.decode(data.trim()).map_err(|_| invalid())?;
    if bytes.len() > MAX_KITCHEN_BYTES {
        return Err(ApiError::BadRequest(
            "Image is too large (max 15MB).".into(),
        ));
    }
    Ok(bytes)
}

/// Design a kitchen
///
/// The kitchen photo with each stone installed on the surfaces asked for,
/// as newline-delimited JSON: `{"type":"status","status":"generating"}`,
/// then per stone `{"type":"design","stoneId","stoneName","image"}` (a
/// `data:` URL) or `{"type":"stoneError","stoneName","error"}`, `{"type":
/// "ping"}` every 10 s, and finally `{"type":"done","count"}` or
/// `{"type":"error","error"}` when no stone worked.
#[utoipa::path(
    post,
    path = "/v1/ai/design",
    operation_id = "design_kitchen",
    tag = "ai",
    request_body = DesignInput,
    responses(
        (status = 200, description = "The designs, as newline-delimited JSON", content_type = "application/x-ndjson", body = String),
        (status = 400, description = "No usable photo or stone", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn design(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    JsonBody(input): JsonBody<DesignInput>,
) -> ApiResult<Response> {
    let kitchen = kitchen_bytes(&input.kitchen)?;
    let mut ids: Vec<i64> = Vec::new();
    for id in input.stone_ids {
        if id > 0 && !ids.contains(&id) {
            ids.push(id);
        }
    }
    if ids.is_empty() {
        return Err(ApiError::BadRequest("Select a stone.".into()));
    }
    if ids.len() > MAX_STONES {
        return Err(ApiError::BadRequest(format!(
            "Maximum {MAX_STONES} stone per request."
        )));
    }
    let marks = vec!["?"; ids.len()].join(",");
    let mut stones_q = sqlx::query_as::<_, DesignStone>(sqlx::AssertSqlSafe(format!(
        "SELECT CAST(id AS SIGNED) AS id, name, url, type FROM stones
          WHERE company_id = ? AND deleted_at IS NULL AND id IN ({marks})"
    )))
    .bind(user.company_id);
    for id in &ids {
        stones_q = stones_q.bind(id);
    }
    let stones: Vec<DesignStone> = stones_q
        .fetch_all(&state.pool)
        .await?
        .into_iter()
        .filter(|s| s.url.as_deref().is_some_and(|u| !u.trim().is_empty()))
        .collect();
    if stones.is_empty() {
        return Err(ApiError::BadRequest(
            "The selected stones do not have a reference image.".into(),
        ));
    }
    let surfaces = kitchen_design::parse_surfaces(&input.surfaces);
    let prepared = tokio::task::spawn_blocking(move || kitchen_design::prepare_kitchen(&kitchen))
        .await
        .map_err(|err| ApiError::Internal(err.to_string()))?
        .map_err(|_| ApiError::BadRequest("Could not read the kitchen image.".into()))?;
    let (tx, rx) = mpsc::channel::<String>(64);
    tokio::spawn(run(
        state,
        user.id,
        user.company_id,
        Arc::new(prepared),
        stones,
        surfaces,
        input.instructions,
        tx,
    ));
    let body = Body::from_stream(ReceiverStream::new(rx).map(Ok::<_, Infallible>));
    Ok((
        [
            (header::CONTENT_TYPE, "application/x-ndjson"),
            (header::CACHE_CONTROL, "no-cache, no-transform"),
            (header::HeaderName::from_static("x-accel-buffering"), "no"),
        ],
        body,
    )
        .into_response())
}

#[allow(clippy::too_many_arguments, clippy::needless_pass_by_value)]
async fn run(
    state: AppState,
    user_id: i32,
    company_id: i32,
    kitchen: Arc<Prepared>,
    stones: Vec<DesignStone>,
    surfaces: Vec<kitchen_design::Surface>,
    instructions: String,
    tx: mpsc::Sender<String>,
) {
    let send = |value: serde_json::Value| {
        let tx = tx.clone();
        async move {
            let _ = tx.send(kitchen_design::event(&value)).await;
        }
    };
    send(json!({ "type": "status", "status": "generating" })).await;
    let pings = {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(10));
            interval.tick().await;
            loop {
                interval.tick().await;
                if tx
                    .send(kitchen_design::event(&json!({ "type": "ping" })))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        })
    };
    let mut jobs = tokio::task::JoinSet::new();
    for stone in stones {
        let (state, kitchen, surfaces, instructions) = (
            state.clone(),
            Arc::clone(&kitchen),
            surfaces.clone(),
            instructions.clone(),
        );
        jobs.spawn(async move {
            let caller = Caller {
                feature: "ai_design",
                user_id: Some(user_id),
                company_id: Some(company_id),
            };
            let result =
                kitchen_design::design(&state, caller, &kitchen, &stone, &surfaces, &instructions)
                    .await;
            (stone, result)
        });
    }
    let mut count = 0;
    let mut first_error: Option<String> = None;
    while let Some(joined) = jobs.join_next().await {
        let Ok((stone, result)) = joined else {
            continue;
        };
        match result {
            Ok(image) => {
                count += 1;
                send(json!({ "type": "design", "stoneId": stone.id, "stoneName": stone.name, "image": image })).await;
            }
            Err(error) => {
                lambda_http::tracing::warn!(stone = stone.id, %error, "AI design generation failed for a stone");
                first_error.get_or_insert_with(|| error.clone());
                send(json!({ "type": "stoneError", "stoneName": stone.name, "error": error }))
                    .await;
            }
        }
    }
    pings.abort();
    if count == 0 {
        send(json!({
            "type": "error",
            "error": first_error.unwrap_or_else(|| "Could not generate a design. Please try again.".into()),
        }))
        .await;
    } else {
        send(json!({ "type": "done", "count": count })).await;
    }
}
