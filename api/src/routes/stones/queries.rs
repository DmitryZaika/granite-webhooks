use super::schemas::{ListStonesQuery, Stone};
use crate::sql::push_in;
use sqlx::{MySql, MySqlPool, QueryBuilder};

/// Remix: `stoneQueryBuilder` in `app/utils/queries.server.ts` as the employee
/// stones page called it (displayed stones only, remnant-only stones kept).
///
/// Each stone draws slabs from itself plus any linked source stones. Joining
/// through that small pair list (`slab_sources`) keeps the slab join indexed;
/// an OR with a correlated IN made MySQL compare every stone with every slab.
/// The join already drops cut and deleted slabs, so the counts only split the
/// rest by sale and by whole slab vs remnant (`parent_id`).
pub async fn list_stones(
    pool: &MySqlPool,
    company_id: i32,
    filter: &ListStonesQuery,
) -> Result<Vec<Stone>, sqlx::Error> {
    let mut query = QueryBuilder::<MySql>::new(
        "SELECT
            stones.id, stones.name, stones.type, stones.url, stones.supplier_id,
            COALESCE(stones.is_display, 0) AS is_display,
            COALESCE(stones.on_sale, 0) AS on_sale,
            COALESCE(stones.regular_stock, 0) AS regular_stock,
            stones.length, stones.width, stones.retail_price, stones.cost_per_sqft,
            stones.level, stones.finishing, stones.samples_amount, stones.samples_importance,
            stones.bundle_number, stones.bundle_location,
            DATE_FORMAT(stones.delivery_date, '%Y-%m-%d') AS delivery_date,
            stones.created_date,
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

    push_in(&mut query, "stones.type", &filter.types);
    if let Some(supplier_id) = filter.supplier_id {
        query
            .push(" AND stones.supplier_id = ")
            .push_bind(supplier_id);
    }
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
    if !filter.include_sold_out {
        query.push(" HAVING slabs_available > 0 OR regular_stock = 1");
    }
    query.push(" ORDER BY stones.name ASC, stones.id ASC");

    query.build_query_as::<Stone>().fetch_all(pool).await
}
