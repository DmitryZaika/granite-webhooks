use super::schemas::{Faucet, ListFaucetsQuery};
use crate::sql::push_in;
use sqlx::{MySql, MySqlPool, QueryBuilder};

/// Remix: `faucetQueryBuilder` in `app/utils/queries.server.ts`. A unit is
/// free while it is not deleted and not attached to a slab.
pub async fn list_faucets(
    pool: &MySqlPool,
    company_id: i32,
    filter: &ListFaucetsQuery,
) -> Result<Vec<Faucet>, sqlx::Error> {
    let mut query = QueryBuilder::<MySql>::new(
        "SELECT
            faucet_type.id, faucet_type.name, faucet_type.type, faucet_type.url,
            faucet_type.supplier_id, faucet_type.is_display,
            COALESCE(faucet_type.regular_stock, 0) AS regular_stock,
            CAST(faucet_type.retail_price AS CHAR) AS retail_price,
            CAST(faucet_type.cost AS CHAR) AS cost,
            COUNT(faucets.id) AS available
         FROM faucet_type
         LEFT JOIN faucets ON faucets.faucet_type_id = faucet_type.id
           AND faucets.is_deleted = 0 AND faucets.slab_id IS NULL
         WHERE faucet_type.is_deleted = 0 AND faucet_type.company_id = ",
    );
    query.push_bind(company_id);
    push_in(&mut query, "faucet_type.type", &filter.types);
    if let Some(supplier_id) = filter.supplier_id {
        query
            .push(" AND faucet_type.supplier_id = ")
            .push_bind(supplier_id);
    }
    query.push(" GROUP BY faucet_type.id");
    if !filter.include_sold_out {
        query.push(" HAVING available > 0 OR regular_stock = 1");
    }
    query.push(" ORDER BY faucet_type.name ASC, faucet_type.id ASC");

    query.build_query_as::<Faucet>().fetch_all(pool).await
}
