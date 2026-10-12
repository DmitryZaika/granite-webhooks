//! Shared implementation of sink and faucet models.
//!
//! `sink_type` / `faucet_type`: detail, create, edit (with unit count), delete and
//! installed photos. The `sinks` and `faucets` domains expose it with their
//! own paths. Remix: `admin.sinks.*` and `admin.faucets.*` routes.

use crate::error::ApiError;
use crate::routes::uploads::{self, schemas::UploadFolder};
use crate::state::AppState;
use serde::{Deserialize, Serialize};
use sqlx::{AssertSqlSafe, FromRow, MySqlConnection};
use utoipa::ToSchema;

/// Which model table a request is about.
#[derive(Debug, Clone, Copy)]
pub struct Kind {
    pub model_table: &'static str,
    pub unit_table: &'static str,
    pub unit_model_column: &'static str,
    pub photo_table: &'static str,
    pub photo_model_column: &'static str,
    pub folder: UploadFolder,
    /// Allowed `type` values.
    pub types: Types,
    /// Sinks have a length and width; faucets do not.
    pub sized: bool,
}

/// The `type` values a model can have.
#[derive(Debug, Clone, Copy)]
pub enum Types {
    Fixed(&'static [&'static str]),
    /// The names in this table of the company (case ignored).
    Company(&'static str),
}

pub const SINKS: Kind = Kind {
    model_table: "sink_type",
    unit_table: "sinks",
    unit_model_column: "sink_type_id",
    photo_table: "installed_sinks",
    photo_model_column: "sink_id",
    folder: UploadFolder::Sinks,
    types: Types::Company("sink_types"),
    sized: true,
};

pub const FAUCETS: Kind = Kind {
    model_table: "faucet_type",
    unit_table: "faucets",
    unit_model_column: "faucet_type_id",
    photo_table: "installed_faucets",
    photo_model_column: "faucet_id",
    folder: UploadFolder::Faucets,
    types: Types::Fixed(&["single handle", "double handle"]),
    sized: false,
};

/// A sink or faucet model with its stock, as the edit form shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow, ToSchema)]
pub struct FixtureModel {
    /// Model id.
    pub id: i32,
    /// Model name.
    pub name: String,
    /// Model type, e.g. `composite` or `single handle`.
    #[sqlx(rename = "type")]
    #[serde(rename = "type")]
    pub kind: String,
    /// Image URL; may be empty.
    pub url: String,
    /// Shown to customers in the showroom catalog.
    pub is_display: bool,
    /// Always orderable, never sold out.
    pub regular_stock: bool,
    /// Supplier id; null for none.
    pub supplier_id: Option<i32>,
    /// Length in inches (sinks); null for faucets.
    pub length: Option<i32>,
    /// Width in inches (sinks); null for faucets.
    pub width: Option<i32>,
    /// Retail price, decimal string; null if not set.
    pub retail_price: Option<String>,
    /// Cost, decimal string; null if not set.
    pub cost: Option<String>,
    /// Units in stock or installed on sales (not deleted).
    pub units: i64,
    /// Units installed on sales (included in `units`).
    pub installed_units: i64,
}

/// Body of the create and edit routes of sink and faucet models.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FixtureInput {
    /// Model name; required.
    pub name: String,
    /// Sinks: `stainless 18 gauge`, `stainless 16 gauge`, `composite`,
    /// `ceramic`, `farm house`. Faucets: `single handle`, `double handle`.
    #[serde(rename = "type")]
    #[schema(rename = "type")]
    pub kind: String,
    /// Image: an upload to the model's folder (`POST /v1/uploads`). Required
    /// when creating; omit to keep the image when editing.
    #[serde(default)]
    pub image_url: Option<String>,
    /// Shown in the catalogs.
    #[serde(default)]
    pub is_display: bool,
    /// Always orderable.
    #[serde(default)]
    pub regular_stock: bool,
    /// Supplier of the company; null for none.
    #[serde(default)]
    pub supplier_id: Option<i32>,
    /// Length in whole inches (sinks only; ignored for faucets).
    #[serde(default)]
    pub length: Option<i32>,
    /// Width in whole inches (sinks only; ignored for faucets).
    #[serde(default)]
    pub width: Option<i32>,
    /// Retail price in dollars.
    #[serde(default)]
    pub retail_price: Option<f64>,
    /// Cost in dollars.
    #[serde(default)]
    pub cost: Option<f64>,
    /// Units the model should have (free plus installed). Free units are
    /// added or removed to match; units installed on sales are never
    /// removed. Omit to leave the stock as is (creating: no units).
    #[serde(default)]
    pub units: Option<i64>,
}

/// Response of the create routes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
pub struct CreatedModel {
    /// Id of the new model.
    pub id: i32,
}

/// A photo of a model installed in a project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow, ToSchema)]
pub struct FixturePhoto {
    /// Photo id.
    pub id: i32,
    /// Photo URL.
    pub url: Option<String>,
}

/// Body of the add photos routes.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AddPhotos {
    /// URLs of uploads to the model's folder, 1 to 50.
    pub urls: Vec<String>,
}

const fn sql(text: String) -> AssertSqlSafe<String> {
    AssertSqlSafe(text)
}

pub async fn find(
    conn: &mut MySqlConnection,
    kind: Kind,
    company_id: i32,
    id: i32,
) -> Result<Option<FixtureModel>, sqlx::Error> {
    let size = if kind.sized {
        "m.length, m.width"
    } else {
        "CAST(NULL AS SIGNED) AS length, CAST(NULL AS SIGNED) AS width"
    };
    sqlx::query_as(sql(format!(
        "SELECT m.id, m.name, m.type, m.url, COALESCE(m.is_display, 0) AS is_display,
            COALESCE(m.regular_stock, 0) AS regular_stock, m.supplier_id, {size},
            CAST(m.retail_price AS CHAR) AS retail_price, CAST(m.cost AS CHAR) AS cost,
            (SELECT COUNT(*) FROM {units} u WHERE u.{col} = m.id AND u.is_deleted = 0) AS units,
            (SELECT COUNT(*) FROM {units} u WHERE u.{col} = m.id AND u.is_deleted = 0
               AND u.slab_id IS NOT NULL) AS installed_units
         FROM {table} m
         WHERE m.id = ? AND m.company_id = ? AND m.is_deleted = 0",
        table = kind.model_table,
        units = kind.unit_table,
        col = kind.unit_model_column,
    )))
    .bind(id)
    .bind(company_id)
    .fetch_optional(conn)
    .await
}

async fn validate(
    conn: &mut MySqlConnection,
    kind: Kind,
    company_id: i32,
    input: &FixtureInput,
) -> Result<(), ApiError> {
    if input.name.trim().is_empty() {
        return Err(ApiError::BadRequest("Name is required".into()));
    }
    let known = match kind.types {
        Types::Fixed(types) => types.contains(&input.kind.as_str()),
        Types::Company(table) => sqlx::query_scalar::<_, i32>(sql(format!(
            "SELECT 1 FROM {table} WHERE company_id = ? AND LOWER(name) = LOWER(?) LIMIT 1"
        )))
        .bind(company_id)
        .bind(input.kind.trim())
        .fetch_optional(&mut *conn)
        .await?
        .is_some(),
    };
    if !known {
        return Err(ApiError::BadRequest(format!(
            "invalid type: {}",
            input.kind
        )));
    }
    for (name, value) in [("retail_price", input.retail_price), ("cost", input.cost)] {
        if value.is_some_and(|v| !v.is_finite() || v < 0.0) {
            return Err(ApiError::BadRequest(format!(
                "{name} must be a non-negative number"
            )));
        }
    }
    if kind.sized
        && [input.length, input.width]
            .into_iter()
            .flatten()
            .any(|v| v < 0)
    {
        return Err(ApiError::BadRequest(
            "Size must be a non-negative number".into(),
        ));
    }
    if input
        .units
        .is_some_and(|units| !(0..=1000).contains(&units))
    {
        return Err(ApiError::BadRequest("units must be 0 to 1000".into()));
    }
    if let Some(supplier_id) = input.supplier_id {
        let found: Option<i32> =
            sqlx::query_scalar("SELECT id FROM suppliers WHERE id = ? AND company_id = ?")
                .bind(supplier_id)
                .bind(company_id)
                .fetch_optional(conn)
                .await?;
        if found.is_none() {
            return Err(ApiError::BadRequest(format!(
                "supplier {supplier_id} not found"
            )));
        }
    }
    Ok(())
}

fn image_url(input: &FixtureInput) -> Option<String> {
    input
        .image_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .map(str::to_string)
}

/// Adds or removes free units so the model has `target` live units.
async fn set_units(
    conn: &mut MySqlConnection,
    kind: Kind,
    id: i32,
    target: i64,
) -> Result<(), ApiError> {
    let (live, free): (i64, i64) = sqlx::query_as(sql(format!(
        "SELECT COUNT(*), CAST(COALESCE(SUM(slab_id IS NULL), 0) AS SIGNED) FROM {units}
         WHERE {col} = ? AND is_deleted = 0",
        units = kind.unit_table,
        col = kind.unit_model_column,
    )))
    .bind(id)
    .fetch_one(&mut *conn)
    .await?;
    if target > live {
        for _ in live..target {
            sqlx::query(sql(format!(
                "INSERT INTO {units} ({col}, is_deleted) VALUES (?, 0)",
                units = kind.unit_table,
                col = kind.unit_model_column,
            )))
            .bind(id)
            .execute(&mut *conn)
            .await?;
        }
    } else if target < live {
        let remove = live - target;
        if remove > free {
            return Err(ApiError::BadRequest(format!(
                "{} units are installed on sales; units cannot go below that",
                live - free
            )));
        }
        sqlx::query(sql(format!(
            "UPDATE {units} SET is_deleted = 1
             WHERE {col} = ? AND is_deleted = 0 AND slab_id IS NULL
             ORDER BY id DESC LIMIT ?",
            units = kind.unit_table,
            col = kind.unit_model_column,
        )))
        .bind(id)
        .bind(remove)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

pub async fn create(
    state: &AppState,
    kind: Kind,
    company_id: i32,
    input: &FixtureInput,
) -> Result<i32, ApiError> {
    let url =
        image_url(input).ok_or_else(|| ApiError::BadRequest("An image is required".into()))?;
    let mut tx = state.pool.begin().await?;
    validate(&mut tx, kind, company_id, input).await?;
    uploads::attach(&state.storage, &url, kind.folder).await?;
    let (columns, values) = if kind.sized {
        (", length, width, depth", ", ?, ?, 0")
    } else {
        ("", "")
    };
    let mut query = sqlx::query(sql(format!(
        "INSERT INTO {table} (name, type, url, company_id, is_display, is_deleted, regular_stock,
            supplier_id, retail_price, cost{columns})
         VALUES (?, ?, ?, ?, ?, 0, ?, ?, ?, ?{values})",
        table = kind.model_table,
    )))
    .bind(input.name.trim())
    .bind(&input.kind)
    .bind(&url)
    .bind(company_id)
    .bind(input.is_display)
    .bind(input.regular_stock)
    .bind(input.supplier_id)
    .bind(input.retail_price)
    .bind(input.cost);
    if kind.sized {
        query = query
            .bind(input.length.unwrap_or(0))
            .bind(input.width.unwrap_or(0));
    }
    let id = i32::try_from(query.execute(&mut *tx).await?.last_insert_id()).unwrap_or(i32::MAX);
    if let Some(units) = input.units {
        set_units(&mut tx, kind, id, units).await?;
    }
    tx.commit().await?;
    Ok(id)
}

pub async fn update(
    state: &AppState,
    kind: Kind,
    company_id: i32,
    id: i32,
    input: &FixtureInput,
) -> Result<(), ApiError> {
    let mut tx = state.pool.begin().await?;
    let current = find(&mut tx, kind, company_id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    validate(&mut tx, kind, company_id, input).await?;
    let new_url = image_url(input).filter(|url| *url != current.url);
    if let Some(url) = &new_url {
        uploads::attach(&state.storage, url, kind.folder).await?;
    }
    let size = if kind.sized {
        ", length = ?, width = ?"
    } else {
        ""
    };
    let mut query = sqlx::query(sql(format!(
        "UPDATE {table} SET name = ?, type = ?, url = COALESCE(?, url), is_display = ?,
            regular_stock = ?, supplier_id = ?, retail_price = ?, cost = ?{size}
         WHERE id = ? AND company_id = ?",
        table = kind.model_table,
    )))
    .bind(input.name.trim())
    .bind(&input.kind)
    .bind(&new_url)
    .bind(input.is_display)
    .bind(input.regular_stock)
    .bind(input.supplier_id)
    .bind(input.retail_price)
    .bind(input.cost);
    if kind.sized {
        query = query
            .bind(input.length.unwrap_or(0))
            .bind(input.width.unwrap_or(0));
    }
    query.bind(id).bind(company_id).execute(&mut *tx).await?;
    if let Some(units) = input.units {
        set_units(&mut tx, kind, id, units).await?;
    }
    tx.commit().await?;
    if new_url.is_some() && !current.url.is_empty() {
        uploads::remove(&state.storage, &current.url).await;
    }
    Ok(())
}

/// Hides the model and its free units; units installed on sales stay.
pub async fn delete(
    state: &AppState,
    kind: Kind,
    company_id: i32,
    id: i32,
) -> Result<(), ApiError> {
    let mut tx = state.pool.begin().await?;
    find(&mut tx, kind, company_id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    sqlx::query(sql(format!(
        "UPDATE {table} SET is_deleted = 1 WHERE id = ?",
        table = kind.model_table
    )))
    .bind(id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(sql(format!(
        "UPDATE {units} SET is_deleted = 1 WHERE {col} = ? AND slab_id IS NULL",
        units = kind.unit_table,
        col = kind.unit_model_column,
    )))
    .bind(id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn photos(
    state: &AppState,
    kind: Kind,
    company_id: i32,
    id: i32,
) -> Result<Vec<FixturePhoto>, ApiError> {
    let mut conn = state.pool.acquire().await?;
    find(&mut conn, kind, company_id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(sqlx::query_as(sql(format!(
        "SELECT id, url FROM {photos} WHERE {col} = ? ORDER BY id",
        photos = kind.photo_table,
        col = kind.photo_model_column,
    )))
    .bind(id)
    .fetch_all(&mut *conn)
    .await?)
}

pub async fn add_photos(
    state: &AppState,
    kind: Kind,
    company_id: i32,
    id: i32,
    urls: Vec<String>,
) -> Result<Vec<FixturePhoto>, ApiError> {
    if urls.is_empty() || urls.len() > 50 {
        return Err(ApiError::BadRequest("send 1 to 50 urls".into()));
    }
    let mut tx = state.pool.begin().await?;
    find(&mut tx, kind, company_id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    for url in &urls {
        uploads::attach(&state.storage, url, kind.folder).await?;
    }
    let mut added = Vec::with_capacity(urls.len());
    for url in urls {
        let result = sqlx::query(sql(format!(
            "INSERT INTO {photos} (url, {col}) VALUES (?, ?)",
            photos = kind.photo_table,
            col = kind.photo_model_column,
        )))
        .bind(&url)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        added.push(FixturePhoto {
            id: i32::try_from(result.last_insert_id()).unwrap_or(i32::MAX),
            url: Some(url),
        });
    }
    tx.commit().await?;
    Ok(added)
}

pub async fn delete_photo(
    state: &AppState,
    kind: Kind,
    company_id: i32,
    id: i32,
    photo_id: i32,
) -> Result<(), ApiError> {
    let mut tx = state.pool.begin().await?;
    find(&mut tx, kind, company_id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let url: Option<Option<String>> = sqlx::query_scalar(sql(format!(
        "SELECT url FROM {photos} WHERE id = ? AND {col} = ?",
        photos = kind.photo_table,
        col = kind.photo_model_column,
    )))
    .bind(photo_id)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let url = url.ok_or(ApiError::NotFound)?;
    sqlx::query(sql(format!(
        "DELETE FROM {photos} WHERE id = ?",
        photos = kind.photo_table
    )))
    .bind(photo_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    if let Some(url) = url {
        uploads::remove(&state.storage, &url).await;
    }
    Ok(())
}
