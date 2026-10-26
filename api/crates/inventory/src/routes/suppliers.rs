//! Adding, editing and deleting suppliers and their price files (admins).
//!
//! - `POST   /v1/suppliers`                → add a supplier
//! - `GET    /v1/suppliers/{id}`           → one supplier's details
//! - `PUT    /v1/suppliers/{id}`           → edit it
//! - `DELETE /v1/suppliers/{id}`           → delete it
//! - `GET    /v1/suppliers/{id}/files`     → its price files and their processing state
//! - `POST   /v1/suppliers/{id}/files`     → add a price file (uploaded first to `supplier-files`)
//! - `DELETE /v1/supplier-files/{id}`      → delete a price file
//!
//! A supplier with a phone number is kept in the company's `CloudTalk`
//! address book (contact `industry` `gm-supplier-<company>-<supplier>`), so
//! its calls and texts show its name. Replaces the Remix
//! `admin.suppliers.{add,edit,delete}` routes and `cloudtalkSupplierSync.server.ts`.

use crate::auth::AdminUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::openapi::{OpenApiRouter, routes};
use crate::routes::calls::to_e164;
use crate::routes::created::CreatedId as Created;
use crate::routes::uploads::{self, schemas::UploadFolder};
use crate::serde_helpers::js_date;
use crate::state::AppState;
use crate::supplier_file_processor::{ALLOWED_EXTENSIONS, file_extension};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use common::telephony::cloudtalk;
use lambda_http::tracing;
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(create_supplier))
        .routes(routes!(get_supplier, update_supplier, delete_supplier))
        .routes(routes!(list_files, add_file))
        .routes(routes!(delete_file))
}

/// A supplier's details.
#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct Supplier {
    pub id: i32,
    /// Display name.
    pub supplier_name: String,
    /// Website URL, or null.
    pub website: Option<String>,
    /// Name of the contact person, or null.
    pub manager: Option<String>,
    /// Phone number as typed, or null.
    pub phone: Option<String>,
    /// Email address, or null.
    pub email: Option<String>,
    /// Free-text notes, or null.
    pub notes: Option<String>,
}

/// Body of `POST /v1/suppliers` and `PUT /v1/suppliers/{id}`. Blank
/// optional fields are saved as null.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SupplierInput {
    /// Display name (required, trimmed).
    pub supplier_name: String,
    /// Website URL (`http(s)://...`).
    #[serde(default)]
    pub website: Option<String>,
    /// Contact person.
    #[serde(default)]
    pub manager: Option<String>,
    /// Phone number, at least 10 characters.
    #[serde(default)]
    pub phone: Option<String>,
    /// Email address.
    #[serde(default)]
    pub email: Option<String>,
    /// Notes.
    #[serde(default)]
    pub notes: Option<String>,
}

fn blank_to_none(value: Option<&String>) -> Option<String> {
    value
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

struct CleanSupplier {
    name: String,
    website: Option<String>,
    manager: Option<String>,
    phone: Option<String>,
    email: Option<String>,
    notes: Option<String>,
}

fn validate(input: &SupplierInput) -> Result<CleanSupplier, ApiError> {
    let name = input.supplier_name.trim().to_string();
    if name.is_empty() {
        return Err(ApiError::BadRequest("supplier_name is required".into()));
    }
    let website = blank_to_none(input.website.as_ref());
    if let Some(website) = &website
        && url::Url::parse(website).is_err()
    {
        return Err(ApiError::BadRequest("website must be a URL".into()));
    }
    let phone = blank_to_none(input.phone.as_ref());
    if phone.as_ref().is_some_and(|p| p.chars().count() < 10) {
        return Err(ApiError::BadRequest(
            "phone must have at least 10 characters".into(),
        ));
    }
    let email = blank_to_none(input.email.as_ref());
    if let Some(email) = &email
        && !is_email(email)
    {
        return Err(ApiError::BadRequest("email is not valid".into()));
    }
    Ok(CleanSupplier {
        name,
        website,
        manager: blank_to_none(input.manager.as_ref()),
        phone,
        email,
        notes: blank_to_none(input.notes.as_ref()),
    })
}

fn is_email(value: &str) -> bool {
    let Some((local, domain)) = value.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !value.chars().any(char::is_whitespace)
}

async fn owned_supplier(state: &AppState, id: i32, company_id: i32) -> ApiResult<Supplier> {
    sqlx::query_as(
        "SELECT id, supplier_name, website, manager, phone, email, notes
           FROM suppliers WHERE id = ? AND company_id = ?",
    )
    .bind(id)
    .bind(company_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or(ApiError::NotFound)
}

/// Add a supplier
///
/// Creates a supplier of the company and adds its phone number to the
/// company's `CloudTalk` contacts. Returns its id.
#[crate::openapi::path(
    post,
    path = "/v1/suppliers",
    operation_id = "create_supplier",
    tag = "catalog",
    request_body = SupplierInput,
    responses(
        (status = 201, description = "Created", body = Created),
        (status = 400, description = "Missing name, or a malformed website, phone or email", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create_supplier(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<SupplierInput>,
) -> ApiResult<(StatusCode, Json<Created>)> {
    let s = validate(&input)?;
    let id = sqlx::query(
        "INSERT INTO suppliers (website, supplier_name, manager, phone, email, notes, company_id)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&s.website)
    .bind(&s.name)
    .bind(&s.manager)
    .bind(&s.phone)
    .bind(&s.email)
    .bind(&s.notes)
    .bind(user.company_id)
    .execute(&state.pool)
    .await?
    .last_insert_id();
    let id =
        i32::try_from(id).map_err(|_| ApiError::Internal("supplier id out of range".into()))?;
    sync_to_cloudtalk(&state, user.company_id, id).await;
    Ok((StatusCode::CREATED, Json(Created { id: i64::from(id) })))
}

/// Get a supplier
///
/// One supplier of the company with its contact details.
#[crate::openapi::path(
    get,
    path = "/v1/suppliers/{id}",
    operation_id = "get_supplier",
    tag = "catalog",
    params(("id" = i32, Path, description = "Supplier id")),
    responses(
        (status = 200, description = "The supplier", body = Supplier),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such supplier in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get_supplier(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<Supplier>> {
    Ok(Json(owned_supplier(&state, id, user.company_id).await?))
}

/// Edit a supplier
///
/// Replaces the supplier's details and updates its `CloudTalk` contact
/// (removed when the phone number is gone).
#[crate::openapi::path(
    put,
    path = "/v1/suppliers/{id}",
    operation_id = "update_supplier",
    tag = "catalog",
    params(("id" = i32, Path, description = "Supplier id")),
    request_body = SupplierInput,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Missing name, or a malformed website, phone or email", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such supplier in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update_supplier(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<SupplierInput>,
) -> ApiResult<StatusCode> {
    owned_supplier(&state, id, user.company_id).await?;
    let s = validate(&input)?;
    sqlx::query(
        "UPDATE suppliers SET website = ?, supplier_name = ?, manager = ?, phone = ?, email = ?, notes = ?
          WHERE id = ? AND company_id = ?",
    )
    .bind(&s.website)
    .bind(&s.name)
    .bind(&s.manager)
    .bind(&s.phone)
    .bind(&s.email)
    .bind(&s.notes)
    .bind(id)
    .bind(user.company_id)
    .execute(&state.pool)
    .await?;
    sync_to_cloudtalk(&state, user.company_id, id).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Delete a supplier
///
/// Deletes the supplier (and its price files) and removes it from the
/// company's `CloudTalk` contacts. Stones, sinks and faucets that named it
/// keep their supplier id (as before); 409 if the database refuses.
#[crate::openapi::path(
    delete,
    path = "/v1/suppliers/{id}",
    operation_id = "delete_supplier",
    tag = "catalog",
    params(("id" = i32, Path, description = "Supplier id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such supplier in the company", body = ErrorBody),
        (status = 409, description = "The database refused the delete", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_supplier(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    owned_supplier(&state, id, user.company_id).await?;
    let file_urls: Vec<String> =
        sqlx::query_scalar("SELECT url FROM supplier_files WHERE supplier_id = ?")
            .bind(id)
            .fetch_all(&state.pool)
            .await?;
    let mut tx = state.pool.begin().await?;
    sqlx::query("DELETE FROM supplier_files WHERE supplier_id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    let deleted = sqlx::query("DELETE FROM suppliers WHERE id = ? AND company_id = ?")
        .bind(id)
        .bind(user.company_id)
        .execute(&mut *tx)
        .await;
    match deleted {
        Ok(_) => tx.commit().await?,
        Err(sqlx::Error::Database(error)) if error.is_foreign_key_violation() => {
            return Err(ApiError::Conflict(
                "Failed to delete supplier: inventory still uses it".into(),
            ));
        }
        Err(error) => return Err(error.into()),
    }
    for url in file_urls {
        uploads::remove(&state.storage, &url, user.company_id).await;
    }
    if let Err(error) = remove_from_cloudtalk(&state, user.company_id, id).await {
        tracing::warn!(
            ?error,
            supplier_id = id,
            "cloudtalk supplier removal failed"
        );
    }
    Ok(StatusCode::NO_CONTENT)
}

/// A supplier price file.
#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct SupplierFile {
    pub id: i64,
    /// Name the admin gave it.
    pub name: String,
    /// The file in the bucket.
    pub url: String,
    /// `pending`, `processing`, `ready` or `failed`.
    pub status: String,
    /// Why it failed (`download_failed`, `empty_result`, `too_large`,
    /// `too_many_pages`, `unsupported_format`, `extract_error`, `stalled`),
    /// else null.
    pub error_token: Option<String>,
    /// When it was added (ISO 8601).
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = String)]
    pub create_date: Option<DateTime<Utc>>,
}

/// List a supplier's price files
///
/// The supplier's price files, newest first, with their processing state.
/// Files in `pending`/`processing` are picked up by the processor within a
/// minute; poll while any is.
#[crate::openapi::path(
    get,
    path = "/v1/suppliers/{id}/files",
    operation_id = "list_supplier_files",
    tag = "catalog",
    params(("id" = i32, Path, description = "Supplier id")),
    responses(
        (status = 200, description = "The files", body = [SupplierFile]),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such supplier in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list_files(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<Vec<SupplierFile>>> {
    owned_supplier(&state, id, user.company_id).await?;
    let files = sqlx::query_as(
        "SELECT CAST(id AS SIGNED) AS id, name, url, CAST(status AS CHAR) AS status, error_token, create_date
           FROM supplier_files WHERE supplier_id = ? ORDER BY id DESC",
    )
    .bind(id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(files))
}

/// Body of `POST /v1/suppliers/{id}/files`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewSupplierFile {
    /// Name to show for the file (required).
    pub name: String,
    /// URL from `POST /v1/uploads` with folder `supplier-files`, after the
    /// file was uploaded. PDF, JPEG, PNG, WebP, CSV, TSV or TXT.
    pub url: String,
}

/// Add a supplier price file
///
/// Records an uploaded price file as `pending`; the processor turns it into
/// searchable prices within a minute (`GET /v1/suppliers/{id}/files` shows
/// its state).
#[crate::openapi::path(
    post,
    path = "/v1/suppliers/{id}/files",
    operation_id = "add_supplier_file",
    tag = "catalog",
    params(("id" = i32, Path, description = "Supplier id")),
    request_body = NewSupplierFile,
    responses(
        (status = 201, description = "Added", body = Created),
        (status = 400, description = "Missing name, or not an uploaded file of an accepted type", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such supplier in the company", body = ErrorBody),
        (status = 500, description = "Database or storage error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn add_file(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<NewSupplierFile>,
) -> ApiResult<(StatusCode, Json<Created>)> {
    owned_supplier(&state, id, user.company_id).await?;
    let name = input.name.trim();
    if name.is_empty() {
        return Err(ApiError::BadRequest("name is required".into()));
    }
    let ext = file_extension("", &input.url);
    if !ALLOWED_EXTENSIONS.contains(&ext.as_str()) {
        return Err(ApiError::BadRequest(format!(
            "unsupported file type; use one of {}",
            ALLOWED_EXTENSIONS.join(", ")
        )));
    }
    uploads::attach(
        &state.storage,
        &input.url,
        UploadFolder::SupplierFiles,
        user.company_id,
    )
    .await?;
    let file_id = sqlx::query(
        "INSERT INTO supplier_files (name, supplier_id, url, status) VALUES (?, ?, ?, 'pending')",
    )
    .bind(name)
    .bind(id)
    .bind(&input.url)
    .execute(&state.pool)
    .await?
    .last_insert_id();
    Ok((
        StatusCode::CREATED,
        Json(Created {
            id: i64::try_from(file_id).unwrap_or(i64::MAX),
        }),
    ))
}

/// Delete a supplier price file
///
/// Deletes a price file of one of the company's suppliers, its prices and
/// the stored file.
#[crate::openapi::path(
    delete,
    path = "/v1/supplier-files/{id}",
    operation_id = "delete_supplier_file",
    tag = "catalog",
    params(("id" = i64, Path, description = "Supplier file id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such file in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_file(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<u64>,
) -> ApiResult<StatusCode> {
    let url: Option<String> = sqlx::query_scalar(
        "SELECT sf.url FROM supplier_files sf JOIN suppliers s ON s.id = sf.supplier_id
          WHERE sf.id = ? AND s.company_id = ?",
    )
    .bind(id)
    .bind(user.company_id)
    .fetch_optional(&state.pool)
    .await?;
    let url = url.ok_or(ApiError::NotFound)?;
    sqlx::query(
        "DELETE sf FROM supplier_files sf JOIN suppliers s ON s.id = sf.supplier_id
          WHERE sf.id = ? AND s.company_id = ?",
    )
    .bind(id)
    .bind(user.company_id)
    .execute(&state.pool)
    .await?;
    uploads::remove(&state.storage, &url, user.company_id).await;
    Ok(StatusCode::NO_CONTENT)
}

// ---- CloudTalk address book

const NAME_MAX: usize = 150;
const COMPANY_MAX: usize = 100;
const TITLE_MAX: usize = 255;
const WEBSITE_MAX: usize = 100;
const EMAIL_MAX: usize = 255;

/// The contact's key in `CloudTalk`.
pub fn industry_key(company_id: i32, supplier_id: i32) -> String {
    format!("gm-supplier-{company_id}-{supplier_id}")
}

fn clip(value: &str, max: usize) -> String {
    let mut units = 0;
    value
        .chars()
        .take_while(|c| {
            units += c.len_utf16();
            units <= max
        })
        .collect()
}

/// The contact `CloudTalk` stores for a supplier with a phone number.
pub fn contact_payload(company_id: i32, supplier: &Supplier, e164: &str) -> Value {
    let name = clip(supplier.supplier_name.trim(), NAME_MAX);
    let mut payload = json!({
        "name": name,
        "industry": industry_key(company_id, supplier.id),
        "company": clip(&name, COMPANY_MAX),
        "ContactNumber": [{ "public_number": e164 }],
    });
    if let Some(manager) = supplier
        .manager
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
    {
        payload["title"] = json!(clip(manager, TITLE_MAX));
    }
    if let Some(website) = supplier
        .website
        .as_deref()
        .map(str::trim)
        .filter(|w| !w.is_empty() && w.encode_utf16().count() <= WEBSITE_MAX)
    {
        payload["website"] = json!(website);
    }
    if let Some(email) = supplier.email.as_deref().map(str::trim).filter(|e| {
        !e.is_empty()
            && e.encode_utf16().count() <= EMAIL_MAX
            && e.split_once('@')
                .is_some_and(|(l, d)| !l.is_empty() && !d.is_empty() && !d.contains('@'))
            && !e.chars().any(char::is_whitespace)
    }) {
        payload["ContactEmail"] = json!([{ "email": email }]);
    }
    payload
}

async fn company_has_cloudtalk(state: &AppState, company_id: i32) -> bool {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM company WHERE id = ?
            AND COALESCE(cloudtalk_access_key, '') <> '' AND COALESCE(cloudtalk_access_secret, '') <> ''",
    )
    .bind(company_id)
    .fetch_one(&state.pool)
    .await
    .is_ok_and(|count| count > 0)
}

fn cloudtalk_api(state: &AppState, company_id: i32) -> cloudtalk::Api<'_> {
    cloudtalk::Api {
        pool: &state.pool,
        client: &state.telephony_http,
        base_url: &state.cloudtalk_base,
        company_id,
    }
}

fn read_id(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64().filter(|id| *id > 0),
        Value::String(s) => s.parse::<i64>().ok().filter(|id| *id > 0),
        _ => None,
    }
}

/// The supplier's contact id in `CloudTalk`, found by its industry key.
async fn find_contact(
    state: &AppState,
    company_id: i32,
    supplier_id: i32,
) -> Result<Option<i64>, cloudtalk::ApiError> {
    let api = cloudtalk_api(state, company_id);
    let industry = industry_key(company_id, supplier_id);
    let list = cloudtalk::request(
        api,
        Method::GET,
        &format!("contacts/index.json?industry={industry}&limit=10"),
        None,
        None,
    )
    .await?;
    let items = list["responseData"]["data"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for item in items {
        let Some(id) = read_id(&item["Contact"]["id"]).or_else(|| read_id(&item["id"])) else {
            continue;
        };
        match item["Contact"]["industry"].as_str() {
            Some(listed) if listed == industry => return Ok(Some(id)),
            Some(_) => {}
            None => {
                let shown = cloudtalk::request(
                    api,
                    Method::GET,
                    &format!("contacts/show/{id}.json"),
                    None,
                    None,
                )
                .await?;
                if shown["responseData"]["Contact"]["industry"].as_str() == Some(industry.as_str())
                {
                    return Ok(Some(id));
                }
            }
        }
    }
    Ok(None)
}

async fn delete_contact(
    state: &AppState,
    company_id: i32,
    contact_id: i64,
) -> Result<(), cloudtalk::ApiError> {
    match cloudtalk::request(
        cloudtalk_api(state, company_id),
        Method::DELETE,
        &format!("contacts/delete/{contact_id}.json"),
        None,
        None,
    )
    .await
    {
        Err(error) if error.status != 404 => Err(error),
        _ => Ok(()),
    }
}

/// Adds, updates or removes the supplier's `CloudTalk` contact; failures are
/// logged (the supplier is saved either way).
pub async fn sync_to_cloudtalk(state: &AppState, company_id: i32, supplier_id: i32) {
    if let Err(error) = try_sync(state, company_id, supplier_id).await {
        tracing::warn!(?error, supplier_id, "cloudtalk supplier sync failed");
    }
}

async fn try_sync(
    state: &AppState,
    company_id: i32,
    supplier_id: i32,
) -> Result<(), cloudtalk::ApiError> {
    if !company_has_cloudtalk(state, company_id).await {
        return Ok(());
    }
    let Ok(supplier) = owned_supplier(state, supplier_id, company_id).await else {
        return Ok(());
    };
    if supplier.supplier_name.trim().is_empty() {
        return Ok(());
    }
    let existing = find_contact(state, company_id, supplier_id).await?;
    let Some(e164) = supplier.phone.as_deref().and_then(to_e164) else {
        if let Some(id) = existing {
            delete_contact(state, company_id, id).await?;
        }
        return Ok(());
    };
    let payload = contact_payload(company_id, &supplier, &e164);
    let api = cloudtalk_api(state, company_id);
    if let Some(id) = existing {
        cloudtalk::request(
            api,
            Method::POST,
            &format!("contacts/edit/{id}.json"),
            Some(&payload),
            None,
        )
        .await?;
    } else {
        let created =
            cloudtalk::request(api, Method::PUT, "contacts/add.json", Some(&payload), None).await?;
        let id = read_id(&created["responseData"]["data"]["Contact"]["id"])
            .or_else(|| read_id(&created["responseData"]["data"]["id"]))
            .or_else(|| read_id(&created["data"]["id"]));
        if id.is_none() {
            return Err(cloudtalk::ApiError {
                status: 502,
                message: Some("CloudTalk add contact did not return an id".into()),
            });
        }
    }
    Ok(())
}

async fn remove_from_cloudtalk(
    state: &AppState,
    company_id: i32,
    supplier_id: i32,
) -> Result<(), cloudtalk::ApiError> {
    if !company_has_cloudtalk(state, company_id).await {
        return Ok(());
    }
    if let Some(id) = find_contact(state, company_id, supplier_id).await? {
        delete_contact(state, company_id, id).await?;
    }
    Ok(())
}
