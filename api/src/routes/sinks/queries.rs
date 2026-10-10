use super::schemas::{ListSinksQuery, Sink};
use crate::sql::push_in;
use sqlx::{MySql, MySqlPool, QueryBuilder};

/// Remix: `sinkQueryBuilder` in `app/utils/queries.server.ts`. A unit is free
/// while it is not deleted and not attached to a slab.
pub async fn list_sinks(
    pool: &MySqlPool,
    company_id: i32,
    filter: &ListSinksQuery,
) -> Result<Vec<Sink>, sqlx::Error> {
    let mut query = QueryBuilder::<MySql>::new(
        "SELECT
            sink_type.id, sink_type.name, sink_type.type, sink_type.url, sink_type.supplier_id,
            sink_type.is_display,
            COALESCE(sink_type.regular_stock, 0) AS regular_stock,
            sink_type.length, sink_type.width,
            CAST(sink_type.retail_price AS CHAR) AS retail_price,
            CAST(sink_type.cost AS CHAR) AS cost,
            COUNT(sinks.id) AS available
         FROM sink_type
         LEFT JOIN sinks ON sinks.sink_type_id = sink_type.id
           AND sinks.is_deleted = 0 AND sinks.slab_id IS NULL
         WHERE sink_type.is_deleted = 0 AND sink_type.company_id = ",
    );
    query.push_bind(company_id);
    push_in(&mut query, "sink_type.type", &filter.types);
    if let Some(supplier_id) = filter.supplier_id {
        query
            .push(" AND sink_type.supplier_id = ")
            .push_bind(supplier_id);
    }
    query.push(" GROUP BY sink_type.id");
    if !filter.include_sold_out {
        query.push(" HAVING available > 0 OR regular_stock = 1");
    }
    query.push(" ORDER BY sink_type.name ASC, sink_type.id ASC");

    query.build_query_as::<Sink>().fetch_all(pool).await
}
