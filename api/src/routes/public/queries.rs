use super::schemas::{
    PublicFixture, PublicFixturesQuery, PublicPhoto, PublicSearchQuery, PublicSlab, PublicStone,
    PublicStonesQuery,
};
use crate::routes::catalog::schemas::Color;
use crate::routes::fixtures::Kind;
use crate::routes::stones::queries::escape_like;
use crate::sql::push_in;
use sqlx::{MySql, MySqlPool, QueryBuilder};

/// Displayed, live stones of the company with their slab counts, `WHERE`
/// open for more filters.
fn showroom_stones(company_id: i32) -> QueryBuilder<MySql> {
    let mut query = QueryBuilder::<MySql>::new(
        "SELECT
            stones.id, stones.name, stones.type, stones.url, stones.length, stones.width,
            COALESCE(stones.on_sale, 0) AS on_sale,
            COALESCE(stones.regular_stock, 0) AS regular_stock,
            stones.retail_price,
            CASE WHEN COALESCE(stones.retail_price, 0) = 0 THEN stones.cost_per_sqft END
              AS slab_price_per_sqft,
            stones.level, stones.finishing, stones.bundle_number, stones.created_date,
            COUNT(slab.id) AS slabs_total,
            COUNT(CASE WHEN slab.sale_id IS NULL THEN slab.id END) AS slabs_available,
            COUNT(CASE WHEN slab.parent_id IS NULL THEN slab.id END) AS slabs_whole_total,
            COUNT(CASE WHEN slab.parent_id IS NULL AND slab.sale_id IS NULL THEN slab.id END)
              AS slabs_whole_available
         FROM stones
         LEFT JOIN (
            SELECT id AS stone_id, id AS source_stone_id FROM stones WHERE company_id = ",
    );
    query.push_bind(company_id).push(
        "
            UNION
            SELECT stone_id, source_stone_id FROM stone_slab_links
         ) slab_sources ON slab_sources.stone_id = stones.id
         LEFT JOIN slab_inventory slab ON slab.stone_id = slab_sources.source_stone_id
           AND slab.cut_date IS NULL AND slab.deleted_at IS NULL
         WHERE stones.deleted_at IS NULL AND stones.is_display = 1 AND stones.company_id = ",
    );
    query.push_bind(company_id);
    query
}

/// In stock for the showroom: regular stock, or a whole slab available
/// (remnant-only stones are left out).
const SHOWROOM_STOCK: &str = " HAVING (slabs_available > 0 OR regular_stock = 1)
    AND (regular_stock = 1 OR slabs_whole_available > 0)";

/// Remix: `stoneQueryBuilder(filters, company, false, true)` with
/// `show_sold_out = false`.
pub async fn list_stones(
    pool: &MySqlPool,
    company_id: i32,
    filter: &PublicStonesQuery,
) -> Result<Vec<PublicStone>, sqlx::Error> {
    let mut query = showroom_stones(company_id);
    push_in(&mut query, "stones.type", &filter.types);
    if !filter.color_ids.is_empty() {
        query.push(
            " AND EXISTS (SELECT 1 FROM stone_colors
                WHERE stone_colors.stone_id = stones.id AND stone_colors.color_id IN (",
        );
        let mut ids = query.separated(", ");
        for id in &filter.color_ids {
            ids.push_bind(*id);
        }
        query.push("))");
    }
    push_in(&mut query, "stones.level", &filter.level);
    push_in(&mut query, "stones.finishing", &filter.finishing);
    query.push(" GROUP BY stones.id");
    query.push(SHOWROOM_STOCK);
    query.push(" ORDER BY stones.name ASC, stones.id ASC");
    query.build_query_as().fetch_all(pool).await
}

/// Remix: `api.stones.search.$companyId.tsx` for visitors.
pub async fn search_stones(
    pool: &MySqlPool,
    company_id: i32,
    filter: &PublicSearchQuery,
) -> Result<Vec<PublicStone>, sqlx::Error> {
    let term = filter.name.trim();
    if term.is_empty() {
        return Ok(Vec::new());
    }
    let escaped = escape_like(term);
    let mut query = showroom_stones(company_id);
    query
        .push(" AND stones.name LIKE ")
        .push_bind(format!("%{escaped}%"));
    query.push(" GROUP BY stones.id");
    query.push(SHOWROOM_STOCK);
    query
        .push(" ORDER BY CASE WHEN stones.name LIKE ")
        .push_bind(format!("{escaped}%"))
        .push(" THEN 0 WHEN stones.name LIKE ")
        .push_bind(format!("% {escaped} %"))
        .push(" THEN 1 ELSE 2 END, stones.name ASC, stones.id ASC LIMIT ")
        .push_bind(filter.limit.unwrap_or(25).clamp(1, 50));
    query.build_query_as().fetch_all(pool).await
}

/// A displayed stone of the company, sold out or not.
pub async fn find_stone(
    pool: &MySqlPool,
    company_id: i32,
    stone_id: i32,
) -> Result<Option<PublicStone>, sqlx::Error> {
    let mut query = showroom_stones(company_id);
    query.push(" AND stones.id = ").push_bind(stone_id);
    query.push(" GROUP BY stones.id");
    query.build_query_as().fetch_optional(pool).await
}

/// Remix: the loader of `customer.$company.stones.slabs.$stone.tsx`. Free
/// slabs (not sold, cut or deleted) of the stone and the stones it draws from.
pub async fn stone_slabs(
    pool: &MySqlPool,
    company_id: i32,
    stone_id: i32,
) -> Result<Vec<PublicSlab>, sqlx::Error> {
    sqlx::query_as(
        "SELECT CAST(slab.id AS SIGNED) AS id, slab.bundle, slab.url, slab.length, slab.width
         FROM slab_inventory slab
         JOIN stones source ON source.id = slab.stone_id AND source.company_id = ?
         WHERE slab.cut_date IS NULL AND slab.deleted_at IS NULL AND slab.sale_id IS NULL
           AND (slab.stone_id = ? OR slab.stone_id IN (
                SELECT source_stone_id FROM stone_slab_links WHERE stone_id = ?))
         ORDER BY slab.id",
    )
    .bind(company_id)
    .bind(stone_id)
    .bind(stone_id)
    .fetch_all(pool)
    .await
}

/// Installed photos of a displayed stone, its own then linked stones'.
pub async fn stone_photos(
    pool: &MySqlPool,
    company_id: i32,
    stone_id: i32,
) -> Result<Vec<PublicPhoto>, sqlx::Error> {
    sqlx::query_as(
        "SELECT image.id, image.url
         FROM installed_stones image
         JOIN stones ON stones.id = image.stone_id AND stones.company_id = ?
         WHERE image.stone_id = ?
            OR image.stone_id IN (SELECT source_stone_id FROM stone_image_links WHERE stone_id = ?)
         ORDER BY image.stone_id <> ?, stones.name ASC, image.id ASC",
    )
    .bind(company_id)
    .bind(stone_id)
    .bind(stone_id)
    .bind(stone_id)
    .fetch_all(pool)
    .await
}

/// Displayed models of the company with a free unit or regular stock.
pub async fn list_fixtures(
    pool: &MySqlPool,
    kind: Kind,
    company_id: i32,
    filter: &PublicFixturesQuery,
) -> Result<Vec<PublicFixture>, sqlx::Error> {
    let size = if kind.sized {
        "m.length, m.width"
    } else {
        "CAST(NULL AS SIGNED) AS length, CAST(NULL AS SIGNED) AS width"
    };
    let mut query = QueryBuilder::<MySql>::new(format!(
        "SELECT m.id, m.name, m.type, m.url, {size},
            COALESCE(m.regular_stock, 0) AS regular_stock,
            COUNT(u.id) AS available, CAST(m.retail_price AS CHAR) AS retail_price
         FROM {table} m
         LEFT JOIN {units} u ON u.{col} = m.id AND u.is_deleted = 0 AND u.slab_id IS NULL
         WHERE m.is_deleted = 0 AND m.is_display = 1 AND m.company_id = ",
        table = kind.model_table,
        units = kind.unit_table,
        col = kind.unit_model_column,
    ));
    query.push_bind(company_id);
    push_in(&mut query, "m.type", &filter.types);
    query.push(
        " GROUP BY m.id HAVING available > 0 OR regular_stock = 1 ORDER BY m.name ASC, m.id ASC",
    );
    query.build_query_as().fetch_all(pool).await
}

pub async fn list_colors(pool: &MySqlPool) -> Result<Vec<Color>, sqlx::Error> {
    crate::routes::catalog::queries::list_colors(pool).await
}

/// Remix: `customer.stones.tsx` (company by the request's subdomain).
pub async fn company_by_subdomain(
    pool: &MySqlPool,
    subdomain: &str,
) -> Result<Option<i32>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT id FROM company WHERE subdomain = ? AND deleted_at IS NULL ORDER BY id LIMIT 1",
    )
    .bind(subdomain)
    .fetch_optional(pool)
    .await
}
