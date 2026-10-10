use sqlx::MySqlPool;

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
