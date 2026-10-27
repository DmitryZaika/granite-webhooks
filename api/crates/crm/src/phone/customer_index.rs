//! The customer phone index (`customer_phone_index`).
//!
//! It holds the last 10 digits of each live customer's phone and phone 2,
//! so a call finds its customer with an index lookup instead of reading
//! every free-text phone of the company.
//!
//! - [`lookup`] answers `contacts::customers_for_numbers` once the company's
//!   index is [`built`]; until then that reads the customers' phones as
//!   before.
//! - The phone job keeps the index in step ([`refresh`]): customers changed
//!   since its last run (`customers.updated_at`) and new ones (ids above the
//!   last one it took) get their rows rewritten, so edited, deleted and moved
//!   customers lose their old rows. What changed in the last
//!   [`SETTLE_MINUTES`] minutes is read again by the next run, so a write
//!   that commits a little late is still taken.
//! - The job fills each company's index the first time ([`fill`]), a batch
//!   at a time and at most [`FILL_PER_RUN`] customers a run, picking up where
//!   the last run stopped (`customer_phone_index_builds`).
//! - The app's own new contacts and added numbers are indexed at once, in
//!   their transaction ([`index_new_customer`], [`reindex_customer`]).
//!
//! Rewriting a customer deletes all its rows and inserts the current ones,
//! so it gives the same rows whenever it runs. A row can lag behind a CRM
//! edit until the next run; [`lookup`] checks each row against the
//! customer's phones as they are now, so a stale row never names the wrong
//! customer.

use super::contacts::index_phones;
use chrono::{DateTime, Duration, Utc};
use sqlx::{FromRow, MySqlConnection, MySqlPool, QueryBuilder};
use std::collections::HashMap;
use std::time::Instant;

/// Customers rewritten per refresh batch (one transaction each).
const REFRESH_BATCH: usize = 1_000;
/// Customers read per batch of a first fill (one transaction each).
const FILL_BATCH: usize = 1_000;
/// The most customers a run's first fills read, so a big company's fill
/// never holds up the job's other steps (the next run goes on with it).
pub const FILL_PER_RUN: usize = 5_000;
/// What changed or was added this recently is read again by the next run:
/// an update or insert that commits up to this late is still taken.
pub const SETTLE_MINUTES: i64 = 2;
/// The refresh's place among the changed customers (`phone_job_marks`): by
/// `updated_at` (`value`), then id (`last_id`).
const CHANGED_MARK: &str = "customer_phone_index.changed";
/// The refresh's place among the new customers: the highest id taken
/// (`last_id`).
const ADDED_MARK: &str = "customer_phone_index.added";

/// A customer as the index reads it.
#[derive(Debug, FromRow)]
struct Customer {
    id: i32,
    company_id: Option<i32>,
    phone: Option<String>,
    phone_2: Option<String>,
    /// Not deleted, and not the onboarding sample: has rows.
    live: bool,
}

/// The columns of a [`Customer`] (`c` the customer).
const CUSTOMER_COLUMNS: &str = "c.id, c.company_id, c.phone, c.phone_2,
        (c.deleted_at IS NULL AND c.id <> 990000021) AS live";

/// A customer the refresh read because it changed.
#[derive(Debug, FromRow)]
struct Changed {
    #[sqlx(flatten)]
    customer: Customer,
    updated_at: DateTime<Utc>,
}

/// A customer the refresh read because it is new.
#[derive(Debug, FromRow)]
struct Added {
    #[sqlx(flatten)]
    customer: Customer,
    created_date: Option<DateTime<Utc>>,
}

/// Whether the company's index is filled, so [`lookup`] may answer for it.
pub async fn built(pool: &MySqlPool, company_id: i32) -> sqlx::Result<bool> {
    let built: Option<bool> = sqlx::query_scalar(
        "SELECT built_at IS NOT NULL FROM customer_phone_index_builds WHERE company_id = ?",
    )
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    Ok(built == Some(true))
}

/// Each of `numbers` (last-10 strings) mapped to the newest live customer of
/// the company with that phone or phone 2, by the index.
///
/// A row the customer no longer matches (edited, deleted or moved since the
/// job last ran) is passed over.
pub async fn lookup(
    pool: &MySqlPool,
    company_id: i32,
    numbers: &[String],
) -> sqlx::Result<HashMap<String, i32>> {
    let mut found = HashMap::new();
    if numbers.is_empty() {
        return Ok(found);
    }
    let mut query = QueryBuilder::new(
        "SELECT i.last10, c.id, c.phone, c.phone_2
           FROM customer_phone_index i
           JOIN customers c ON c.id = i.customer_id AND c.company_id = i.company_id
          WHERE c.deleted_at IS NULL AND c.id <> 990000021 AND i.company_id = ",
    );
    query.push_bind(company_id).push(" AND i.last10 IN (");
    let mut list = query.separated(", ");
    for number in numbers {
        list.push_bind(number.clone());
    }
    query.push(") ORDER BY c.id DESC");
    let rows: Vec<(String, i32, Option<String>, Option<String>)> =
        query.build_query_as().fetch_all(pool).await?;
    for (digits, id, phone, phone_2) in rows {
        if index_phones(phone.as_deref(), phone_2.as_deref()).contains(&digits) {
            found.entry(digits).or_insert(id);
        }
    }
    Ok(found)
}

/// Inserts the rows of these customers: the last 10 digits of each live one's
/// phone and phone 2, under its company.
async fn insert_rows(conn: &mut MySqlConnection, customers: &[&Customer]) -> sqlx::Result<()> {
    let rows: Vec<(i32, String, i32)> = customers
        .iter()
        .filter(|customer| customer.live)
        .filter_map(|customer| Some((customer.company_id?, *customer)))
        .flat_map(|(company_id, customer)| {
            index_phones(customer.phone.as_deref(), customer.phone_2.as_deref())
                .into_iter()
                .map(move |digits| (company_id, digits, customer.id))
        })
        .collect();
    if rows.is_empty() {
        return Ok(());
    }
    let mut insert =
        QueryBuilder::new("INSERT INTO customer_phone_index (company_id, last10, customer_id) ");
    insert.push_values(rows, |mut row, (company_id, digits, customer_id)| {
        row.push_bind(company_id)
            .push_bind(digits)
            .push_bind(customer_id);
    });
    insert.build().execute(&mut *conn).await?;
    Ok(())
}

/// Rewrites these customers' rows: all their rows go, then the live ones'
/// current phones come back under the company they are in now.
async fn rewrite(conn: &mut MySqlConnection, customers: &[&Customer]) -> sqlx::Result<()> {
    if customers.is_empty() {
        return Ok(());
    }
    let mut delete = QueryBuilder::new("DELETE FROM customer_phone_index WHERE customer_id IN (");
    let mut ids = delete.separated(", ");
    for customer in customers {
        ids.push_bind(customer.id);
    }
    delete.push(")");
    delete.build().execute(&mut *conn).await?;
    insert_rows(conn, customers).await
}

/// Indexes a customer the app just created, with `phone` its one number, in
/// the caller's transaction: calls find them at once.
pub async fn index_new_customer(
    conn: &mut MySqlConnection,
    company_id: i32,
    customer_id: i32,
    phone: &str,
) -> sqlx::Result<()> {
    let customer = Customer {
        id: customer_id,
        company_id: Some(company_id),
        phone: Some(phone.to_string()),
        phone_2: None,
        live: true,
    };
    insert_rows(conn, &[&customer]).await
}

/// Rewrites one live customer's rows from their phone and phone 2 now, in
/// the caller's transaction: a number the app added is found at once.
pub async fn reindex_customer(
    conn: &mut MySqlConnection,
    company_id: i32,
    customer_id: i32,
    phone: Option<&str>,
    phone_2: Option<&str>,
) -> sqlx::Result<()> {
    let customer = Customer {
        id: customer_id,
        company_id: Some(company_id),
        phone: phone.map(str::to_string),
        phone_2: phone_2.map(str::to_string),
        live: true,
    };
    rewrite(conn, &[&customer]).await
}

/// The database's clock now (it stamps `updated_at` and `created_date`).
async fn db_now(conn: &mut MySqlConnection) -> sqlx::Result<DateTime<Utc>> {
    sqlx::query_scalar("SELECT UTC_TIMESTAMP()")
        .fetch_one(&mut *conn)
        .await
}

/// The refresh's mark `name` (time, id), locked until the transaction ends,
/// so an overlapping run waits here instead of doing the same work. Both
/// are `None` before the first run.
async fn claim_mark(
    conn: &mut MySqlConnection,
    name: &str,
) -> sqlx::Result<(Option<DateTime<Utc>>, Option<i64>)> {
    sqlx::query(
        "INSERT INTO phone_job_marks (name) VALUES (?) ON DUPLICATE KEY UPDATE name = name",
    )
    .bind(name)
    .execute(&mut *conn)
    .await?;
    sqlx::query_as("SELECT value, last_id FROM phone_job_marks WHERE name = ? FOR UPDATE")
        .bind(name)
        .fetch_one(&mut *conn)
        .await
}

/// Moves the refresh's mark `name` on.
async fn save_mark(
    conn: &mut MySqlConnection,
    name: &str,
    value: Option<DateTime<Utc>>,
    last_id: i64,
) -> sqlx::Result<()> {
    sqlx::query("UPDATE phone_job_marks SET value = ?, last_id = ? WHERE name = ?")
        .bind(value)
        .bind(last_id)
        .bind(name)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// The job's refresh: rewrites the rows of the customers changed or added
/// since the last run.
///
/// A batch at a time until `deadline`. The first run starts from now (the
/// first fills take what is there already). Returns how many customers were
/// rewritten (one read again because it changed lately counts again).
pub async fn refresh(pool: &MySqlPool, deadline: Instant) -> sqlx::Result<usize> {
    let mut rewritten = 0;
    loop {
        if Instant::now() >= deadline {
            return Ok(rewritten);
        }
        let (count, more) = refresh_changed(pool).await?;
        rewritten += count;
        if !more {
            break;
        }
    }
    loop {
        if Instant::now() >= deadline {
            return Ok(rewritten);
        }
        let (count, more) = refresh_added(pool).await?;
        rewritten += count;
        if !more {
            break;
        }
    }
    Ok(rewritten)
}

/// One batch of the customers changed since the mark, by (`updated_at`,
/// id): (how many were rewritten, whether more are waiting now).
async fn refresh_changed(pool: &MySqlPool) -> sqlx::Result<(usize, bool)> {
    let mut tx = pool.begin().await?;
    let (at, id) = claim_mark(&mut tx, CHANGED_MARK).await?;
    let settled = db_now(&mut tx).await? - Duration::minutes(SETTLE_MINUTES);
    let from = (at.unwrap_or(settled), id.unwrap_or(0));
    let changed: Vec<Changed> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {CUSTOMER_COLUMNS}, c.updated_at
           FROM customers c
          WHERE c.updated_at >= ? AND (c.updated_at > ? OR c.id > ?)
          ORDER BY c.updated_at, c.id
          LIMIT {REFRESH_BATCH}"
    )))
    .bind(from.0)
    .bind(from.0)
    .bind(from.1)
    .fetch_all(&mut *tx)
    .await?;
    let customers: Vec<&Customer> = changed.iter().map(|row| &row.customer).collect();
    rewrite(&mut tx, &customers).await?;
    // The next batch starts after the last customer read, but never past
    // `settled`: a change made since then may still be committing. Fewer
    // than a batch means everything was read.
    let last = changed
        .last()
        .map(|row| (row.updated_at, i64::from(row.customer.id)));
    let more = changed.len() == REFRESH_BATCH && last.is_some_and(|last| last < (settled, 0));
    let next = if more {
        last.unwrap_or((settled, 0))
    } else {
        (settled, 0)
    }
    .max(from);
    save_mark(&mut tx, CHANGED_MARK, Some(next.0), next.1).await?;
    tx.commit().await?;
    Ok((changed.len(), more))
}

/// One batch of the customers added since the mark, by id: (how many were
/// rewritten, whether more are waiting now).
async fn refresh_added(pool: &MySqlPool) -> sqlx::Result<(usize, bool)> {
    let mut tx = pool.begin().await?;
    let (_, after) = claim_mark(&mut tx, ADDED_MARK).await?;
    let now = db_now(&mut tx).await?;
    let settled = now - Duration::minutes(SETTLE_MINUTES);
    let Some(after) = after else {
        // The first run: new customers are the ones added from now on.
        let newest: i64 =
            sqlx::query_scalar("SELECT CAST(COALESCE(MAX(id), 0) AS SIGNED) FROM customers")
                .fetch_one(&mut *tx)
                .await?;
        save_mark(&mut tx, ADDED_MARK, None, newest).await?;
        tx.commit().await?;
        return Ok((0, false));
    };
    let added: Vec<Added> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {CUSTOMER_COLUMNS}, c.created_date
           FROM customers c
          WHERE c.id > ?
          ORDER BY c.id
          LIMIT {REFRESH_BATCH}"
    )))
    .bind(after)
    .fetch_all(&mut *tx)
    .await?;
    let customers: Vec<&Customer> = added.iter().map(|row| &row.customer).collect();
    rewrite(&mut tx, &customers).await?;
    // The next batch starts after the last customer read that was added
    // before `settled`: a lower id added since may still be committing, so
    // the newer ones are read again.
    let recent = |row: &&Added| {
        row.created_date
            .is_some_and(|created| (settled..=now).contains(&created))
    };
    let next = added
        .iter()
        .take_while(|row| !recent(row))
        .last()
        .map_or(after, |row| i64::from(row.customer.id));
    save_mark(&mut tx, ADDED_MARK, None, next).await?;
    tx.commit().await?;
    let more = added.len() == REFRESH_BATCH
        && added
            .last()
            .is_some_and(|row| i64::from(row.customer.id) == next);
    Ok((added.len(), more))
}

/// The job's first fills: fills the index of the companies that have none
/// yet.
///
/// A batch at a time, until `deadline` or [`FILL_PER_RUN`] customers;
/// deleted companies are left out. Returns how many customers were read
/// (with phones or not).
pub async fn fill(pool: &MySqlPool, deadline: Instant) -> sqlx::Result<usize> {
    let mut read = 0;
    while read < FILL_PER_RUN && Instant::now() < deadline {
        let next: Option<i32> = sqlx::query_scalar(
            "SELECT co.id FROM company co
               LEFT JOIN customer_phone_index_builds b ON b.company_id = co.id
              WHERE co.deleted_at IS NULL AND b.built_at IS NULL
              ORDER BY co.id
              LIMIT 1",
        )
        .fetch_optional(pool)
        .await?;
        let Some(company_id) = next else {
            break;
        };
        read += fill_batch(pool, company_id).await?;
    }
    Ok(read)
}

/// One batch of a company's first fill: its next customers by id after
/// where the fill got to; fewer than a batch finishes it. How many were
/// read.
async fn fill_batch(pool: &MySqlPool, company_id: i32) -> sqlx::Result<usize> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO customer_phone_index_builds (company_id) VALUES (?)
         ON DUPLICATE KEY UPDATE company_id = company_id",
    )
    .bind(company_id)
    .execute(&mut *tx)
    .await?;
    let (done_id, built): (i32, bool) = sqlx::query_as(
        "SELECT done_id, built_at IS NOT NULL FROM customer_phone_index_builds
          WHERE company_id = ? FOR UPDATE",
    )
    .bind(company_id)
    .fetch_one(&mut *tx)
    .await?;
    if built {
        // Another run finished it meanwhile.
        tx.commit().await?;
        return Ok(0);
    }
    let customers: Vec<Customer> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {CUSTOMER_COLUMNS}
           FROM customers c
          WHERE c.company_id = ? AND c.id > ?
          ORDER BY c.id
          LIMIT {FILL_BATCH}"
    )))
    .bind(company_id)
    .bind(done_id)
    .fetch_all(&mut *tx)
    .await?;
    let batch: Vec<&Customer> = customers.iter().collect();
    rewrite(&mut tx, &batch).await?;
    let done = customers.last().map_or(done_id, |customer| customer.id);
    sqlx::query(
        "UPDATE customer_phone_index_builds
            SET done_id = ?, built_at = IF(?, UTC_TIMESTAMP(), NULL)
          WHERE company_id = ?",
    )
    .bind(done)
    .bind(customers.len() < FILL_BATCH)
    .bind(company_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(customers.len())
}
