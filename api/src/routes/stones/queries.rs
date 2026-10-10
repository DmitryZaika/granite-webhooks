use super::schemas::{
    ListStonesQuery, SlabParentSale, SlabSale, Stone, StoneDetail, StoneRef, StoneSlab,
};
use crate::sql::push_in;
use chrono::{DateTime, Utc};
use sqlx::FromRow;
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

/// A non-deleted stone of the company, with its color ids.
pub async fn find_stone(
    pool: &MySqlPool,
    company_id: i32,
    stone_id: i32,
) -> Result<Option<StoneDetail>, sqlx::Error> {
    let stone = sqlx::query_as::<_, StoneDetail>(
        "SELECT id, name, type, url, supplier_id,
            COALESCE(is_display, 0) AS is_display,
            COALESCE(on_sale, 0) AS on_sale,
            COALESCE(regular_stock, 0) AS regular_stock,
            length, width, retail_price, cost_per_sqft, level, finishing,
            samples_amount, samples_importance, bundle_number, bundle_location,
            DATE_FORMAT(delivery_date, '%Y-%m-%d') AS delivery_date, created_date
         FROM stones
         WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(stone_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    let Some(mut stone) = stone else {
        return Ok(None);
    };
    stone.color_ids = sqlx::query_scalar(
        "SELECT color_id FROM stone_colors WHERE stone_id = ? ORDER BY color_id",
    )
    .bind(stone_id)
    .fetch_all(pool)
    .await?;
    Ok(Some(stone))
}

#[derive(FromRow)]
struct SlabRow {
    id: i64,
    bundle: Option<String>,
    url: Option<String>,
    length: Option<String>,
    width: Option<String>,
    parent_id: Option<i64>,
    is_leftover: bool,
    stone_id: i32,
    stone_name: Option<String>,
    sale_id: Option<i32>,
    found_sale_id: Option<i32>,
    sale_date: Option<DateTime<Utc>>,
    customer_name: Option<String>,
    seller_name: Option<String>,
    sale_notes: Option<String>,
    sale_square_feet: Option<String>,
    slab_notes: Option<String>,
    room: Option<String>,
    slab_square_feet: Option<String>,
    parent_sale_id: Option<i32>,
    parent_customer_name: Option<String>,
    parent_seller_name: Option<String>,
}

#[derive(FromRow)]
struct SinkRow {
    sale_id: i32,
    name: String,
}

/// Remix: the loader of `employee.stones.slabs.$stone.tsx` (and its admin copy).
///
/// Slabs of the stone and of the stones it draws from
/// (`stone_slab_links`), excluding cut and deleted ones, each with its sale
/// and the sale of the slab it was cut from. The caller has already checked
/// that `stone_id` belongs to `company_id`; linked stones are scoped too.
pub async fn list_stone_slabs(
    pool: &MySqlPool,
    company_id: i32,
    stone_id: i32,
) -> Result<Vec<StoneSlab>, sqlx::Error> {
    let rows = sqlx::query_as::<_, SlabRow>(
        "SELECT
            CAST(slab.id AS SIGNED) AS id, slab.bundle, slab.url, slab.length, slab.width,
            CAST(slab.parent_id AS SIGNED) AS parent_id,
            COALESCE(slab.is_leftover, 0) AS is_leftover,
            source.id AS stone_id, source.name AS stone_name,
            slab.sale_id, sale.id AS found_sale_id, sale.sale_date,
            customer.name AS customer_name, seller.name AS seller_name,
            sale.notes AS sale_notes, CAST(sale.square_feet AS CHAR) AS sale_square_feet,
            slab.notes AS slab_notes, slab.room,
            CAST(slab.square_feet AS CHAR) AS slab_square_feet,
            parent_sale.id AS parent_sale_id,
            parent_customer.name AS parent_customer_name,
            parent_seller.name AS parent_seller_name
         FROM slab_inventory slab
         JOIN stones source ON source.id = slab.stone_id AND source.company_id = ?
         LEFT JOIN sales sale ON sale.id = slab.sale_id
         LEFT JOIN customers customer ON customer.id = sale.customer_id
         LEFT JOIN users seller ON seller.id = sale.seller_id
         LEFT JOIN slab_inventory parent ON parent.id = slab.parent_id
         LEFT JOIN sales parent_sale ON parent_sale.id = parent.sale_id
         LEFT JOIN customers parent_customer ON parent_customer.id = parent_sale.customer_id
         LEFT JOIN users parent_seller ON parent_seller.id = parent_sale.seller_id
         WHERE slab.cut_date IS NULL AND slab.deleted_at IS NULL
           AND (slab.stone_id = ? OR slab.stone_id IN (
                SELECT source_stone_id FROM stone_slab_links WHERE stone_id = ?))
         ORDER BY slab.id",
    )
    .bind(company_id)
    .bind(stone_id)
    .bind(stone_id)
    .fetch_all(pool)
    .await?;

    let sale_ids: Vec<i32> = rows.iter().filter_map(|row| row.found_sale_id).collect();
    let sinks: Vec<SinkRow> = if sale_ids.is_empty() {
        Vec::new()
    } else {
        let mut query = QueryBuilder::<MySql>::new(
            "SELECT slab.sale_id, sink_type.name
             FROM sinks
             JOIN sink_type ON sink_type.id = sinks.sink_type_id
             JOIN slab_inventory slab ON slab.id = sinks.slab_id
             WHERE 1 = 1",
        );
        push_in(&mut query, "slab.sale_id", &sale_ids);
        query.push(" ORDER BY sinks.id");
        query.build_query_as::<SinkRow>().fetch_all(pool).await?
    };

    Ok(rows
        .into_iter()
        .map(|row| {
            let sale = row.found_sale_id.map(|id| SlabSale {
                id,
                sale_date: row.sale_date,
                customer_name: row.customer_name,
                seller_name: row.seller_name,
                notes: row.sale_notes,
                square_feet: row.sale_square_feet,
                slab_notes: row.slab_notes,
                room: row.room,
                slab_square_feet: row.slab_square_feet,
                sink_names: sinks
                    .iter()
                    .filter(|sink| sink.sale_id == id)
                    .map(|sink| sink.name.clone())
                    .collect(),
            });
            StoneSlab {
                id: row.id,
                bundle: row.bundle,
                url: row.url,
                length: row.length,
                width: row.width,
                parent_id: row.parent_id,
                is_leftover: row.is_leftover,
                source_stone: (row.stone_id != stone_id).then_some(StoneRef {
                    id: row.stone_id,
                    name: row.stone_name,
                }),
                sale_id: row.sale_id,
                sale,
                parent_sale: row.parent_sale_id.map(|id| SlabParentSale {
                    id,
                    customer_name: row.parent_customer_name,
                    seller_name: row.parent_seller_name,
                }),
            }
        })
        .collect())
}
