//! `QuickBooks` Online invoices and customers.
//!
//! - `GET /v1/customers/{id}/qbo-invoices` → the customer's invoices,
//!   linking the customer to its `QuickBooks` customer on first use
//! - `POST /v1/customers/{id}/qbo-invoices` → a new invoice (a quote's
//!   Create invoice)
//! - `GET`/`PUT /v1/customers/{id}/qbo-invoices/{invoice_id}` → one
//!   invoice for the editor, and saving it
//! - `GET /v1/customers/{id}/qbo-invoices/{invoice_id}/pdf` → its PDF
//! - `GET /v1/qbo/invoices/{invoice_id}/pdf` → any invoice of the company
//!   as a PDF (the rep's sales panel, schedule events)
//! - `GET /v1/qbo/customers` → `QuickBooks` customers by email or name
//! - `GET /v1/qbo/items` → the products and services a line can sell
//! - `GET /v1/qbo/class-sales` → the "Sales by Class" report (admins)
//! - `GET /v1/me/qbo-sales` → the rep's sales this month, and
//!   `/customers` the customers behind them
//!
//! Replaces the Remix `api.quickbooks.*` and `api.employee.qbo-*` routes.

pub mod class_sales;
pub mod instructions;
pub mod invoice;
pub mod products;
pub mod rules;
pub mod sales;

use crate::auth::session::SessionUser;
use crate::auth::{AdminUser, EmployeeUser};
use crate::entitlements::AddonKey;
use crate::error::{AddonRequiredBody, ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::{IntoParams, ToSchema};
use crate::openapi::{OpenApiRouter, routes};
use crate::qbo::{Access, QboError, escape, valid_id};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::IntoResponse;
use invoice::{
    Address, EditableInvoice, InvoiceSummary, Line, create_payload, editable, summary, text,
    update_payload,
};
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(customer_invoices, create_invoice))
        .routes(routes!(get_invoice, update_invoice))
        .routes(routes!(customer_invoice_pdf))
        .routes(routes!(invoice_pdf))
        .routes(routes!(customers))
        .routes(routes!(items))
        .routes(routes!(recent_invoices))
        .routes(routes!(rules::invoice_draft))
        .routes(routes!(
            instructions::get_instructions,
            instructions::save_instructions
        ))
        .routes(routes!(sales::my_sales))
        .routes(routes!(sales::my_customers))
        .routes(routes!(class_sales::class_sales))
}

/// `positions.id` of an installer.
const INSTALLER: i32 = 6;

/// An installer of the company who is not an admin: they are locked into the
/// installer portal and see only the invoice the office linked to their job.
async fn installer_only(state: &AppState, user: &SessionUser) -> ApiResult<bool> {
    if user.is_admin || user.is_superuser {
        return Ok(false);
    }
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM users_positions WHERE user_id = ? AND company_id = ? AND position_id = ? LIMIT 1",
    )
    .bind(user.id)
    .bind(user.company_id)
    .bind(INSTALLER)
    .fetch_optional(&state.pool)
    .await?;
    Ok(found.is_some())
}

/// Installers may not browse, create or edit invoices.
async fn refuse_installer(state: &AppState, user: &SessionUser) -> ApiResult<()> {
    if installer_only(state, user).await? {
        return Err(ApiError::Denied(
            "Installers see only the invoice linked to their job.".into(),
        ));
    }
    Ok(())
}

/// What a `QuickBooks` failure means to the page.
#[allow(clippy::needless_pass_by_value)] // used as `map_err(qbo_error)`
fn qbo_error(err: QboError) -> ApiError {
    if matches!(err, QboError::AddonRequired) {
        return ApiError::AddonRequired(AddonKey::Quickbooks);
    }
    if err.is_not_connected() {
        return ApiError::Conflict("QuickBooks is not connected".into());
    }
    tracing::error!(error = %err, "QuickBooks request failed");
    ApiError::Upstream("QuickBooks request failed".into())
}

/// A write `QuickBooks` refused (a taken name, an inactive item) says why.
#[allow(clippy::needless_pass_by_value)] // used as `map_err(qbo_refusal)`
fn qbo_refusal(err: QboError) -> ApiError {
    if let QboError::Status { status: 400, body } = &err
        && let Ok(body) = serde_json::from_str::<Value>(body)
        && let Some(detail) = text(&body["Fault"]["Error"][0]["Detail"])
            .or_else(|| text(&body["Fault"]["Error"][0]["Message"]))
    {
        return ApiError::BadRequest(format!("QuickBooks: {detail}"));
    }
    qbo_error(err)
}

/// `ok`, or why there are no invoices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum InvoicesStatus {
    Ok,
    /// The company has not connected `QuickBooks`.
    NotConnected,
    /// No `QuickBooks` customer matches this contact.
    MissingQboId,
    /// `QuickBooks` failed.
    Error,
}

/// `GET /v1/customers/{id}/qbo-invoices`.
#[derive(Debug, Serialize, ToSchema)]
pub struct CustomerInvoices {
    pub status: InvoicesStatus,
    /// Newest first.
    pub invoices: Vec<InvoiceSummary>,
    /// The `QuickBooks` customer id.
    pub qbo_id: Option<String>,
}

impl CustomerInvoices {
    const fn empty(status: InvoicesStatus) -> Self {
        Self {
            status,
            invoices: Vec::new(),
            qbo_id: None,
        }
    }
}

#[derive(sqlx::FromRow)]
struct CustomerRow {
    name: Option<String>,
    qbo_id: Option<String>,
    email: Option<String>,
}

async fn customer(state: &AppState, company_id: i32, customer_id: i32) -> ApiResult<CustomerRow> {
    let row: Option<CustomerRow> = sqlx::query_as(
        "SELECT c.name, NULLIF(TRIM(CAST(c.qbo_id AS CHAR)), '') AS qbo_id, ce.email
           FROM customers c
           LEFT JOIN customers_emails ce ON ce.id = c.email_id AND ce.customer_id = c.id
          WHERE c.id = ? AND c.company_id = ? AND c.deleted_at IS NULL",
    )
    .bind(customer_id)
    .bind(company_id)
    .fetch_optional(&state.pool)
    .await?;
    row.ok_or(ApiError::NotFound)
}

/// A `QuickBooks` customer.
#[derive(Debug, Serialize, ToSchema)]
pub struct QboCustomer {
    /// `QuickBooks` customer id.
    pub id: String,
    /// Display name.
    pub display_name: String,
    /// Primary email.
    pub email: Option<String>,
}

fn qbo_customer(row: &Value) -> Option<QboCustomer> {
    let id = text(&row["Id"])?;
    Some(QboCustomer {
        display_name: text(&row["DisplayName"])
            .unwrap_or_else(|| format!("QuickBooks customer {id}")),
        email: text(&row["PrimaryEmailAddr"]["Address"]),
        id,
    })
}

async fn by_email(state: &AppState, access: &Access, email: &str) -> Result<Vec<Value>, QboError> {
    state
        .qbo
        .query(
            access,
            &format!(
                "select * from Customer WHERE PrimaryEmailAddr = '{}'",
                escape(email)
            ),
            "Customer",
        )
        .await
}

async fn by_name(
    state: &AppState,
    access: &Access,
    name: &str,
) -> Result<Vec<QboCustomer>, QboError> {
    let rows = state
        .qbo
        .query(
            access,
            &format!(
                "select Id, DisplayName, PrimaryEmailAddr from Customer where DisplayName like '%{}%' maxresults 25",
                escape(name.trim())
            ),
            "Customer",
        )
        .await?;
    Ok(rows.iter().filter_map(qbo_customer).collect())
}

/// The CRM's `pickQboCustomerByName`: an exact name, then a unique email
/// match, then the only result.
fn pick_by_name(matches: &[QboCustomer], name: &str, email: &str) -> Option<String> {
    let key = |value: &str| value.trim().to_lowercase();
    let (name, email) = (key(name), key(email));
    let with_email = |list: Vec<&QboCustomer>| -> Option<String> {
        let found: Vec<_> = list
            .into_iter()
            .filter(|found| found.email.as_deref().is_some_and(|e| key(e) == email))
            .collect();
        (found.len() == 1).then(|| found[0].id.clone())
    };
    let exact: Vec<&QboCustomer> = matches
        .iter()
        .filter(|found| key(&found.display_name) == name)
        .collect();
    if exact.len() == 1 {
        return Some(exact[0].id.clone());
    }
    if !email.is_empty() {
        if exact.len() > 1
            && let Some(id) = with_email(exact)
        {
            return Some(id);
        }
        if let Some(id) = with_email(matches.iter().collect()) {
            return Some(id);
        }
    }
    (matches.len() == 1).then(|| matches[0].id.clone())
}

async fn resolve_qbo_id(
    state: &AppState,
    access: &Access,
    row: &CustomerRow,
) -> Result<Option<String>, QboError> {
    let email = row.email.as_deref().unwrap_or("").trim();
    let name = row.name.as_deref().unwrap_or("").trim();
    if !email.is_empty() {
        let matches = by_email(state, access, email).await?;
        if let [only] = matches.as_slice()
            && let Some(id) = text(&only["Id"])
        {
            return Ok(Some(id.trim().to_string()));
        }
    }
    if name.is_empty() {
        return Ok(None);
    }
    Ok(pick_by_name(
        &by_name(state, access, name).await?,
        name,
        email,
    ))
}

async fn invoices_for(
    state: &AppState,
    access: &Access,
    qbo_id: &str,
) -> Result<Vec<InvoiceSummary>, QboError> {
    let rows = state
        .qbo
        .query(
            access,
            &format!(
                "select Id, DocNumber, TxnDate, DueDate, TotalAmt, Balance, CurrencyRef, ShipAddr, MetaData from Invoice where CustomerRef = '{}' MAXRESULTS 1000",
                escape(qbo_id)
            ),
            "Invoice",
        )
        .await?;
    let mut invoices: Vec<InvoiceSummary> = rows.iter().filter_map(summary).collect();
    invoices.sort_by(|a, b| b.txn_date.cmp(&a.txn_date));
    Ok(invoices)
}

/// List a customer's QuickBooks invoices
///
/// The invoices of the customer's `QuickBooks` customer, newest first. A
/// customer not linked yet is matched by email, then by name, and linked.
/// `status` says why there are none when `QuickBooks` is not connected or
/// has no matching customer.
#[crate::openapi::path(
    get,
    path = "/v1/customers/{id}/qbo-invoices",
    operation_id = "list_customer_qbo_invoices",
    tag = "quickbooks",
    params(("id" = i32, Path, description = "Customer id")),
    responses(
        (status = 200, description = "Invoices", body = CustomerInvoices),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin, or the plan lacks the QuickBooks add-on (`addon`: `quickbooks`)", body = AddonRequiredBody),
        (status = 404, description = "No such customer in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn customer_invoices(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(customer_id): Path<i32>,
) -> ApiResult<Json<CustomerInvoices>> {
    user.plan.require(AddonKey::Quickbooks)?;
    refuse_installer(&state, &user).await?;
    invoices_of_customer(&state, user.company_id, customer_id)
        .await
        .map(Json)
}

/// A customer's `QuickBooks` invoices, linking the customer first when it
/// is not linked yet (also for the customer's own view).
pub async fn invoices_of_customer(
    state: &AppState,
    company_id: i32,
    customer_id: i32,
) -> ApiResult<CustomerInvoices> {
    let row = customer(state, company_id, customer_id).await?;
    let result = async {
        let access = state.qbo.access(&state.pool, company_id).await?;
        let qbo_id = if let Some(id) = row.qbo_id.clone() {
            id
        } else {
            let Some(id) = resolve_qbo_id(state, &access, &row).await? else {
                return Ok(CustomerInvoices::empty(InvoicesStatus::MissingQboId));
            };
            sqlx::query("UPDATE customers SET qbo_id = ? WHERE id = ? AND company_id = ? AND deleted_at IS NULL")
                .bind(&id)
                .bind(customer_id)
                .bind(company_id)
                .execute(&state.pool)
                .await?;
            id
        };
        Ok::<_, QboError>(CustomerInvoices {
            status: InvoicesStatus::Ok,
            invoices: invoices_for(state, &access, &qbo_id).await?,
            qbo_id: Some(qbo_id),
        })
    }
    .await;
    Ok(match result {
        Ok(found) => found,
        Err(err) if err.is_not_connected() => CustomerInvoices::empty(InvoicesStatus::NotConnected),
        // Public and commission callers have no session; they see no invoices.
        Err(QboError::AddonRequired) => CustomerInvoices::empty(InvoicesStatus::NotConnected),
        Err(QboError::Database(err)) => return Err(err.into()),
        Err(err) => {
            tracing::error!(error = %err, customer_id, "QuickBooks invoices failed");
            CustomerInvoices::empty(InvoicesStatus::Error)
        }
    })
}

/// A `QuickBooks` display name: no `:` (it nests customers), tabs or new lines.
fn display_name(name: &str) -> String {
    name.split([':', '\t', '\n', '\r', ' '])
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(500)
        .collect()
}

/// The customer's `QuickBooks` customer: the linked one, a match, or a new
/// one made from the CRM customer. Links it.
async fn qbo_customer_for(
    state: &AppState,
    access: &Access,
    company_id: i32,
    customer_id: i32,
    row: &CustomerRow,
    bill_addr: Option<&Address>,
) -> ApiResult<String> {
    if let Some(id) = row.qbo_id.clone() {
        return Ok(id);
    }
    let id = if let Some(id) = resolve_qbo_id(state, access, row)
        .await
        .map_err(qbo_error)?
    {
        id
    } else {
        let name = display_name(row.name.as_deref().unwrap_or(""));
        if name.is_empty() {
            return Err(ApiError::BadRequest(
                "The customer needs a name for QuickBooks".into(),
            ));
        }
        let mut body = json!({ "DisplayName": name });
        if let Some(email) = row
            .email
            .as_deref()
            .map(str::trim)
            .filter(|e| !e.is_empty())
        {
            body["PrimaryEmailAddr"] = json!({ "Address": email });
        }
        if let Some(address) = bill_addr.and_then(Address::to_qbo) {
            body["BillAddr"] = address;
        }
        let created = state
            .qbo
            .post(access, "customer", &body)
            .await
            .map_err(qbo_refusal)?;
        text(&created["Customer"]["Id"])
            .ok_or_else(|| ApiError::Upstream("Failed to parse created customer".into()))?
    };
    sqlx::query(
        "UPDATE customers SET qbo_id = ? WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(&id)
    .bind(customer_id)
    .bind(company_id)
    .execute(&state.pool)
    .await?;
    Ok(id)
}

/// Body of `POST /v1/customers/{id}/qbo-invoices`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct InvoiceCreate {
    /// The invoice as the editor left it (`id` and `sync_token` unused).
    pub invoice: EditableInvoice,
    /// The quote it is made from, which keeps the new invoice's id.
    pub quote_id: Option<i32>,
    /// The lines the editor opened with: what the rep changed in them is
    /// kept for the next drafts to learn from.
    pub draft: Option<Vec<Line>>,
}

/// Create a QuickBooks invoice for a customer
///
/// Creates the editor's invoice in `QuickBooks` for the customer, matching
/// or creating the customer's `QuickBooks` customer first. With `quote_id`
/// the quote keeps the invoice's id, and a quote that has one already is
/// refused.
#[crate::openapi::path(
    post,
    path = "/v1/customers/{id}/qbo-invoices",
    operation_id = "create_customer_qbo_invoice",
    tag = "quickbooks",
    params(("id" = i32, Path, description = "Customer id")),
    request_body = InvoiceCreate,
    responses(
        (status = 200, description = "The created invoice", body = EditableInvoice),
        (status = 400, description = "No sales line, a sales line without an item, or QuickBooks refused it", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such customer, or no such quote of the customer", body = ErrorBody),
        (status = 409, description = "QuickBooks is not connected", body = ErrorBody),
        (status = 502, description = "QuickBooks failed", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create_invoice(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(customer_id): Path<i32>,
    JsonBody(input): JsonBody<InvoiceCreate>,
) -> ApiResult<Json<EditableInvoice>> {
    refuse_installer(&state, &user).await?;
    let company_id = user.company_id;
    if let Some(quote_id) = input.quote_id {
        // A quote may be invoiced again (e.g. its invoice was deleted in
        // QuickBooks); it keeps the newest invoice's id.
        let quote: Option<i32> = sqlx::query_scalar(
            "SELECT id FROM quotes
              WHERE id = ? AND company_id = ? AND customer_id = ? AND deleted_at IS NULL",
        )
        .bind(quote_id)
        .bind(company_id)
        .bind(customer_id)
        .fetch_optional(&state.pool)
        .await?;
        if quote.is_none() {
            return Err(ApiError::Denied(
                "This quote is not one of this customer's.".into(),
            ));
        }
    }
    let row = customer(&state, company_id, customer_id).await?;
    let access = state
        .qbo
        .access(&state.pool, company_id)
        .await
        .map_err(qbo_error)?;
    let qbo_id = qbo_customer_for(
        &state,
        &access,
        company_id,
        customer_id,
        &row,
        input.invoice.bill_addr.as_ref(),
    )
    .await?;
    let class = match input
        .invoice
        .class_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        Some(name) => Some(class_id(&state, &access, name).await?),
        None => None,
    };
    let class_per_line = class.is_some() && class_per_line(&state, &access).await;
    let payload = create_payload(
        &qbo_id,
        &input.invoice,
        row.email.as_deref(),
        class.as_deref(),
        class_per_line,
    )
    .map_err(ApiError::BadRequest)?;
    let created = state
        .qbo
        .post(&access, "invoice", &payload)
        .await
        .map_err(qbo_refusal)?;
    let created = if created["Invoice"].is_object() {
        &created["Invoice"]
    } else {
        &created
    };
    let invoice = editable(created)
        .ok_or_else(|| ApiError::Upstream("Failed to parse created invoice".into()))?;
    if let Some(quote_id) = input.quote_id {
        sqlx::query("UPDATE quotes SET qbo_invoice_id = ? WHERE id = ? AND company_id = ?")
            .bind(&invoice.id)
            .bind(quote_id)
            .bind(company_id)
            .execute(&state.pool)
            .await?;
    }
    if let Some(draft) = &input.draft
        && let Err(err) = rules::record(
            &state.pool,
            &user,
            input.quote_id,
            &invoice.id,
            draft,
            &input.invoice,
        )
        .await
    {
        // The invoice is made; only the lesson is lost.
        tracing::error!(error = %err, "invoice draft outcomes not kept");
    }
    Ok(Json(invoice))
}

/// Whether the company sets classes on each line rather than on the invoice.
async fn class_per_line(state: &AppState, access: &Access) -> bool {
    state
        .qbo
        .get(access, "preferences")
        .await
        .ok()
        .and_then(|prefs| {
            prefs["Preferences"]["AccountingInfoPrefs"]["ClassTrackingPerTxnLine"].as_bool()
        })
        .unwrap_or(false)
}

/// A `QuickBooks` class's id by its full name.
async fn class_id(state: &AppState, access: &Access, name: &str) -> ApiResult<String> {
    let rows = state
        .qbo
        .query(
            access,
            &format!(
                "select Id from Class where FullyQualifiedName = '{}'",
                escape(name)
            ),
            "Class",
        )
        .await
        .map_err(qbo_error)?;
    rows.first()
        .and_then(|row| text(&row["Id"]))
        .ok_or_else(|| ApiError::BadRequest(format!("QuickBooks has no class named \"{name}\"")))
}

/// The access token, once the invoice is known to be the customer's: billed
/// to their QuickBooks customer, or made from one of their quotes (it stays
/// theirs when it is moved to another QuickBooks customer, e.g. a contractor).
async fn owned_invoice(
    state: &AppState,
    company_id: i32,
    customer_id: i32,
    invoice_id: &str,
) -> ApiResult<Access> {
    if !valid_id(invoice_id) {
        return Err(ApiError::BadRequest("Invalid invoice".into()));
    }
    let expected = customer(state, company_id, customer_id)
        .await?
        .qbo_id
        .ok_or(ApiError::NotFound)?;
    let access = state
        .qbo
        .access(&state.pool, company_id)
        .await
        .map_err(qbo_error)?;
    let rows = state
        .qbo
        .query(
            &access,
            &format!(
                "select Id, CustomerRef from Invoice where Id = '{}'",
                escape(invoice_id)
            ),
            "Invoice",
        )
        .await
        .map_err(qbo_error)?;
    let owner = rows
        .first()
        .and_then(|row| text(&row["CustomerRef"]["value"]));
    if owner.is_none() {
        return Err(ApiError::Conflict(format!(
            "QuickBooks has no invoice {invoice_id}: it was deleted there."
        )));
    }
    if owner.as_deref() != Some(expected.as_str()) {
        let from_quote: Option<i32> = sqlx::query_scalar(
            "SELECT id FROM quotes
              WHERE company_id = ? AND customer_id = ? AND qbo_invoice_id = ? AND deleted_at IS NULL
              LIMIT 1",
        )
        .bind(company_id)
        .bind(customer_id)
        .bind(invoice_id)
        .fetch_optional(&state.pool)
        .await?;
        if from_quote.is_none() {
            return Err(ApiError::Denied(format!(
                "Invoice {invoice_id} belongs to another customer in QuickBooks."
            )));
        }
    }
    Ok(access)
}

async fn fetch_invoice(
    state: &AppState,
    access: &Access,
    invoice_id: &str,
) -> Result<Value, QboError> {
    let body = state
        .qbo
        .get(access, &format!("invoice/{invoice_id}"))
        .await?;
    Ok(if body["Invoice"].is_object() {
        body["Invoice"].clone()
    } else {
        body
    })
}

/// Get a customer's QuickBooks invoice
///
/// One invoice of the customer, for the invoice editor.
#[crate::openapi::path(
    get,
    path = "/v1/customers/{id}/qbo-invoices/{invoice_id}",
    operation_id = "get_customer_qbo_invoice",
    tag = "quickbooks",
    params(
        ("id" = i32, Path, description = "Customer id"),
        ("invoice_id" = String, Path, description = "QuickBooks invoice id"),
    ),
    responses(
        (status = 200, description = "Invoice", body = EditableInvoice),
        (status = 400, description = "Invalid invoice id", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Not an invoice of this customer, or the plan lacks the QuickBooks add-on (`addon`: `quickbooks`)", body = AddonRequiredBody),
        (status = 404, description = "No such customer, or it is not linked to QuickBooks", body = ErrorBody),
        (status = 409, description = "QuickBooks is not connected", body = ErrorBody),
        (status = 502, description = "QuickBooks failed", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get_invoice(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((customer_id, invoice_id)): Path<(i32, String)>,
) -> ApiResult<Json<EditableInvoice>> {
    refuse_installer(&state, &user).await?;
    let access = owned_invoice(&state, user.company_id, customer_id, &invoice_id).await?;
    let raw = fetch_invoice(&state, &access, &invoice_id)
        .await
        .map_err(qbo_error)?;
    editable(&raw)
        .map(Json)
        .ok_or_else(|| ApiError::Upstream("Failed to parse invoice".into()))
}

/// Body of `PUT /v1/customers/{id}/qbo-invoices/{invoice_id}`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct InvoiceUpdate {
    /// The invoice as the editor left it.
    pub invoice: EditableInvoice,
}

async fn save(
    state: &AppState,
    access: &Access,
    invoice_id: &str,
    edited: &EditableInvoice,
) -> Result<Value, ApiError> {
    let current = fetch_invoice(state, access, invoice_id)
        .await
        .map_err(qbo_error)?;
    let payload = update_payload(&current, edited).map_err(ApiError::BadRequest)?;
    state
        .qbo
        .post(access, "invoice", &payload)
        .await
        .map_err(qbo_error)
}

/// Save a customer's QuickBooks invoice
///
/// Writes the editor's lines, dates, notes and addresses to the invoice.
/// Lines the editor removed are deleted; discounts and tax stay. When
/// `QuickBooks` refuses a stale version the update is retried once on the
/// current one.
#[crate::openapi::path(
    put,
    path = "/v1/customers/{id}/qbo-invoices/{invoice_id}",
    operation_id = "update_customer_qbo_invoice",
    tag = "quickbooks",
    params(
        ("id" = i32, Path, description = "Customer id"),
        ("invoice_id" = String, Path, description = "QuickBooks invoice id"),
    ),
    request_body = InvoiceUpdate,
    responses(
        (status = 200, description = "The saved invoice", body = EditableInvoice),
        (status = 400, description = "Invalid invoice, or a sales line without an item", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Not an invoice of this customer, or the plan lacks the QuickBooks add-on (`addon`: `quickbooks`)", body = AddonRequiredBody),
        (status = 404, description = "No such customer, or it is not linked to QuickBooks", body = ErrorBody),
        (status = 409, description = "QuickBooks is not connected", body = ErrorBody),
        (status = 502, description = "QuickBooks failed", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update_invoice(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((customer_id, invoice_id)): Path<(i32, String)>,
    JsonBody(input): JsonBody<InvoiceUpdate>,
) -> ApiResult<Json<EditableInvoice>> {
    refuse_installer(&state, &user).await?;
    if input.invoice.id != invoice_id {
        return Err(ApiError::BadRequest("Invalid invoice payload".into()));
    }
    let access = owned_invoice(&state, user.company_id, customer_id, &invoice_id).await?;
    let saved = match save(&state, &access, &invoice_id, &input.invoice).await {
        Ok(saved) => saved,
        Err(ApiError::Upstream(_)) => save(&state, &access, &invoice_id, &input.invoice).await?,
        Err(err) => return Err(err),
    };
    let saved = if saved["Invoice"].is_object() {
        &saved["Invoice"]
    } else {
        &saved
    };
    editable(saved)
        .map(Json)
        .ok_or_else(|| ApiError::Upstream("Failed to parse updated invoice".into()))
}

fn pdf_response(invoice_id: &str, bytes: Vec<u8>) -> impl IntoResponse + use<> {
    (
        [
            (header::CONTENT_TYPE, "application/pdf".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("inline; filename=\"invoice-{invoice_id}.pdf\""),
            ),
            (header::CACHE_CONTROL, "private, no-store".to_string()),
        ],
        bytes,
    )
}

/// Get a customer's QuickBooks invoice as a PDF
///
/// The invoice's PDF, once it is known to be this customer's.
#[crate::openapi::path(
    get,
    path = "/v1/customers/{id}/qbo-invoices/{invoice_id}/pdf",
    operation_id = "get_customer_qbo_invoice_pdf",
    tag = "quickbooks",
    params(
        ("id" = i32, Path, description = "Customer id"),
        ("invoice_id" = String, Path, description = "QuickBooks invoice id"),
    ),
    responses(
        (status = 200, description = "The PDF", content_type = "application/pdf", body = Vec<u8>),
        (status = 400, description = "Invalid invoice id", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Not an invoice of this customer, or the plan lacks the QuickBooks add-on (`addon`: `quickbooks`)", body = AddonRequiredBody),
        (status = 404, description = "No such customer, or it is not linked to QuickBooks", body = ErrorBody),
        (status = 409, description = "QuickBooks is not connected", body = ErrorBody),
        (status = 502, description = "QuickBooks failed", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn customer_invoice_pdf(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((customer_id, invoice_id)): Path<(i32, String)>,
) -> ApiResult<impl IntoResponse> {
    refuse_installer(&state, &user).await?;
    customer_invoice_pdf_of(&state, user.company_id, customer_id, &invoice_id).await
}

/// The PDF of one of a customer's `QuickBooks` invoices (also for the
/// customer's own view).
pub async fn customer_invoice_pdf_of(
    state: &AppState,
    company_id: i32,
    customer_id: i32,
    invoice_id: &str,
) -> ApiResult<axum::response::Response> {
    let access = owned_invoice(state, company_id, customer_id, invoice_id).await?;
    let bytes = state
        .qbo
        .invoice_pdf(&access, invoice_id)
        .await
        .map_err(qbo_error)?;
    Ok(pdf_response(invoice_id, bytes).into_response())
}

/// Get a QuickBooks invoice as a PDF
///
/// Any invoice of the company's `QuickBooks`, for the rep's sales panel and
/// schedule events.
#[crate::openapi::path(
    get,
    path = "/v1/qbo/invoices/{invoice_id}/pdf",
    operation_id = "get_qbo_invoice_pdf",
    tag = "quickbooks",
    params(("invoice_id" = String, Path, description = "QuickBooks invoice id")),
    responses(
        (status = 200, description = "The PDF", content_type = "application/pdf", body = Vec<u8>),
        (status = 400, description = "Invalid invoice id", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin, or the plan lacks the QuickBooks add-on (`addon`: `quickbooks`)", body = AddonRequiredBody),
        (status = 409, description = "QuickBooks is not connected", body = ErrorBody),
        (status = 502, description = "QuickBooks failed", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn invoice_pdf(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(invoice_id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    if !valid_id(&invoice_id) {
        return Err(ApiError::BadRequest("Invalid invoice".into()));
    }
    if installer_only(&state, &user).await? {
        let linked: Option<i32> = sqlx::query_scalar(
            "SELECT 1 FROM event_qbo_invoices i
               JOIN events e ON e.id = i.event_id
              WHERE i.company_id = ? AND i.qbo_invoice_id = ?
                AND e.assigned_user_id = ? AND e.deleted_date IS NULL
              LIMIT 1",
        )
        .bind(user.company_id)
        .bind(&invoice_id)
        .bind(user.id)
        .fetch_optional(&state.pool)
        .await?;
        if linked.is_none() {
            return Err(ApiError::Denied(
                "This invoice is not linked to one of your jobs.".into(),
            ));
        }
    }
    let access = state
        .qbo
        .access(&state.pool, user.company_id)
        .await
        .map_err(qbo_error)?;
    let bytes = state
        .qbo
        .invoice_pdf(&access, &invoice_id)
        .await
        .map_err(qbo_error)?;
    Ok(pdf_response(&invoice_id, bytes))
}

/// Query of `GET /v1/qbo/customers`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CustomersQuery {
    /// Exact primary email.
    pub email: Option<String>,
    /// Part of the display name (wins over `email`).
    pub name: Option<String>,
}

/// `GET /v1/qbo/customers`.
#[derive(Debug, Serialize, ToSchema)]
pub struct QboCustomers {
    /// False when the company has not connected `QuickBooks`.
    pub connected: bool,
    /// Matches.
    pub customers: Vec<QboCustomer>,
}

/// Find QuickBooks customers
///
/// `QuickBooks` customers whose display name contains `name` (up to 25), or
/// whose primary email is `email`.
#[crate::openapi::path(
    get,
    path = "/v1/qbo/customers",
    operation_id = "search_qbo_customers",
    tag = "quickbooks",
    params(CustomersQuery),
    responses(
        (status = 200, description = "Matches", body = QboCustomers),
        (status = 400, description = "Neither email nor name", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin, or the plan lacks the QuickBooks add-on (`addon`: `quickbooks`)", body = AddonRequiredBody),
        (status = 502, description = "QuickBooks failed", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn customers(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Query(query): Query<CustomersQuery>,
) -> ApiResult<Json<QboCustomers>> {
    refuse_installer(&state, &user).await?;
    let name = query.name.as_deref().map_or("", str::trim);
    let email = query.email.as_deref().map_or("", str::trim);
    if name.is_empty() && email.is_empty() {
        return Err(ApiError::BadRequest("email or name is required".into()));
    }
    let access = match state.qbo.access(&state.pool, user.company_id).await {
        Ok(access) => access,
        Err(err) if err.is_not_connected() => {
            return Ok(Json(QboCustomers {
                connected: false,
                customers: Vec::new(),
            }));
        }
        Err(err) => return Err(qbo_error(err)),
    };
    let customers = if name.is_empty() {
        by_email(&state, &access, email)
            .await
            .map_err(qbo_error)?
            .iter()
            .filter_map(qbo_customer)
            .collect()
    } else {
        by_name(&state, &access, name).await.map_err(qbo_error)?
    };
    Ok(Json(QboCustomers {
        connected: true,
        customers,
    }))
}

/// A product or service an invoice line can sell.
#[derive(Debug, Serialize, ToSchema)]
pub struct QboItem {
    /// `QuickBooks` item id.
    pub id: String,
    /// Its name.
    pub name: String,
}

/// `GET /v1/qbo/items`.
#[derive(Debug, Serialize, ToSchema)]
pub struct QboItems {
    /// By name.
    pub items: Vec<QboItem>,
    /// The active classes' full names, by name.
    pub classes: Vec<String>,
}

/// List QuickBooks products and services
///
/// The company's active services and products (up to 1000), by name, for
/// the lines of a new invoice.
#[crate::openapi::path(
    get,
    path = "/v1/qbo/items",
    operation_id = "list_qbo_items",
    tag = "quickbooks",
    responses(
        (status = 200, description = "Items", body = QboItems),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 409, description = "QuickBooks is not connected", body = ErrorBody),
        (status = 502, description = "QuickBooks failed", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn items(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<QboItems>> {
    refuse_installer(&state, &user).await?;
    company_items(&state, user.company_id).await.map(Json)
}

/// The company's active products and services and classes, by name.
async fn company_items(state: &AppState, company_id: i32) -> ApiResult<QboItems> {
    let access = state
        .qbo
        .access(&state.pool, company_id)
        .await
        .map_err(qbo_error)?;
    let rows = state
        .qbo
        .query(
            &access,
            "select * from Item where Active = true MAXRESULTS 1000",
            "Item",
        )
        .await
        .map_err(qbo_error)?;
    let mut items: Vec<QboItem> = rows
        .iter()
        .filter(|row| {
            matches!(
                row["Type"].as_str(),
                Some("Service" | "NonInventory" | "Inventory")
            )
        })
        .filter_map(|row| {
            Some(QboItem {
                id: text(&row["Id"])?,
                name: text(&row["Name"])?,
            })
        })
        .collect();
    items.sort_by_key(|item| item.name.to_lowercase());
    let mut classes: Vec<String> = state
        .qbo
        .query(
            &access,
            "select * from Class where Active = true MAXRESULTS 1000",
            "Class",
        )
        .await
        .map_err(qbo_error)?
        .iter()
        .filter_map(|row| text(&row["FullyQualifiedName"]).or_else(|| text(&row["Name"])))
        .collect();
    classes.sort_by_key(|name| name.to_lowercase());
    Ok(QboItems { items, classes })
}

/// Query of `GET /v1/qbo/invoices/recent`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct RecentQuery {
    /// How many, newest first (1–500, default 200).
    pub limit: Option<u32>,
}

/// `GET /v1/qbo/invoices/recent`.
#[derive(Debug, Serialize, ToSchema)]
pub struct RecentInvoices {
    /// `QuickBooks`' own invoice objects (class, CC, terms, lines, ...).
    #[schema(value_type = Vec<Object>)]
    pub invoices: Vec<Value>,
}

/// List recent QuickBooks invoices as QuickBooks has them
///
/// The company's newest invoices with every field `QuickBooks` keeps (class,
/// CC, terms, custom fields, lines), to learn how the company writes its
/// invoices. Admins only; read-only.
#[crate::openapi::path(
    get,
    path = "/v1/qbo/invoices/recent",
    operation_id = "list_recent_qbo_invoices",
    tag = "quickbooks",
    params(RecentQuery),
    responses(
        (status = 200, description = "Invoices, newest first", body = RecentInvoices),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 409, description = "QuickBooks is not connected", body = ErrorBody),
        (status = 502, description = "QuickBooks failed", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn recent_invoices(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Query(query): Query<RecentQuery>,
) -> ApiResult<Json<RecentInvoices>> {
    let limit = query.limit.unwrap_or(200).clamp(1, 500);
    let access = state
        .qbo
        .access(&state.pool, user.company_id)
        .await
        .map_err(qbo_error)?;
    let invoices = state
        .qbo
        .query(
            &access,
            &format!(
                "select * from Invoice ORDERBY MetaData.CreateTime DESC STARTPOSITION 1 MAXRESULTS {limit}"
            ),
            "Invoice",
        )
        .await
        .map_err(qbo_error)?;
    Ok(Json(RecentInvoices { invoices }))
}

#[cfg(test)]
mod tests {
    use super::{QboCustomer, pick_by_name};

    fn found(id: &str, name: &str, email: Option<&str>) -> QboCustomer {
        QboCustomer {
            id: id.into(),
            display_name: name.into(),
            email: email.map(Into::into),
        }
    }

    #[test]
    fn names_pick_an_exact_or_unique_match() {
        let list = [
            found("1", "Alice Lead", Some("a@x.test")),
            found("2", "Alice Lead", Some("b@x.test")),
            found("3", "Alice Leader", None),
        ];
        assert_eq!(
            pick_by_name(&list, "alice lead", "B@x.test"),
            Some("2".into())
        );
        assert_eq!(pick_by_name(&list, "alice lead", ""), None);
        assert_eq!(pick_by_name(&list[2..], "Alice", ""), Some("3".into()));
    }
}
