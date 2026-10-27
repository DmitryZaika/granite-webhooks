use super::schemas::{InvoiceListQuery, InvoiceRow, InvoiceStatus, InvoiceSummary};
use super::validate::CleanInvoice;
use sqlx::MySqlPool;

/// Most invoices one list returns.
pub const LIST_LIMIT: i64 = 500;

const INVOICE_SELECT: &str = "SELECT i.id, i.invoice_number, i.status, i.customer_id,
       c.name AS customer_name, i.quote_id, q.quote_name,
       DATE_FORMAT(i.invoice_date, '%Y-%m-%d') AS invoice_date,
       DATE_FORMAT(i.due_date, '%Y-%m-%d') AS due_date,
       i.bill_to_name, i.bill_to_email, i.project_address,
       CAST(i.line_items AS CHAR) AS line_items,
       CAST(i.subtotal AS DOUBLE) AS subtotal, CAST(i.tax AS DOUBLE) AS tax,
       CAST(i.total AS DOUBLE) AS total, i.customer_memo, i.private_note,
       i.sent_at, i.paid_at, u.name AS created_by_name, i.created_at, i.updated_at
  FROM invoices i
  LEFT JOIN customers c ON c.id = i.customer_id AND c.company_id = i.company_id
  LEFT JOIN quotes q ON q.id = i.quote_id AND q.company_id = i.company_id
  LEFT JOIN users u ON u.id = i.created_by";

/// One of the company's invoices.
pub async fn find_invoice(
    pool: &MySqlPool,
    company_id: i32,
    id: i32,
) -> Result<Option<InvoiceRow>, sqlx::Error> {
    sqlx::query_as::<_, InvoiceRow>(&format!(
        "{INVOICE_SELECT} WHERE i.company_id = ? AND i.id = ?"
    ))
    .bind(company_id)
    .bind(id)
    .fetch_optional(pool)
    .await
}

/// The company's invoices matching `filter`, highest number first.
pub async fn list_invoices(
    pool: &MySqlPool,
    company_id: i32,
    filter: &InvoiceListQuery,
    status: Option<InvoiceStatus>,
) -> Result<Vec<InvoiceSummary>, sqlx::Error> {
    let search = filter
        .search
        .as_deref()
        .map(str::trim)
        .filter(|search| !search.is_empty());
    let number = search.and_then(|search| search.trim_start_matches('#').parse::<i32>().ok());
    let like = search.map(|search| {
        format!(
            "%{}%",
            search
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        )
    });
    sqlx::query_as::<_, InvoiceSummary>(
        "SELECT i.id, i.invoice_number, i.status, i.customer_id, i.quote_id,
                i.bill_to_name, i.bill_to_email, i.project_address,
                DATE_FORMAT(i.invoice_date, '%Y-%m-%d') AS invoice_date,
                DATE_FORMAT(i.due_date, '%Y-%m-%d') AS due_date,
                CAST(i.total AS DOUBLE) AS total, i.sent_at, i.created_at
           FROM invoices i
          WHERE i.company_id = ?
            AND (? IS NULL OR i.customer_id = ?)
            AND (? IS NULL OR i.quote_id = ?)
            AND (? IS NULL OR i.status = ?)
            AND (? IS NULL
                 OR i.invoice_number = ?
                 OR i.bill_to_name LIKE ?
                 OR i.bill_to_email LIKE ?
                 OR i.project_address LIKE ?)
          ORDER BY i.invoice_number DESC
          LIMIT ?",
    )
    .bind(company_id)
    .bind(filter.customer_id)
    .bind(filter.customer_id)
    .bind(filter.quote_id)
    .bind(filter.quote_id)
    .bind(status.map(InvoiceStatus::as_str))
    .bind(status.map(InvoiceStatus::as_str))
    .bind(like.as_deref())
    .bind(number)
    .bind(like.as_deref())
    .bind(like.as_deref())
    .bind(like.as_deref())
    .bind(LIST_LIMIT)
    .fetch_all(pool)
    .await
}

/// The number the company's next invoice gets.
pub async fn next_number(pool: &MySqlPool, company_id: i32) -> Result<i32, sqlx::Error> {
    let next = sqlx::query_scalar::<_, i64>(
        "SELECT CAST(COALESCE(MAX(invoice_number), 0) + 1 AS SIGNED)
           FROM invoices WHERE company_id = ?",
    )
    .bind(company_id)
    .fetch_one(pool)
    .await?;
    i32::try_from(next).map_err(|err| sqlx::Error::Decode(Box::new(err)))
}

/// The customer of one of the company's quotes (`Some(None)` for a quote
/// without one); `None` when the quote is not the company's.
pub async fn quote_customer(
    pool: &MySqlPool,
    company_id: i32,
    quote_id: i32,
) -> Result<Option<Option<i32>>, sqlx::Error> {
    sqlx::query_scalar::<_, Option<i32>>(
        "SELECT customer_id FROM quotes
          WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(quote_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await
}

/// Whether the customer is one of the company's.
pub async fn customer_exists(
    pool: &MySqlPool,
    company_id: i32,
    customer_id: i32,
) -> Result<bool, sqlx::Error> {
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT id FROM customers WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(customer_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    Ok(found.is_some())
}

fn lines_json(invoice: &CleanInvoice) -> String {
    serde_json::to_string(&invoice.lines).unwrap_or_else(|_| "[]".into())
}

/// Stores a new invoice with `number`; its id.
pub async fn insert_invoice(
    pool: &MySqlPool,
    company_id: i32,
    user_id: i32,
    number: i32,
    invoice: &CleanInvoice,
) -> Result<i32, sqlx::Error> {
    let result = sqlx::query(
        "INSERT INTO invoices (company_id, invoice_number, customer_id, quote_id, status,
            invoice_date, due_date, bill_to_name, bill_to_email, project_address,
            line_items, subtotal, tax, total, customer_memo, private_note, created_by)
         VALUES (?, ?, ?, ?, 'draft', ?, ?, ?, ?, ?, CAST(? AS JSON), ?, ?, ?, ?, ?, ?)",
    )
    .bind(company_id)
    .bind(number)
    .bind(invoice.customer_id)
    .bind(invoice.quote_id)
    .bind(invoice.invoice_date)
    .bind(invoice.due_date)
    .bind(&invoice.bill_to_name)
    .bind(&invoice.bill_to_email)
    .bind(&invoice.project_address)
    .bind(lines_json(invoice))
    .bind(invoice.subtotal)
    .bind(invoice.tax)
    .bind(invoice.total)
    .bind(&invoice.customer_memo)
    .bind(&invoice.private_note)
    .bind(user_id)
    .execute(pool)
    .await?;
    i32::try_from(result.last_insert_id()).map_err(|err| sqlx::Error::Decode(Box::new(err)))
}

/// Replaces the editable fields of one of the company's invoices (its
/// status is kept); false when there is no such invoice.
pub async fn update_invoice(
    pool: &MySqlPool,
    company_id: i32,
    id: i32,
    number: i32,
    invoice: &CleanInvoice,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE invoices
            SET invoice_number = ?, customer_id = ?, quote_id = ?, invoice_date = ?,
                due_date = ?, bill_to_name = ?, bill_to_email = ?, project_address = ?,
                line_items = CAST(? AS JSON), subtotal = ?, tax = ?, total = ?,
                customer_memo = ?, private_note = ?
          WHERE id = ? AND company_id = ?",
    )
    .bind(number)
    .bind(invoice.customer_id)
    .bind(invoice.quote_id)
    .bind(invoice.invoice_date)
    .bind(invoice.due_date)
    .bind(&invoice.bill_to_name)
    .bind(&invoice.bill_to_email)
    .bind(&invoice.project_address)
    .bind(lines_json(invoice))
    .bind(invoice.subtotal)
    .bind(invoice.tax)
    .bind(invoice.total)
    .bind(&invoice.customer_memo)
    .bind(&invoice.private_note)
    .bind(id)
    .bind(company_id)
    .execute(pool)
    .await?;
    // MySQL counts only changed rows: an unchanged save is still a match.
    Ok(result.rows_affected() > 0 || find_invoice(pool, company_id, id).await?.is_some())
}

/// Sets the status of one of the company's invoices; false when there is
/// no such invoice. `sent` keeps the first sending time; `paid` records
/// when, and any other status clears it.
pub async fn set_status(
    pool: &MySqlPool,
    company_id: i32,
    id: i32,
    status: InvoiceStatus,
) -> Result<bool, sqlx::Error> {
    let sent = matches!(status, InvoiceStatus::Sent | InvoiceStatus::Paid);
    let paid = status == InvoiceStatus::Paid;
    let result = sqlx::query(
        "UPDATE invoices
            SET status = ?,
                sent_at = IF(?, COALESCE(sent_at, CURRENT_TIMESTAMP), sent_at),
                paid_at = IF(?, COALESCE(paid_at, CURRENT_TIMESTAMP), NULL)
          WHERE id = ? AND company_id = ?",
    )
    .bind(status.as_str())
    .bind(sent)
    .bind(paid)
    .bind(id)
    .bind(company_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0 || find_invoice(pool, company_id, id).await?.is_some())
}
