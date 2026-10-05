use super::schemas::{CompanyRow, CustomerRow, CustomersView, EmailsByCustomer, ListFilter};
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
