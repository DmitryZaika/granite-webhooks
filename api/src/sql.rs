//! Small helpers for building SQL with `QueryBuilder`.

use sqlx::{MySql, QueryBuilder};

/// ` AND <column> IN (?, ?, ...)`, or nothing for an empty list.
pub fn push_in<T>(query: &mut QueryBuilder<MySql>, column: &str, values: &[T])
where
    T: Clone + for<'q> sqlx::Encode<'q, MySql> + sqlx::Type<MySql>,
{
    if values.is_empty() {
        return;
    }
    query.push(format!(" AND {column} IN ("));
    let mut separated = query.separated(", ");
    for value in values {
        separated.push_bind(value.clone());
    }
    query.push(")");
}
