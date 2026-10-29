use super::schemas::{
    ListStonesQuery, QuickStoneInput, SearchStonesQuery, SlabParentSale, SlabSale, Stone,
    StoneDetail, StoneRef, StoneSlab,
};
use crate::sql::{escape_like, push_in};
use chrono::{DateTime, Utc};
use sqlx::FromRow;
use sqlx::{MySql, MySqlPool, QueryBuilder};

/// `SELECT <Stone columns> FROM stones ... WHERE` live stones of the company,
/// joined to their slabs for the counts. Callers add filters, then
/// `GROUP BY stones.id`.
pub fn stones_with_slab_counts(company_id: i32) -> QueryBuilder<MySql> {
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
         WHERE stones.deleted_at IS NULL AND stones.company_id = ",
    );
    query.push_bind(company_id);
    query
}

/// Remix: `api.stones.search.$companyId.tsx` for signed-in staff. Name
/// matches first (prefix, then whole word, then anywhere), then by name.
pub async fn search_stones(
    pool: &MySqlPool,
    company_id: i32,
    filter: &SearchStonesQuery,
) -> Result<Vec<Stone>, sqlx::Error> {
    let term = filter.name.trim();
    if term.is_empty() {
        return Ok(Vec::new());
    }
    let escaped = escape_like(term);
    let mut query = stones_with_slab_counts(company_id);
    query
        .push(" AND stones.name LIKE ")
        .push_bind(format!("%{escaped}%"));
    if !filter.include_hidden {
        query.push(" AND stones.is_display = 1");
    }
    query.push(" GROUP BY stones.id");
    if !filter.include_sold_out {
        query.push(" HAVING slabs_available > 0 OR regular_stock = 1");
    }
    query
        .push(" ORDER BY CASE WHEN stones.name LIKE ")
        .push_bind(format!("{escaped}%"))
        .push(" THEN 0 WHEN stones.name LIKE ")
        .push_bind(format!("% {escaped} %"))
        .push(" THEN 1 ELSE 2 END, stones.name ASC, stones.id ASC LIMIT ")
        .push_bind(filter.limit.unwrap_or(25).clamp(1, 50));
    query.build_query_as::<Stone>().fetch_all(pool).await
}

/// Remix: `api.stones.quick-add.tsx`. A displayed stone plus its first slabs.
pub async fn quick_add_stone(
    pool: &MySqlPool,
    company_id: i32,
    input: &QuickStoneInput,
    bundles: &[String],
) -> Result<(i32, Vec<i64>), sqlx::Error> {
    let mut tx = pool.begin().await?;
    let length = input.length.unwrap_or(0.0);
    let width = input.width.unwrap_or(0.0);
    let stone = sqlx::query(
        "INSERT INTO stones (name, type, retail_price, width, length, company_id, is_display)
         VALUES (?, ?, ?, ?, ?, ?, 1)",
    )
    .bind(input.name.trim())
    .bind(&input.kind)
    .bind(input.retail_price)
    .bind(width)
    .bind(length)
    .bind(company_id)
    .execute(&mut *tx)
    .await?;
    let stone_id = i32::try_from(stone.last_insert_id()).unwrap_or(i32::MAX);
    let mut slab_ids = Vec::with_capacity(bundles.len());
    for bundle in bundles {
        let slab = sqlx::query(
            "INSERT INTO slab_inventory (bundle, stone_id, width, length, is_leftover, url)
             VALUES (?, ?, ?, ?, ?, NULL)",
        )
        .bind(bundle)
        .bind(stone_id)
        .bind(width.to_string())
        .bind(length.to_string())
        .bind(input.leftover)
        .execute(&mut *tx)
        .await?;
        slab_ids.push(i64::try_from(slab.last_insert_id()).unwrap_or(i64::MAX));
    }
    tx.commit().await?;
    Ok((stone_id, slab_ids))
}

/// Whether the company has this stone type (case-insensitive).
pub async fn company_has_stone_type(
    pool: &MySqlPool,
    company_id: i32,
    name: &str,
) -> Result<bool, sqlx::Error> {
    let found: Option<i64> = sqlx::query_scalar(
        "SELECT CAST(id AS SIGNED) FROM stone_types WHERE company_id = ? AND LOWER(name) = LOWER(?) LIMIT 1",
    )
    .bind(company_id)
    .bind(name)
    .fetch_optional(pool)
    .await?;
    Ok(found.is_some())
}

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
    let mut query = stones_with_slab_counts(company_id);
    if !filter.include_hidden {
        query.push(" AND stones.is_display = 1");
    }

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
    child_count: i64,
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
            CAST(slab.id AS SIGNED) AS id, slab.bundle, NULLIF(slab.url, 'undefined') AS url, slab.length, slab.width,
            CAST(slab.parent_id AS SIGNED) AS parent_id,
            COALESCE(slab.is_leftover, 0) AS is_leftover,
            (SELECT COUNT(*) FROM slab_inventory child
              WHERE child.parent_id = slab.id AND child.deleted_at IS NULL) AS child_count,
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
                child_count: row.child_count,
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
