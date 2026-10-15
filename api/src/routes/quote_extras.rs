//! The company's quote extras catalog (Admin > Quotes > Extras): extras
//! such as tear out, kept once and offered by every price list unless a
//! list switches one off or prices it its own way.
//!
//! - `GET  /v1/quote-extras`        → the catalog, and how many extras lists still keep themselves
//! - `POST /v1/quote-extras`        → add one (optionally off in some lists)
//! - `PUT  /v1/quote-extras`        → save the catalog as edited (content, order, deletions)
//! - `POST /v1/quote-extras/moves`  → move the lists' own extras into the catalog, once
//!
//! Port of `crud/quoteExtras.server.ts` and the `admin.quotes.extras` route.

use crate::auth::AdminUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::quote_pricing::edit::{
    catalog_items_to_create, list_after_move, own_extras, with_extra_ids, without_catalog_id,
};
use crate::routes::created::CreatedId;
use crate::routes::quote_price_lists::{find, list_rows, save_revision};
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::collections::HashSet;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_extras, create_extra, save_extras))
        .routes(routes!(move_list_extras))
}

/// A catalog extra.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CatalogExtra {
    /// Extra id.
    pub id: i64,
    /// The extra (`OtherItem`: name, category, options or per-unit tiers).
    #[schema(value_type = Object)]
    pub item: Value,
}

/// `loadExtrasCatalog`: in order, skipping rows that are not extras.
pub async fn load_catalog(pool: &MySqlPool, company_id: i32) -> ApiResult<Vec<CatalogExtra>> {
    let rows: Vec<(i32, Option<String>)> = sqlx::query_as(
        "SELECT id, CAST(item_json AS CHAR) FROM quote_extras
          WHERE company_id = ? AND deleted_at IS NULL ORDER BY sequence, id",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(id, text)| {
            let mut item: Value = serde_json::from_str(text.as_deref()?).ok()?;
            if let Value::String(inner) = &item {
                item = serde_json::from_str(inner).ok()?;
            }
            item.get("name")?;
            Some(CatalogExtra {
                id: i64::from(id),
                item,
            })
        })
        .collect())
}

fn name_of(item: &Value) -> String {
    item.get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// `categoryOf` in the app: the trimmed category, `""` when none.
fn category_of(item: &Value) -> String {
    item.get("category")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string()
}

async fn insert(
    pool: &MySqlPool,
    company_id: i32,
    user_id: i32,
    item: &Value,
    sequence: usize,
) -> ApiResult<i64> {
    let item = without_catalog_id(&with_extra_ids(item));
    let id = sqlx::query(
        "INSERT INTO quote_extras (company_id, sequence, item_json, created_by) VALUES (?, ?, ?, ?)",
    )
    .bind(company_id)
    .bind(i64::try_from(sequence).unwrap_or(i64::MAX))
    .bind(item.to_string())
    .bind(user_id)
    .execute(pool)
    .await?
    .last_insert_id();
    Ok(i64::try_from(id).unwrap_or_default())
}

/// The catalog.
#[derive(Debug, Serialize, ToSchema)]
pub struct ExtrasCatalog {
    /// In order.
    pub catalog: Vec<CatalogExtra>,
    /// Extras the price lists still keep only for themselves (to move here).
    pub list_only: usize,
}

/// List the extras catalog
///
/// In order, with how many extras the price lists still keep themselves.
#[utoipa::path(
    get,
    path = "/v1/quote-extras",
    operation_id = "list_quote_extras",
    tag = "quotes",
    responses(
        (status = 200, description = "The catalog", body = ExtrasCatalog),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list_extras(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
) -> ApiResult<Json<ExtrasCatalog>> {
    let catalog = load_catalog(&state.pool, user.company_id).await?;
    let mut list_only = 0;
    for list in list_rows(&state.pool, user.company_id).await? {
        if let Some((_, Some(info))) = find(&state.pool, list.id, user.company_id).await? {
            list_only += own_extras(&info).len();
        }
    }
    Ok(Json(ExtrasCatalog { catalog, list_only }))
}

/// A new extra.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewExtra {
    /// The extra (`OtherItem`).
    #[schema(value_type = Object)]
    pub item: Value,
    /// Price lists that do not offer it.
    #[serde(default)]
    pub off_list_ids: Vec<i32>,
}

/// Add an extra
///
/// Every price list offers it, except `off_list_ids`, which each get a
/// revision that switches it off.
#[utoipa::path(
    post,
    path = "/v1/quote-extras",
    operation_id = "create_quote_extra",
    tag = "quotes",
    request_body = NewExtra,
    responses(
        (status = 201, description = "Added", body = CreatedId),
        (status = 400, description = "No name", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 409, description = "The name is taken", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create_extra(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<NewExtra>,
) -> ApiResult<(StatusCode, Json<CreatedId>)> {
    let name = name_of(&input.item);
    if name.is_empty() || !input.item.is_object() {
        return Err(ApiError::BadRequest("Give the extra a name.".into()));
    }
    let catalog = load_catalog(&state.pool, user.company_id).await?;
    if catalog
        .iter()
        .any(|e| name_of(&e.item).to_lowercase() == name.to_lowercase())
    {
        return Err(ApiError::Conflict(format!("\"{name}\" already exists.")));
    }
    // New extras go on top of their category (a new category goes first):
    // the ones from there down move one place.
    let category = category_of(&input.item);
    let position = catalog
        .iter()
        .position(|e| category_of(&e.item) == category)
        .unwrap_or(0);
    for (index, extra) in catalog.iter().enumerate().skip(position) {
        sqlx::query("UPDATE quote_extras SET sequence = ? WHERE id = ? AND company_id = ?")
            .bind(i64::try_from(index + 1).unwrap_or(i64::MAX))
            .bind(extra.id)
            .bind(user.company_id)
            .execute(&state.pool)
            .await?;
    }
    let id = insert(&state.pool, user.company_id, user.id, &input.item, position).await?;
    for list_id in input.off_list_ids {
        let Some((_, Some(mut info))) = find(&state.pool, list_id, user.company_id).await? else {
            continue;
        };
        let mut links = info
            .get("extraLinks")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        links.insert(id.to_string(), json!({ "off": true }));
        info["extraLinks"] = Value::Object(links);
        save_revision(
            &state.pool,
            list_id,
            user.company_id,
            &info,
            Some(user.id),
            Some(&format!("Extra \"{name}\" not offered")),
        )
        .await?;
    }
    Ok((StatusCode::CREATED, Json(CreatedId { id })))
}

/// The catalog as edited.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct EditedCatalog {
    /// Every extra kept, in order; extras left out are deleted.
    pub extras: Vec<CatalogExtra>,
}

/// Save the extras catalog
///
/// Each extra's content and order; extras left out are deleted (price lists
/// stop offering them). Names must be unique.
#[utoipa::path(
    put,
    path = "/v1/quote-extras",
    operation_id = "save_quote_extras",
    tag = "quotes",
    request_body = EditedCatalog,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "A name is listed twice", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn save_extras(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<EditedCatalog>,
) -> ApiResult<StatusCode> {
    let names: Vec<String> = input
        .extras
        .iter()
        .map(|e| name_of(&e.item).to_lowercase())
        .collect();
    if let Some(dupe) = names
        .iter()
        .enumerate()
        .find(|(i, n)| names[..*i].contains(n))
        .map(|(_, n)| n)
    {
        return Err(ApiError::BadRequest(format!(
            "\"{dupe}\" is listed more than once."
        )));
    }
    let current = load_catalog(&state.pool, user.company_id).await?;
    let known: HashSet<i64> = current.iter().map(|c| c.id).collect();
    let keep: HashSet<i64> = input.extras.iter().map(|e| e.id).collect();
    let mut tx = state.pool.begin().await?;
    for (sequence, extra) in input.extras.iter().enumerate() {
        if !known.contains(&extra.id) {
            continue;
        }
        let item = without_catalog_id(&with_extra_ids(&extra.item));
        sqlx::query(
            "UPDATE quote_extras SET item_json = ?, sequence = ?
              WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
        )
        .bind(item.to_string())
        .bind(i64::try_from(sequence).unwrap_or(i64::MAX))
        .bind(extra.id)
        .bind(user.company_id)
        .execute(&mut *tx)
        .await?;
    }
    for extra in current.iter().filter(|c| !keep.contains(&c.id)) {
        sqlx::query("UPDATE quote_extras SET deleted_at = NOW() WHERE id = ? AND company_id = ?")
            .bind(extra.id)
            .bind(user.company_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// How many catalog extras the move made.
#[derive(Debug, Serialize, ToSchema)]
pub struct MovedExtras {
    /// New catalog extras.
    pub created: usize,
}

/// Move the lists' own extras into the catalog
///
/// Once: extras with the same name become one catalog extra (the first
/// list's version wins), and each list links to them, keeping its own
/// prices where they differ.
#[utoipa::path(
    post,
    path = "/v1/quote-extras/moves",
    operation_id = "move_quote_list_extras",
    tag = "quotes",
    responses(
        (status = 200, description = "Moved", body = MovedExtras),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn move_list_extras(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
) -> ApiResult<Json<MovedExtras>> {
    let mut lists: Vec<(i32, Value)> = Vec::new();
    for row in list_rows(&state.pool, user.company_id).await? {
        if let Some((_, Some(info))) = find(&state.pool, row.id, user.company_id).await? {
            lists.push((row.id, info));
        }
    }
    let before: Vec<(i64, Value)> = load_catalog(&state.pool, user.company_id)
        .await?
        .into_iter()
        .map(|e| (e.id, e.item))
        .collect();
    let infos: Vec<Value> = lists.iter().map(|(_, info)| info.clone()).collect();
    let mut created = HashSet::new();
    for (i, item) in catalog_items_to_create(&infos, &before).iter().enumerate() {
        created.insert(
            insert(
                &state.pool,
                user.company_id,
                user.id,
                item,
                before.len() + i,
            )
            .await?,
        );
    }
    let catalog: Vec<(i64, Value)> = load_catalog(&state.pool, user.company_id)
        .await?
        .into_iter()
        .map(|e| (e.id, e.item))
        .collect();
    for (id, info) in &lists {
        if own_extras(info).is_empty() && created.is_empty() {
            continue;
        }
        save_revision(
            &state.pool,
            *id,
            user.company_id,
            &list_after_move(info, &catalog, &created),
            Some(user.id),
            Some("Extras moved to the catalog"),
        )
        .await?;
    }
    Ok(Json(MovedExtras {
        created: created.len(),
    }))
}
