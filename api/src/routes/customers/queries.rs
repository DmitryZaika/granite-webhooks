use super::fuzzy::matches_name_fuzzy;
use super::schemas::{
    CompanyRow, CustomerDetail, CustomerRow, CustomerSearchBy, CustomerSearchQuery,
    CustomerSearchRow, CustomersView, EmailsByCustomer, ListFilter,
};
use sqlx::{FromRow, MySql, MySqlPool, QueryBuilder};

const CUSTOMER_COLUMNS: &str = "
    c.id, c.name, primary_email.email AS email, c.phone, c.phone_2, c.address, c.sales_rep,
    c.created_date, c.assigned_date, u.name AS sales_rep_name, c.company_id, c.source,
    c.invalid_lead, c.company_name, c.customer_temperature AS customerTemperature";

const COMPANY_TOTALS: &str = ",
    CAST((
      SELECT SUM(s.price)
      FROM sales s
      LEFT JOIN customers sub ON sub.id = s.customer_id
      WHERE s.customer_id = c.id OR sub.parent_id = c.id
    ) AS CHAR) AS revenue_generated,
    (
      SELECT COUNT(*)
      FROM sales s
      LEFT JOIN customers sub ON sub.id = s.customer_id
      WHERE s.customer_id = c.id OR sub.parent_id = c.id
    ) AS projects_count";

const CUSTOMER_JOINS: &str = "
    FROM customers c
    LEFT JOIN customers_emails primary_email
      ON primary_email.id = c.email_id AND primary_email.customer_id = c.id
    LEFT JOIN users u ON c.sales_rep = u.id AND u.is_deleted = 0
    WHERE c.deleted_at IS NULL AND c.company_id = ";

fn list_query(company_id: i32, filter: ListFilter) -> QueryBuilder<MySql> {
    let mut query = QueryBuilder::new("SELECT");
    query.push(CUSTOMER_COLUMNS);
    if filter.view == CustomersView::Companies {
        query.push(COMPANY_TOTALS);
    }
    query.push(CUSTOMER_JOINS).push_bind(company_id);
    if filter.view == CustomersView::Companies {
        query.push(" AND (c.company_name IS NOT NULL AND c.company_name != '')");
    }
    if let Some(sales_rep) = filter.sales_rep {
        query.push(" AND c.sales_rep = ").push_bind(sales_rep);
    }
    if !filter.include_invalid && filter.view != CustomersView::Companies {
        query.push(" AND (c.invalid_lead IS NULL OR c.invalid_lead = '')");
    }
    query
}

async fn fetch_list<T>(
    pool: &MySqlPool,
    company_id: i32,
    filter: ListFilter,
) -> Result<Vec<T>, sqlx::Error>
where
    T: for<'r> FromRow<'r, sqlx::mysql::MySqlRow> + Send + Unpin,
{
    list_query(company_id, filter)
        .build_query_as::<T>()
        .fetch_all(pool)
        .await
}

pub async fn list_customers(
    pool: &MySqlPool,
    company_id: i32,
    filter: ListFilter,
) -> Result<Vec<CustomerRow>, sqlx::Error> {
    debug_assert_eq!(filter.view, CustomersView::Customers);
    fetch_list(pool, company_id, filter).await
}

pub async fn list_companies(
    pool: &MySqlPool,
    company_id: i32,
    filter: ListFilter,
) -> Result<Vec<CompanyRow>, sqlx::Error> {
    debug_assert_eq!(filter.view, CustomersView::Companies);
    fetch_list(pool, company_id, filter).await
}

#[derive(FromRow)]
struct CustomerEmailRow {
    customer_id: i32,
    email: String,
}

/// Placeholders per statement; keeps far below MySQL's 65,535 limit.
const EMAIL_CHUNK: usize = 1_000;

/// Remix: `loadCustomerEmailsByIds`, additionally scoped to the caller's
/// company because the ids now come from the client.
pub async fn emails_by_customer_ids(
    pool: &MySqlPool,
    company_id: i32,
    customer_ids: &[i32],
) -> Result<EmailsByCustomer, sqlx::Error> {
    let mut ids = customer_ids.to_vec();
    ids.sort_unstable();
    ids.dedup();

    let mut emails = EmailsByCustomer::new();
    for chunk in ids.chunks(EMAIL_CHUNK) {
        let mut query = QueryBuilder::<MySql>::new(
            "SELECT ce.customer_id, ce.email
             FROM customers_emails ce
             JOIN customers c ON c.id = ce.customer_id
             WHERE c.company_id = ",
        );
        query.push_bind(company_id).push(" AND ce.customer_id IN (");
        let mut separated = query.separated(", ");
        for id in chunk {
            separated.push_bind(*id);
        }
        query.push(
            ") ORDER BY ce.customer_id, ce.id = c.email_id DESC, ce.created_at ASC, ce.id ASC",
        );
        let rows = query
            .build_query_as::<CustomerEmailRow>()
            .fetch_all(pool)
            .await?;
        for row in rows {
            emails.entry(row.customer_id).or_default().push(row.email);
        }
    }
    Ok(emails)
}

/// Remix: the loader of `api.customers.$customerId.tsx`.
pub async fn find_customer(
    pool: &MySqlPool,
    company_id: i32,
    customer_id: i32,
) -> Result<Option<CustomerDetail>, sqlx::Error> {
    let customer = sqlx::query_as::<_, CustomerDetail>(
        "SELECT c.id, c.name, c.address, c.phone, c.phone_2, ce.email, c.company_name,
            c.source, c.referral_source, c.your_message, c.sales_rep, u.name AS sales_rep_name
         FROM customers c
         LEFT JOIN customers_emails ce ON ce.id = c.email_id AND ce.customer_id = c.id
         LEFT JOIN users u ON u.id = c.sales_rep AND u.is_deleted = 0
         WHERE c.id = ? AND c.company_id = ? AND c.deleted_at IS NULL",
    )
    .bind(customer_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    let Some(mut customer) = customer else {
        return Ok(None);
    };
    customer.emails = emails_by_customer_ids(pool, company_id, &[customer_id])
        .await?
        .remove(&customer_id)
        .unwrap_or_default();
    Ok(Some(customer))
}

/// Rows a name or company search fetches before the typo filter keeps 15.
const SEARCH_PREFETCH: i64 = 100;
const SEARCH_RESULTS: usize = 15;

/// Remix: `api.customers.search.tsx`.
///
/// The Remix SQL used `SELECT DISTINCT`
/// with a join on deals and ordered by a deal column, which MySQL 8 rejects
/// (error 3065); deals are an `EXISTS` in the ordering here instead.
pub async fn search_customers(
    pool: &MySqlPool,
    company_id: i32,
    user_id: i32,
    query: &CustomerSearchQuery,
) -> Result<Vec<CustomerSearchRow>, sqlx::Error> {
    let term = query.term.trim();
    let mut sql = QueryBuilder::<MySql>::new(
        "SELECT c.id, c.name, c.address, c.phone, c.phone_2, primary_email.email AS email,
            c.company_name,
            (SELECT d.id FROM deals d WHERE d.customer_id = c.id AND d.deleted_at IS NULL
              ORDER BY d.id DESC LIMIT 1) AS deal_id,
            (SELECT d.is_won FROM deals d WHERE d.customer_id = c.id AND d.deleted_at IS NULL
              ORDER BY d.id DESC LIMIT 1) AS deal_is_won,
            (SELECT l.group_id FROM deals d JOIN deals_list l ON l.id = d.list_id
              WHERE d.customer_id = c.id AND d.deleted_at IS NULL
              ORDER BY d.id DESC LIMIT 1) AS deal_group_id
         FROM customers c
         LEFT JOIN customers_emails primary_email
           ON primary_email.id = c.email_id AND primary_email.customer_id = c.id
         WHERE c.deleted_at IS NULL AND c.company_id = ",
    );
    sql.push_bind(company_id);
    if query.own_only {
        sql.push(" AND (c.sales_rep IS NULL OR c.sales_rep = ")
            .push_bind(user_id)
            .push(")");
    }
    let limit = push_search_filter(&mut sql, query.by, term, user_id);
    sql.push(", c.id ASC LIMIT ").push_bind(limit);

    let mut rows = sql
        .build_query_as::<CustomerSearchRow>()
        .fetch_all(pool)
        .await?;
    let lowered = term.to_lowercase();
    let company_contains = |row: &CustomerSearchRow| {
        row.company_name
            .as_deref()
            .is_some_and(|company| company.to_lowercase().contains(&lowered))
    };
    match query.by {
        CustomerSearchBy::Name => rows.retain(|row| {
            matches_name_fuzzy(row.name.as_deref().unwrap_or_default(), term)
                || matches_name_fuzzy(row.company_name.as_deref().unwrap_or_default(), term)
                || company_contains(row)
        }),
        CustomerSearchBy::Company => rows.retain(|row| {
            matches_name_fuzzy(row.company_name.as_deref().unwrap_or_default(), term)
                || company_contains(row)
        }),
        CustomerSearchBy::Phone | CustomerSearchBy::Email => {}
    }
    rows.truncate(SEARCH_RESULTS);

    let ids: Vec<i32> = rows.iter().map(|row| row.id).collect();
    let mut emails = emails_by_customer_ids(pool, company_id, &ids).await?;
    for row in &mut rows {
        row.emails = emails
            .remove(&row.id)
            .unwrap_or_else(|| row.email.clone().into_iter().collect());
    }
    Ok(rows)
}

const PHONE: &str = "REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(c.phone,'-',''),' ',''),'(',''),')',''),'+',''),'.','')";
const PHONE_2: &str = "REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(c.phone_2,'-',''),' ',''),'(',''),')',''),'+',''),'.','')";

/// ` AND <match>` plus ` ORDER BY ...` for one search mode; returns the
/// SQL row limit.
fn push_search_filter(
    sql: &mut QueryBuilder<MySql>,
    by: CustomerSearchBy,
    term: &str,
    user_id: i32,
) -> i64 {
    let like = format!("%{term}%");
    let prefix = format!("{term}%");
    let company_match = |sql: &mut QueryBuilder<MySql>| {
        sql.push("(c.company_name IS NOT NULL AND c.company_name != '' AND (c.company_name LIKE ")
            .push_bind(prefix.clone())
            .push(" OR c.company_name LIKE ")
            .push_bind(like.clone())
            .push("))");
    };
    let digits: String = term.chars().filter(char::is_ascii_digit).collect();
    let last10: String = digits
        .chars()
        .skip(digits.chars().count().saturating_sub(10))
        .collect();
    match by {
        CustomerSearchBy::Name => {
            let words: Vec<&str> = term.split_whitespace().collect();
            let short = |word: &str| format!("%{}%", word.chars().take(3).collect::<String>());
            sql.push(" AND ((c.name LIKE ");
            if words.len() > 1 {
                sql.push_bind(format!("%{}%", words.join(" ")))
                    .push(" OR (");
                for (index, word) in words.iter().enumerate() {
                    if index > 0 {
                        sql.push(" AND ");
                    }
                    sql.push("c.name LIKE ").push_bind(short(word));
                }
                sql.push("))");
            } else {
                sql.push_bind(short(term)).push(")");
            }
            sql.push(" OR ");
            company_match(sql);
            sql.push(")");
            push_rank(sql, user_id, "c.name LIKE ", &prefix, &like);
            SEARCH_PREFETCH
        }
        CustomerSearchBy::Company => {
            sql.push(" AND ");
            company_match(sql);
            push_rank(sql, user_id, "c.company_name LIKE ", &prefix, &like);
            SEARCH_PREFETCH
        }
        CustomerSearchBy::Phone if digits.is_empty() => {
            sql.push(" AND (c.phone LIKE ")
                .push_bind(like.clone())
                .push(" OR c.phone_2 LIKE ")
                .push_bind(like.clone())
                .push(")");
            push_owner_rank(sql, user_id);
            sql.push(" CASE WHEN c.phone LIKE ")
                .push_bind(prefix.clone())
                .push(" OR c.phone_2 LIKE ")
                .push_bind(prefix.clone())
                .push(" THEN 0 WHEN c.phone LIKE ")
                .push_bind(like.clone())
                .push(" OR c.phone_2 LIKE ")
                .push_bind(like.clone())
                .push(" THEN 1 ELSE 2 END, c.name ASC");
            15
        }
        CustomerSearchBy::Phone => {
            sql.push(" AND ");
            push_phone_match(sql, format!("%{digits}%"), format!("%{last10}%"));
            push_owner_rank(sql, user_id);
            sql.push(" CASE WHEN ");
            push_phone_match(sql, format!("{digits}%"), format!("{last10}%"));
            sql.push(" THEN 0 WHEN ");
            push_phone_match(sql, format!("%{digits}%"), format!("%{last10}%"));
            sql.push(" THEN 1 ELSE 2 END, c.name ASC");
            15
        }
        CustomerSearchBy::Email => {
            let has_email = |sql: &mut QueryBuilder<MySql>, pattern: String| {
                sql.push(
                    "EXISTS (SELECT 1 FROM customers_emails search_email
                      WHERE search_email.customer_id = c.id AND search_email.email LIKE ",
                )
                .push_bind(pattern)
                .push(")");
            };
            sql.push(" AND (");
            has_email(sql, prefix.clone());
            sql.push(" OR ");
            has_email(sql, like.clone());
            sql.push(")");
            push_owner_rank(sql, user_id);
            sql.push(" CASE WHEN ");
            has_email(sql, prefix.clone());
            sql.push(" THEN 0 WHEN ");
            has_email(sql, like.clone());
            sql.push(" THEN 1 ELSE 2 END, c.name ASC");
            15
        }
    }
}

/// Either phone column, digits only, matches either pattern.
fn push_phone_match(sql: &mut QueryBuilder<MySql>, a: String, b: String) {
    sql.push(format!("({PHONE} LIKE "))
        .push_bind(a.clone())
        .push(format!(" OR {PHONE} LIKE "))
        .push_bind(b.clone())
        .push(format!(" OR {PHONE_2} LIKE "))
        .push_bind(a)
        .push(format!(" OR {PHONE_2} LIKE "))
        .push_bind(b)
        .push(")");
}

/// ` ORDER BY` customers with the caller's live deal, then the caller's own
/// customers, then the rest (each followed by the caller's next keys).
fn push_owner_rank(sql: &mut QueryBuilder<MySql>, user_id: i32) {
    sql.push(
        " ORDER BY CASE WHEN EXISTS (SELECT 1 FROM deals mine
            WHERE mine.customer_id = c.id AND mine.user_id = ",
    )
    .push_bind(user_id)
    .push(" AND mine.deleted_at IS NULL) THEN 0 WHEN c.sales_rep = ")
    .push_bind(user_id)
    .push(" THEN 1 ELSE 2 END,");
}

fn push_rank(sql: &mut QueryBuilder<MySql>, user_id: i32, column: &str, prefix: &str, like: &str) {
    push_owner_rank(sql, user_id);
    sql.push(format!(" CASE WHEN {column}"))
        .push_bind(prefix.to_string())
        .push(format!(" THEN 0 WHEN {column}"))
        .push_bind(like.to_string())
        .push(" THEN 1 ELSE 2 END, c.name ASC");
}
