use super::schemas::{Color, StoneType, StoneUsingType, Supplier};
use sqlx::{AssertSqlSafe, MySqlPool};

pub async fn list_suppliers(
    pool: &MySqlPool,
    company_id: i32,
) -> Result<Vec<Supplier>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id, supplier_name AS name FROM suppliers WHERE company_id = ?
         ORDER BY supplier_name ASC, id ASC",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await
}

pub async fn list_colors(pool: &MySqlPool) -> Result<Vec<Color>, sqlx::Error> {
    sqlx::query_as("SELECT id, name, hex_code FROM colors ORDER BY name ASC, id ASC")
        .fetch_all(pool)
        .await
}

/// A company's catalog types: their table and the items that store a
/// type's name in their `type` column.
#[derive(Clone, Copy)]
pub struct TypeTable {
    pub types: &'static str,
    pub items: &'static str,
    /// Condition of an item that is not deleted.
    pub live: &'static str,
    /// What the messages call an item, plural ("stones").
    pub plural: &'static str,
    /// What the messages call a type ("stone").
    pub label: &'static str,
}

pub const STONE_TYPES: TypeTable = TypeTable {
    types: "stone_types",
    items: "stones",
    live: "deleted_at IS NULL",
    plural: "stones",
    label: "stone",
};

pub const SINK_TYPES: TypeTable = TypeTable {
    types: "sink_types",
    items: "sink_type",
    live: "is_deleted = 0",
    plural: "sinks",
    label: "sink",
};

const fn sql(text: String) -> AssertSqlSafe<String> {
    AssertSqlSafe(text)
}

pub async fn list_types(
    pool: &MySqlPool,
    table: TypeTable,
    company_id: i32,
) -> Result<Vec<StoneType>, sqlx::Error> {
    sqlx::query_as(sql(format!(
        "SELECT CAST(id AS SIGNED) AS id, name FROM {} WHERE company_id = ?
         ORDER BY name ASC, id ASC",
        table.types
    )))
    .bind(company_id)
    .fetch_all(pool)
    .await
}

/// A type of the company with this name, ignoring case, other than `except`.
pub async fn find_type_by_name(
    pool: &MySqlPool,
    table: TypeTable,
    company_id: i32,
    name: &str,
    except: Option<i64>,
) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar(sql(format!(
        "SELECT CAST(id AS SIGNED) FROM {}
         WHERE company_id = ? AND LOWER(name) = LOWER(?) AND id <> ? LIMIT 1",
        table.types
    )))
    .bind(company_id)
    .bind(name)
    .bind(except.unwrap_or(-1))
    .fetch_optional(pool)
    .await
}

pub async fn find_type(
    pool: &MySqlPool,
    table: TypeTable,
    company_id: i32,
    id: i64,
) -> Result<Option<StoneType>, sqlx::Error> {
    sqlx::query_as(sql(format!(
        "SELECT CAST(id AS SIGNED) AS id, name FROM {} WHERE id = ? AND company_id = ?",
        table.types
    )))
    .bind(id)
    .bind(company_id)
    .fetch_optional(pool)
    .await
}

pub async fn create_type(
    pool: &MySqlPool,
    table: TypeTable,
    company_id: i32,
    name: &str,
) -> Result<i64, sqlx::Error> {
    let result = sqlx::query(sql(format!(
        "INSERT INTO {} (name, company_id) VALUES (?, ?)",
        table.types
    )))
    .bind(name)
    .bind(company_id)
    .execute(pool)
    .await?;
    Ok(i64::try_from(result.last_insert_id()).unwrap_or(i64::MAX))
}

/// Renames the type and every item of the company that has it.
pub async fn rename_type(
    pool: &MySqlPool,
    table: TypeTable,
    company_id: i32,
    existing: &StoneType,
    name: &str,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query(sql(format!(
        "UPDATE {} SET name = ? WHERE id = ? AND company_id = ?",
        table.types
    )))
    .bind(name)
    .bind(existing.id)
    .bind(company_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(sql(format!(
        "UPDATE {} SET type = ? WHERE company_id = ? AND LOWER(type) = LOWER(?)",
        table.items
    )))
    .bind(name)
    .bind(company_id)
    .bind(&existing.name)
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

pub async fn items_using_type(
    pool: &MySqlPool,
    table: TypeTable,
    company_id: i32,
    name: &str,
) -> Result<Vec<StoneUsingType>, sqlx::Error> {
    sqlx::query_as(sql(format!(
        "SELECT id, name FROM {} WHERE company_id = ? AND {} AND LOWER(type) = LOWER(?)
         ORDER BY name ASC, id ASC",
        table.items, table.live
    )))
    .bind(company_id)
    .bind(name)
    .fetch_all(pool)
    .await
}

pub async fn delete_type(
    pool: &MySqlPool,
    table: TypeTable,
    company_id: i32,
    id: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(sql(format!(
        "DELETE FROM {} WHERE id = ? AND company_id = ?",
        table.types
    )))
    .bind(id)
    .bind(company_id)
    .execute(pool)
    .await?;
    Ok(())
}
