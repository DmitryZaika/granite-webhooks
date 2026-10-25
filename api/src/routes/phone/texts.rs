//! The app's Texts tab (device token): my Telnyx texts as threads of one of
//! my lines and one other number, sending from a line, read markers and
//! media links. See `crate::phone::texts`; the sends go through
//! `crate::phone::texting::deliver`, like the CRM's.

use super::{bad_request, denied, rate_limited};
use crate::auth::device::DeviceUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::{JsonBody, MultiQuery};
use crate::phone::numbers::{is_nanp, last10, to_e164};
use crate::phone::texting;
use crate::phone::texts::{self, TextMessage, TextThread, ThreadFilter, ThreadKey};
use crate::serde_helpers::js_date_required;
use crate::state::AppState;
use crate::routes::sms::send::{SendSmsRequest, SmsFile, check};
use crate::webhooks_client::{OutgoingSms, OutgoingSmsFile};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

/// Threads per page by default, and at most.
const DEFAULT_THREADS: i64 = 30;
/// Texts per page by default.
const DEFAULT_MESSAGES: i64 = 50;
const MAX_PAGE: i64 = 100;
/// How long a media link works.
const MEDIA_LINK_SECS: i64 = 300;

/// Query of `GET /v1/phone/texts/threads`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ThreadsQuery {
    /// `nextBefore` of the previous page.
    pub before: Option<String>,
    /// Threads per page, 1 to 100 (default 30).
    pub limit: Option<i64>,
    /// Only the threads with this other number (E.164 or 10 digits).
    pub number: Option<String>,
}

/// `GET /v1/phone/texts/threads`.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TextThreadList {
    /// Newest activity first.
    pub threads: Vec<TextThread>,
    /// `before` for the next page; null on the last.
    pub next_before: Option<String>,
    /// My unread texts over all my threads.
    pub unread: i64,
}

/// Query of `GET /v1/phone/texts/threads/{key}/messages`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct MessagesQuery {
    /// `nextBefore` of the previous page (a text id).
    pub before: Option<String>,
    /// Texts per page, 1 to 100 (default 50).
    pub limit: Option<i64>,
}

/// `GET /v1/phone/texts/threads/{key}/messages`.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TextMessageList {
    /// Newest first.
    pub messages: Vec<TextMessage>,
    /// `before` for the next (older) page; null on the last.
    pub next_before: Option<String>,
    /// The thread.
    pub thread: TextThread,
}

/// `GET /v1/phone/texts/media/{id}`.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MediaLink {
    /// A link to the file (works 5 minutes).
    pub url: String,
    /// When the link stops working (RFC 3339).
    #[serde(serialize_with = "js_date_required")]
    pub expires_at: DateTime<Utc>,
}

/// Body of `POST /v1/phone/texts`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SendText {
    /// The line to text from: one of mine (`ourNumber.id` of a thread, or a
    /// caller ID's `numberId`).
    pub our_number_id: i32,
    /// The other number: E.164 or 10 digits (US and Canada only).
    pub to: String,
    /// The text, up to 1600 characters; may be empty with files.
    #[serde(default)]
    pub text: String,
    /// Up to ten files made ready by `POST /v1/sms/attachments` (pictures go
    /// as MMS, other files as links), as for the CRM's texts.
    #[serde(default)]
    pub attachments: Vec<SmsFile>,
}

/// 403 `texting_disabled` unless the company texts through Telnyx.
async fn texting_on(state: &AppState, company_id: i32) -> ApiResult<()> {
    texting::texting_setup(&state.pool, company_id)
        .await?
        .map(|_| ())
        .ok_or_else(|| denied("texting_disabled"))
}

fn limit(value: Option<i64>, default: i64) -> ApiResult<i64> {
    let limit = value.unwrap_or(default);
    if (1..=MAX_PAGE).contains(&limit) {
        Ok(limit)
    } else {
        Err(bad_request("limit must be 1 to 100"))
    }
}

fn cursor(value: Option<&str>) -> ApiResult<Option<i64>> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| {
            value
                .parse::<i64>()
                .ok()
                .filter(|id| *id > 0)
                .ok_or_else(|| bad_request("invalid before"))
        })
        .transpose()
}

/// The thread of `key` when it is on one of my lines and has texts: 404
/// otherwise.
async fn my_thread(
    state: &AppState,
    device: &DeviceUser,
    key: ThreadKey,
) -> ApiResult<(ThreadKey, TextThread, Vec<i32>)> {
    let user = &device.user;
    let lines = texts::my_lines(&state.pool, user.company_id, user.id).await?;
    if !lines.contains_key(&key.number_id) {
        return Err(ApiError::NotFound);
    }
    let filter = ThreadFilter {
        key: Some(key.clone()),
        ..ThreadFilter::default()
    };
    let (mut threads, _) =
        texts::threads(&state.pool, user.company_id, user.id, &lines, &filter, 1).await?;
    let thread = threads.pop().ok_or(ApiError::NotFound)?;
    Ok((key, thread, lines.keys().copied().collect()))
}

/// List my text threads
///
/// The conversations of the numbers I may call from, one per line and other
/// number, newest activity first (page back with `before` = `nextBefore`),
/// optionally only those with one other `number`. Each has our line, the
/// other number, the customer with it, the newest text and my unread count;
/// `unread` totals every thread. 403 `texting_disabled` unless the company
/// texts through Telnyx. Next: `GET /v1/phone/texts/threads/{key}/messages`.
#[utoipa::path(
    get,
    path = "/v1/phone/texts/threads",
    operation_id = "list_phone_text_threads",
    tag = "phone",
    params(ThreadsQuery),
    responses(
        (status = 200, description = "A page of threads", body = TextThreadList),
        (status = 400, description = "Bad `limit` or `before`, or `number` is not a phone (`invalid_phone`)", body = ErrorBody),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 403, description = "`texting_disabled`", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn list_text_threads(
    State(state): State<AppState>,
    device: DeviceUser,
    MultiQuery(query): MultiQuery<ThreadsQuery>,
) -> ApiResult<Json<TextThreadList>> {
    let user = &device.user;
    texting_on(&state, user.company_id).await?;
    let limit = limit(query.limit, DEFAULT_THREADS)?;
    let before = cursor(query.before.as_deref())?;
    let other10 = match query
        .number
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
    {
        Some(number) => Some(
            to_e164(number)
                .and_then(|e164| last10(&e164))
                .ok_or_else(|| bad_request("invalid_phone"))?,
        ),
        None => None,
    };
    let pool = &state.pool;
    let lines = texts::my_lines(pool, user.company_id, user.id).await?;
    let filter = ThreadFilter {
        other10,
        before,
        ..ThreadFilter::default()
    };
    let (threads, next_before) =
        texts::threads(pool, user.company_id, user.id, &lines, &filter, limit).await?;
    let unread = texts::unread_total(pool, user.company_id, user.id).await?;
    Ok(Json(TextThreadList {
        threads,
        next_before,
        unread,
    }))
}

/// Get a text thread's texts
///
/// The newest texts of one of my threads (newest first; page back with
/// `before` = `nextBefore`), each with its pictures and files (links from
/// `GET /v1/phone/texts/media/{id}`), status, error and sender, and the
/// thread itself. 404 for a thread that is not one of my lines' (or has no
/// texts). Opening it does not mark it read: `POST .../read` does.
#[utoipa::path(
    get,
    path = "/v1/phone/texts/threads/{key}/messages",
    operation_id = "list_phone_text_messages",
    tag = "phone",
    params(("key" = String, Path, description = "The thread's key"), MessagesQuery),
    responses(
        (status = 200, description = "A page of texts and the thread", body = TextMessageList),
        (status = 400, description = "Bad `limit` or `before`", body = ErrorBody),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 403, description = "`texting_disabled`", body = ErrorBody),
        (status = 404, description = "Not one of my threads", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn list_text_messages(
    State(state): State<AppState>,
    device: DeviceUser,
    Path(key): Path<String>,
    MultiQuery(query): MultiQuery<MessagesQuery>,
) -> ApiResult<Json<TextMessageList>> {
    texting_on(&state, device.user.company_id).await?;
    let limit = limit(query.limit, DEFAULT_MESSAGES)?;
    let before = cursor(query.before.as_deref())?;
    let key = ThreadKey::parse(&key).ok_or(ApiError::NotFound)?;
    let (key, thread, _) = my_thread(&state, &device, key).await?;
    let (messages, next_before) =
        texts::messages(&state.pool, device.user.company_id, &key, before, limit).await?;
    Ok(Json(TextMessageList {
        messages,
        next_before,
        thread,
    }))
}

/// Get the thread of a text
///
/// The thread a text is in (one of my lines and one other number), as
/// `GET /v1/phone/texts/threads` lists it: its `key`, the other number, the
/// customer, the newest text and my unread count. The app's new-text
/// notification starts here: the `text` push names only the text
/// (`textId`), never the number or the thread. 404 for a text that is not
/// on one of my lines (or another company's). Device token only.
#[utoipa::path(
    get,
    path = "/v1/phone/texts/messages/{id}/thread",
    operation_id = "get_phone_text_thread",
    tag = "phone",
    params(("id" = String, Path, description = "The text's id (`textId` of a `text` push, or a text's `id`)")),
    responses(
        (status = 200, description = "The thread", body = TextThread),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 403, description = "`texting_disabled`", body = ErrorBody),
        (status = 404, description = "Not a text of my lines", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn text_thread(
    State(state): State<AppState>,
    device: DeviceUser,
    Path(id): Path<String>,
) -> ApiResult<Json<TextThread>> {
    let company_id = device.user.company_id;
    texting_on(&state, company_id).await?;
    let id: i64 = id.trim().parse().map_err(|_| ApiError::NotFound)?;
    let key = texts::thread_of(&state.pool, company_id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let (_, thread, _) = my_thread(&state, &device, key).await?;
    Ok(Json(thread))
}

/// Link to a text's picture or file
///
/// A 5-minute link to a picture or file of a text on one of my lines. 404
/// for one that is not.
#[utoipa::path(
    get,
    path = "/v1/phone/texts/media/{id}",
    operation_id = "get_phone_text_media",
    tag = "phone",
    params(("id" = String, Path, description = "The file's id (`media[].id` of a text)")),
    responses(
        (status = 200, description = "A link", body = MediaLink),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 403, description = "`texting_disabled`", body = ErrorBody),
        (status = 404, description = "Not a file of my texts", body = ErrorBody),
        (status = 500, description = "Database or storage error", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn text_media(
    State(state): State<AppState>,
    device: DeviceUser,
    Path(id): Path<String>,
) -> ApiResult<Json<MediaLink>> {
    let user = &device.user;
    texting_on(&state, user.company_id).await?;
    let id: i64 = id.trim().parse().map_err(|_| ApiError::NotFound)?;
    let lines = texts::my_lines(&state.pool, user.company_id, user.id).await?;
    let number_ids: Vec<i32> = lines.keys().copied().collect();
    let (key, content_type, name) =
        texts::media_file(&state.pool, user.company_id, &number_ids, id)
            .await?
            .ok_or(ApiError::NotFound)?;
    let url = state
        .storage
        .presign_get_in_for(
            None,
            &key,
            &content_type,
            name.as_deref(),
            std::time::Duration::from_secs(MEDIA_LINK_SECS.unsigned_abs()),
        )
        .await?;
    Ok(Json(MediaLink {
        url,
        expires_at: Utc::now() + Duration::seconds(MEDIA_LINK_SECS),
    }))
}

/// Text from one of my lines
///
/// Sends an SMS from `ourNumberId` (a number I may call from) to `to`
/// (US and Canada only) and returns it (`queued`; Telnyx's delivery events
/// update it), with the thread marked read for me. Files from
/// `POST /v1/sms/attachments` go with it (pictures as MMS, other files as
/// links). 400 `invalid_phone`, a bad text (up to 1600 characters, a text or
/// a file), or a bad file (`invalid_attachment`, `too_many_images`,
/// `images_disabled`), 403 `texting_disabled` /
/// `not_my_line`, 409 `opted_out` (the person texted STOP), 429
/// `rate_limited` (ten texts a minute, shared with the CRM) with
/// `retryAfterSecs`, 502 when Telnyx failed.
#[utoipa::path(
    post,
    path = "/v1/phone/texts",
    operation_id = "send_phone_text",
    tag = "phone",
    request_body = SendText,
    responses(
        (status = 201, description = "Sent", body = TextMessage),
        (status = 400, description = "`invalid_phone`, `empty_message`, `text_too_long`, `invalid_attachment`, `too_many_images` or `images_disabled`", body = ErrorBody),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 403, description = "`texting_disabled` or `not_my_line`", body = ErrorBody),
        (status = 409, description = "`opted_out`: the person texted STOP", body = ErrorBody),
        (status = 429, description = "`rate_limited`", body = super::RateLimited),
        (status = 502, description = "Telnyx failed", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn send_text(
    State(state): State<AppState>,
    device: DeviceUser,
    JsonBody(body): JsonBody<SendText>,
) -> ApiResult<Response> {
    let user = &device.user;
    let pool = &state.pool;
    texting_on(&state, user.company_id).await?;
    let lines = texts::my_lines(pool, user.company_id, user.id).await?;
    let line = lines
        .get(&body.our_number_id)
        .cloned()
        .ok_or_else(|| denied("not_my_line"))?;
    let to = to_e164(&body.to)
        .filter(|to| is_nanp(to))
        .ok_or_else(|| bad_request("invalid_phone"))?;
    let other10 = last10(&to).ok_or_else(|| bad_request("invalid_phone"))?;
    let text = body.text.trim();
    // The CRM's own checks of a text: files under the company's prefix and of
    // a type that can go, at most ten, a text or a file, at most 1600 characters.
    let request = SendSmsRequest {
        phone_digits: other10.clone(),
        text: text.to_string(),
        attachments: body.attachments.clone(),
        template_id: None,
        customer_id: None,
        send_at: None,
        cancel_if_customer_messages: false,
    };
    if let Err(token) = check(user, &request) {
        return Err(bad_request(token));
    }
    let mut attachments = Vec::with_capacity(body.attachments.len());
    for file in &body.attachments {
        let filename: String = file.filename.chars().take(255).collect();
        let fallback = if file.content_type.starts_with("image/") {
            "image"
        } else {
            "file"
        };
        attachments.push(OutgoingSmsFile {
            url: state
                .storage
                .public_url(&file.s3_key)
                .map_err(|err| -> ApiError { err.into() })?,
            s3_key: file.s3_key.clone(),
            content_type: file.content_type.clone(),
            filename: if filename.is_empty() {
                fallback.to_string()
            } else {
                filename
            },
            width: file.width,
            height: file.height,
        });
    }
    if texting::is_opted_out(pool, user.company_id, &to).await? {
        return Err(ApiError::Conflict("opted_out".into()));
    }
    if let Some(wait) = crate::routes::sms::send::send_slot_wait(&state, user.id) {
        return Ok(rate_limited(wait));
    }
    let sms = OutgoingSms {
        provider: "telnyx".into(),
        company_id: user.company_id,
        user_id: user.id,
        agent_id: user.id.to_string(),
        phone_digits: other10.clone(),
        text: text.to_string(),
        attachments,
        is_automated: false,
        idempotency_key: None,
    };
    let ids = match texting::deliver(&state, &line, &sms).await {
        Ok(ids) => ids,
        Err(failure) if failure.error == "opted_out" => {
            return Err(ApiError::Conflict("opted_out".into()));
        }
        Err(failure) if failure.status == 400 => return Err(bad_request(&failure.error)),
        Err(_) => {
            return Err(ApiError::Upstream(
                "Telnyx did not take the text. Try again.".into(),
            ));
        }
    };
    texts::mark_read(pool, user.company_id, user.id, &other10).await?;
    let id = ids
        .first()
        .copied()
        .ok_or_else(|| ApiError::Internal("no text stored".into()))?;
    let message = texts::message(pool, user.company_id, id)
        .await?
        .ok_or_else(|| ApiError::Internal("the text was not stored".into()))?;
    Ok((StatusCode::CREATED, Json(message)).into_response())
}

/// Mark a text thread read
///
/// Marks the other number's texts read for me (the CRM's unread counts
/// follow). 404 for a thread that is not one of my lines'.
#[utoipa::path(
    post,
    path = "/v1/phone/texts/threads/{key}/read",
    operation_id = "mark_phone_text_thread_read",
    tag = "phone",
    params(("key" = String, Path, description = "The thread's key")),
    responses(
        (status = 204, description = "Marked read"),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 403, description = "`texting_disabled`", body = ErrorBody),
        (status = 404, description = "Not one of my threads", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn mark_text_thread_read(
    State(state): State<AppState>,
    device: DeviceUser,
    Path(key): Path<String>,
) -> ApiResult<StatusCode> {
    let user = &device.user;
    texting_on(&state, user.company_id).await?;
    let key = ThreadKey::parse(&key).ok_or(ApiError::NotFound)?;
    let lines = texts::my_lines(&state.pool, user.company_id, user.id).await?;
    if !lines.contains_key(&key.number_id) {
        return Err(ApiError::NotFound);
    }
    texts::mark_read(&state.pool, user.company_id, user.id, &key.other10).await?;
    Ok(StatusCode::NO_CONTENT)
}
