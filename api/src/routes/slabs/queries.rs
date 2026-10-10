use super::schemas::SlabSummary;
use crate::sql::push_in;
use sqlx::{MySql, MySqlPool, QueryBuilder};

/// Remix: the `updateSize` intent of `employee.stones.slabs.$stone.tsx`.
///
/// Sizes are stored as text the way JavaScript prints numbers (`130`,
/// `65.5`). Only slabs of the company's stones are touched; returns whether
/// the slab exists.
pub async fn update_slab_size(
    pool: &MySqlPool,
    company_id: i32,
    slab_id: i64,
    length: f64,
    width: f64,
) -> Result<bool, sqlx::Error> {
    let exists: Option<i64> = sqlx::query_scalar(
        "SELECT CAST(slab.id AS SIGNED) FROM slab_inventory slab
         JOIN stones ON stones.id = slab.stone_id
         WHERE slab.id = ? AND stones.company_id = ? AND slab.deleted_at IS NULL",
    )
    .bind(slab_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    if exists.is_none() {
        return Ok(false);
    }
    sqlx::query("UPDATE slab_inventory SET length = ?, width = ? WHERE id = ?")
        .bind(length.to_string())
        .bind(width.to_string())
        .bind(slab_id)
        .execute(pool)
        .await?;
    Ok(true)
}

/// Remix: `api.slabNames.tsx` and `api.stoneInfo.$slabId.ts`. Slabs of the
/// company's stones (deleted slabs included, as on old sales), by id.
pub async fn slabs_by_ids(
    pool: &MySqlPool,
    company_id: i32,
    ids: &[i64],
) -> Result<Vec<SlabSummary>, sqlx::Error> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut query = QueryBuilder::<MySql>::new(
        "SELECT CAST(slab.id AS SIGNED) AS id, slab.bundle,
            COALESCE(slab.is_leftover, 0) AS is_leftover,
            stones.id AS stone_id, stones.name AS stone_name, stones.type AS stone_type
         FROM slab_inventory slab
         JOIN stones ON stones.id = slab.stone_id
         WHERE stones.company_id = ",
    );
    query.push_bind(company_id);
    push_in(&mut query, "slab.id", ids);
    query.push(" ORDER BY slab.id");
    query.build_query_as::<SlabSummary>().fetch_all(pool).await
}
