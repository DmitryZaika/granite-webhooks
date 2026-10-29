use crate::openapi::{IntoParams, ToSchema};
use crate::serde_helpers::js_date;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

/// Where an invoice stands.
///
/// - `draft`: saved, not emailed to the customer yet
/// - `sent`: emailed to the customer (or handed over), not paid
/// - `paid`: the customer paid it
/// - `void`: cancelled; kept so its number is not reused
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum InvoiceStatus {
    /// Saved, not sent yet.
    Draft,
    /// Emailed to the customer.
    Sent,
    /// Paid.
    Paid,
    /// Cancelled.
    Void,
}

impl InvoiceStatus {
    /// The stored value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Sent => "sent",
            Self::Paid => "paid",
            Self::Void => "void",
        }
    }

    /// A stored or requested value; `None` when unknown.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "draft" => Some(Self::Draft),
            "sent" => Some(Self::Sent),
            "paid" => Some(Self::Paid),
            "void" => Some(Self::Void),
            _ => None,
        }
    }
}

/// One line of an invoice, in the order it is printed.
///
/// A line with a `unit_price` is priced: `amount` = `qty` × `unit_price`,
/// rounded to cents (a negative price makes a discount). A line without a
/// `unit_price` is text only (a note under the line above) and has no `qty`
/// or `amount`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct InvoiceLine {
    /// What was sold, or the note's text.
    pub description: String,
    /// Quantity; 1 when a priced line leaves it out. Null on a text line.
    #[serde(default)]
    pub qty: Option<f64>,
    /// Price of one unit, in dollars. Null makes the line text only.
    #[serde(default)]
    pub unit_price: Option<f64>,
    /// `qty` × `unit_price` in dollars, rounded to cents. Computed by the
    /// API: a value sent in a request is ignored. Null on a text line.
    #[serde(default)]
    pub amount: Option<f64>,
}

/// Body of `POST /v1/invoices` and `PUT /v1/invoices/{id}`: the whole
/// invoice as the editor shows it. Totals are computed by the API.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct InvoiceInput {
    /// Invoice number, unique in the company. Leave it out on create to
    /// take the next one (`next_number` of `GET /v1/invoices`); on update,
    /// leaving it out keeps the current number.
    #[serde(default)]
    pub invoice_number: Option<i32>,
    /// The CRM customer billed (`GET /v1/customers`). Null bills someone
    /// who is not a customer record; with a `quote_id` and no customer, the
    /// quote's customer is used.
    #[serde(default)]
    pub customer_id: Option<i32>,
    /// The quote the invoice was made from, if any. It must be one of the
    /// company's quotes, and of `customer_id` when both are given.
    #[serde(default)]
    pub quote_id: Option<i32>,
    /// Invoice date, `YYYY-MM-DD`.
    pub invoice_date: String,
    /// Due date, `YYYY-MM-DD`, on or after `invoice_date`; null or empty for
    /// none (due on receipt).
    #[serde(default)]
    pub due_date: Option<String>,
    /// Name printed under "Bill to"; required.
    pub bill_to_name: String,
    /// Email the invoice is sent to; null or empty for none.
    #[serde(default)]
    pub bill_to_email: Option<String>,
    /// Job site address printed on the invoice, on one line.
    #[serde(default)]
    pub project_address: Option<String>,
    /// The lines, in order; at least one must have a price. Blank lines
    /// (no text, no price) are dropped. At most 200.
    pub lines: Vec<InvoiceLine>,
    /// Sales tax in dollars, added to the subtotal; 0 when left out.
    #[serde(default)]
    pub tax: Option<f64>,
    /// Message to the customer printed on the invoice.
    #[serde(default)]
    pub customer_memo: Option<String>,
    /// Internal note; never printed or sent.
    #[serde(default)]
    pub private_note: Option<String>,
}

/// Body of `PUT /v1/invoices/{id}/status`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct InvoiceStatusInput {
    /// The new status. `sent` records when it was first sent; `paid` records
    /// when it was paid (cleared again by any other status).
    pub status: InvoiceStatus,
}

/// Query string of `GET /v1/invoices`. Every filter is optional and they
/// combine.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct InvoiceListQuery {
    /// Only this customer's invoices.
    pub customer_id: Option<i32>,
    /// Only invoices made from this quote.
    pub quote_id: Option<i32>,
    /// Only invoices with this status.
    #[param(value_type = Option<InvoiceStatus>)]
    pub status: Option<String>,
    /// Matches the invoice number exactly, or part of the bill-to name,
    /// email or project address.
    pub search: Option<String>,
}

/// An invoice in a list.
#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct InvoiceSummary {
    /// Invoice id; `GET /v1/invoices/{id}` returns the whole invoice.
    pub id: i32,
    /// Invoice number, unique in the company.
    pub invoice_number: i32,
    /// `draft`, `sent`, `paid` or `void`.
    #[schema(value_type = InvoiceStatus)]
    pub status: String,
    /// The CRM customer billed; null when the invoice bills someone else.
    pub customer_id: Option<i32>,
    /// The quote it was made from; null for an invoice made by hand.
    pub quote_id: Option<i32>,
    /// Name under "Bill to".
    pub bill_to_name: String,
    /// Email it is sent to; null when none.
    pub bill_to_email: Option<String>,
    /// Job site address; null when none.
    pub project_address: Option<String>,
    /// Invoice date, `YYYY-MM-DD`.
    pub invoice_date: String,
    /// Due date, `YYYY-MM-DD`; null when due on receipt.
    pub due_date: Option<String>,
    /// Total with tax, in dollars.
    pub total: f64,
    /// When it was first sent (ISO 8601, UTC); null while a draft.
    #[serde(serialize_with = "js_date")]
    pub sent_at: Option<DateTime<Utc>>,
    /// When it was created (ISO 8601, UTC).
    #[serde(serialize_with = "js_date")]
    pub created_at: Option<DateTime<Utc>>,
}

/// `GET /v1/invoices`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct InvoiceList {
    /// Matching invoices, highest number first; at most 500.
    pub invoices: Vec<InvoiceSummary>,
    /// The number the next new invoice gets when none is given.
    pub next_number: i32,
}

/// An invoice row as stored; `Invoice` is what the API returns.
#[derive(Debug, Clone, FromRow)]
pub struct InvoiceRow {
    pub id: i32,
    pub invoice_number: i32,
    pub status: String,
    pub customer_id: Option<i32>,
    pub customer_name: Option<String>,
    pub quote_id: Option<i32>,
    pub quote_name: Option<String>,
    pub invoice_date: String,
    pub due_date: Option<String>,
    pub bill_to_name: String,
    pub bill_to_email: Option<String>,
    pub project_address: Option<String>,
    pub line_items: Option<String>,
    pub subtotal: f64,
    pub tax: f64,
    pub total: f64,
    pub customer_memo: Option<String>,
    pub private_note: Option<String>,
    pub sent_at: Option<DateTime<Utc>>,
    pub paid_at: Option<DateTime<Utc>>,
    pub created_by_name: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
}

/// A whole invoice.
#[derive(Debug, Clone, Serialize, ToSchema)]
// Billing's Stripe `Invoice` already has that schema name.
#[schema(as = CrmInvoice)]
pub struct Invoice {
    /// Invoice id.
    pub id: i32,
    /// Invoice number, unique in the company.
    pub invoice_number: i32,
    /// `draft`, `sent`, `paid` or `void`.
    #[schema(value_type = InvoiceStatus)]
    pub status: String,
    /// The CRM customer billed; null when the invoice bills someone else.
    pub customer_id: Option<i32>,
    /// That customer's current name in the CRM; null without a customer.
    pub customer_name: Option<String>,
    /// The quote it was made from; null for an invoice made by hand.
    pub quote_id: Option<i32>,
    /// That quote's name; null without a quote.
    pub quote_name: Option<String>,
    /// Invoice date, `YYYY-MM-DD`.
    pub invoice_date: String,
    /// Due date, `YYYY-MM-DD`; null when due on receipt.
    pub due_date: Option<String>,
    /// Name under "Bill to".
    pub bill_to_name: String,
    /// Email it is sent to; null when none.
    pub bill_to_email: Option<String>,
    /// Job site address; null when none.
    pub project_address: Option<String>,
    /// The lines, in order.
    pub lines: Vec<InvoiceLine>,
    /// Sum of the line amounts, in dollars.
    pub subtotal: f64,
    /// Sales tax, in dollars.
    pub tax: f64,
    /// `subtotal` + `tax`, in dollars.
    pub total: f64,
    /// Message to the customer printed on the invoice; null when none.
    pub customer_memo: Option<String>,
    /// Internal note, never printed; null when none.
    pub private_note: Option<String>,
    /// When it was first sent (ISO 8601, UTC); null while a draft.
    #[serde(serialize_with = "js_date")]
    pub sent_at: Option<DateTime<Utc>>,
    /// When it was marked paid (ISO 8601, UTC); null unless `paid`.
    #[serde(serialize_with = "js_date")]
    pub paid_at: Option<DateTime<Utc>>,
    /// Name of the user who created it; null when unknown.
    pub created_by_name: Option<String>,
    /// When it was created (ISO 8601, UTC).
    #[serde(serialize_with = "js_date")]
    pub created_at: Option<DateTime<Utc>>,
    /// When it last changed (ISO 8601, UTC).
    #[serde(serialize_with = "js_date")]
    pub updated_at: Option<DateTime<Utc>>,
}

impl From<InvoiceRow> for Invoice {
    fn from(row: InvoiceRow) -> Self {
        let lines = row
            .line_items
            .as_deref()
            .and_then(|json| serde_json::from_str(json).ok())
            .unwrap_or_default();
        Self {
            id: row.id,
            invoice_number: row.invoice_number,
            status: row.status,
            customer_id: row.customer_id,
            customer_name: row.customer_name,
            quote_id: row.quote_id,
            quote_name: row.quote_name,
            invoice_date: row.invoice_date,
            due_date: row.due_date,
            bill_to_name: row.bill_to_name,
            bill_to_email: row.bill_to_email,
            project_address: row.project_address,
            lines,
            subtotal: row.subtotal,
            tax: row.tax,
            total: row.total,
            customer_memo: row.customer_memo,
            private_note: row.private_note,
            sent_at: row.sent_at,
            paid_at: row.paid_at,
            created_by_name: row.created_by_name,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}
