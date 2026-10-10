use super::schemas::{Color, StoneType, StoneUsingType, Supplier};
use sqlx::MySqlPool;

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

pub async fn list_stone_types(
    pool: &MySqlPool,
    company_id: i32,
) -> Result<Vec<StoneType>, sqlx::Error> {
    sqlx::query_as(
        "SELECT CAST(id AS SIGNED) AS id, name FROM stone_types WHERE company_id = ?
         ORDER BY name ASC, id ASC",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await
}

/// A type of the company with this name, ignoring case, other than `except`.
pub async fn find_type_by_name(
    pool: &MySqlPool,
    company_id: i32,
    name: &str,
    except: Option<i64>,
) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT CAST(id AS SIGNED) FROM stone_types
         WHERE company_id = ? AND LOWER(name) = LOWER(?) AND id <> ? LIMIT 1",
    )
    .bind(company_id)
    .bind(name)
    .bind(except.unwrap_or(-1))
    .fetch_optional(pool)
    .await
}

pub async fn find_type(
    pool: &MySqlPool,
    company_id: i32,
    id: i64,
) -> Result<Option<StoneType>, sqlx::Error> {
    sqlx::query_as(
        "SELECT CAST(id AS SIGNED) AS id, name FROM stone_types WHERE id = ? AND company_id = ?",
    )
    .bind(id)
    .bind(company_id)
    .fetch_optional(pool)
    .await
}

pub async fn create_type(
    pool: &MySqlPool,
    company_id: i32,
    name: &str,
) -> Result<i64, sqlx::Error> {
    let result = sqlx::query("INSERT INTO stone_types (name, company_id) VALUES (?, ?)")
        .bind(name)
        .bind(company_id)
        .execute(pool)
        .await?;
    Ok(i64::try_from(result.last_insert_id()).unwrap_or(i64::MAX))
}

/// Renames the type and every stone of the company that has it.
pub async fn rename_type(
    pool: &MySqlPool,
    company_id: i32,
    existing: &StoneType,
    name: &str,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE stone_types SET name = ? WHERE id = ? AND company_id = ?")
        .bind(name)
        .bind(existing.id)
        .bind(company_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE stones SET type = ? WHERE company_id = ? AND LOWER(type) = LOWER(?)")
        .bind(name)
        .bind(company_id)
        .bind(&existing.name)
        .execute(&mut *tx)
        .await?;
    tx.commit().await
}

pub async fn stones_using_type(
    pool: &MySqlPool,
    company_id: i32,
    name: &str,
) -> Result<Vec<StoneUsingType>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id, name FROM stones
         WHERE company_id = ? AND deleted_at IS NULL AND LOWER(type) = LOWER(?)
         ORDER BY name ASC, id ASC",
    )
    .bind(company_id)
    .bind(name)
    .fetch_all(pool)
    .await
}

pub async fn delete_type(pool: &MySqlPool, company_id: i32, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM stone_types WHERE id = ? AND company_id = ?")
        .bind(id)
        .bind(company_id)
        .execute(pool)
        .await?;
    Ok(())
}
