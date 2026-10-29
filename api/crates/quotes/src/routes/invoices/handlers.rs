use super::queries::{
    customer_exists, find_invoice, insert_invoice, list_invoices, next_number, quote_customer,
    set_status, update_invoice,
};
use super::schemas::{
    Invoice, InvoiceInput, InvoiceList, InvoiceListQuery, InvoiceStatus, InvoiceStatusInput,
};
use super::validate::{CleanInvoice, clean};
use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::routes::emails::send::valid_address;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;

/// Tries to give an invoice the next number before giving up when other
/// invoices keep taking it.
const NUMBER_ATTEMPTS: usize = 3;

fn is_duplicate(err: &sqlx::Error) -> bool {
    err.as_database_error()
        .is_some_and(sqlx::error::DatabaseError::is_unique_violation)
}

fn taken(number: i32) -> ApiError {
    ApiError::Conflict(format!("Invoice #{number} already exists"))
}

/// The editor's invoice, checked, with its customer and quote checked to be
/// the company's. A quote without a customer given bills the quote's.
async fn checked(
    state: &AppState,
    company_id: i32,
    input: &InvoiceInput,
) -> ApiResult<CleanInvoice> {
    let mut invoice = clean(input, valid_address).map_err(ApiError::BadRequest)?;
    if let Some(quote_id) = invoice.quote_id {
        let Some(quote_customer) = quote_customer(&state.pool, company_id, quote_id).await? else {
            return Err(ApiError::BadRequest("Unknown quote".into()));
        };
        match (invoice.customer_id, quote_customer) {
            (Some(customer), Some(of_quote)) if customer != of_quote => {
                return Err(ApiError::BadRequest(
                    "This quote is not one of this customer's.".into(),
                ));
            }
            (None, of_quote) => invoice.customer_id = of_quote,
            _ => {}
        }
    }
    if let Some(customer_id) = invoice.customer_id
        && !customer_exists(&state.pool, company_id, customer_id).await?
    {
        return Err(ApiError::BadRequest("Unknown customer".into()));
    }
    Ok(invoice)
}

async fn invoice_or_404(state: &AppState, company_id: i32, id: i32) -> ApiResult<Invoice> {
    find_invoice(&state.pool, company_id, id)
        .await?
        .map(Invoice::from)
        .ok_or(ApiError::NotFound)
}

/// List the company's invoices
///
/// Returns the invoices the CRM keeps itself (not QuickBooks invoices, which
/// are at `GET /v1/customers/{id}/qbo-invoices`), highest number first, at
/// most 500, with `next_number`: the number a new invoice gets. Filters
/// combine: `customer_id`, `quote_id`, `status`, and `search` (an invoice
/// number, or part of the bill-to name, email or project address). Open one
/// with `GET /v1/invoices/{id}`.
#[crate::openapi::path(
    get,
    path = "/v1/invoices",
    operation_id = "list_invoices",
    tag = "invoices",
    params(InvoiceListQuery),
    responses(
        (status = 200, description = "Matching invoices and the next number", body = InvoiceList),
        (status = 400, description = "A filter is not valid (unknown status, non-numeric id)", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Query(query): Query<InvoiceListQuery>,
) -> ApiResult<Json<InvoiceList>> {
    let status = match query.status.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(status) => Some(
            InvoiceStatus::parse(status)
                .ok_or_else(|| ApiError::BadRequest("Unknown invoice status".into()))?,
        ),
    };
    let invoices = list_invoices(&state.pool, user.company_id, &query, status).await?;
    let next_number = next_number(&state.pool, user.company_id).await?;
    Ok(Json(InvoiceList {
        invoices,
        next_number,
    }))
}

/// Create an invoice
///
/// Saves a new invoice as a `draft`, for companies that invoice without
/// QuickBooks. Line amounts, the subtotal and the total (subtotal + tax)
/// are computed here. Without `invoice_number` it takes the company's next
/// number. With a `quote_id` and no `customer_id`, the quote's customer is
/// billed. Nothing is sent: email it to the customer with `POST /v1/emails`
/// (the invoice PDF attached), then mark it sent with
/// `PUT /v1/invoices/{id}/status`.
#[crate::openapi::path(
    post,
    path = "/v1/invoices",
    operation_id = "create_invoice",
    tag = "invoices",
    request_body = InvoiceInput,
    responses(
        (status = 201, description = "The new invoice", body = Invoice),
        (status = 400, description = "Missing name, bad date or email, no priced line, or an unknown customer or quote", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 409, description = "The invoice number is taken", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<InvoiceInput>,
) -> ApiResult<(StatusCode, Json<Invoice>)> {
    let company_id = user.company_id;
    let invoice = checked(&state, company_id, &input).await?;
    for _ in 0..NUMBER_ATTEMPTS {
        let number = match invoice.invoice_number {
            Some(number) => number,
            None => next_number(&state.pool, company_id).await?,
        };
        match insert_invoice(&state.pool, company_id, user.id, number, &invoice).await {
            Ok(id) => {
                let created = invoice_or_404(&state, company_id, id).await?;
                return Ok((StatusCode::CREATED, Json(created)));
            }
            Err(err) if is_duplicate(&err) => {
                if invoice.invoice_number.is_some() {
                    return Err(taken(number));
                }
            }
            Err(err) => return Err(err.into()),
        }
    }
    Err(ApiError::Conflict(
        "Could not number the invoice. Try again.".into(),
    ))
}

/// Get one invoice
///
/// Returns the whole invoice: its lines, totals, status, customer and
/// quote. Only the caller's company's invoices are found.
#[crate::openapi::path(
    get,
    path = "/v1/invoices/{id}",
    operation_id = "get_invoice",
    tag = "invoices",
    params(("id" = i32, Path, description = "Invoice id")),
    responses(
        (status = 200, description = "The invoice", body = Invoice),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such invoice in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<Invoice>> {
    Ok(Json(invoice_or_404(&state, user.company_id, id).await?))
}

/// Edit an invoice
///
/// Replaces the invoice's number, customer, quote, dates, bill-to, lines,
/// tax and notes with the body (same rules as `POST /v1/invoices`; leaving
/// out `invoice_number` keeps the current one). The status is not changed:
/// use `PUT /v1/invoices/{id}/status`. Returns the saved invoice.
#[crate::openapi::path(
    put,
    path = "/v1/invoices/{id}",
    operation_id = "update_invoice",
    tag = "invoices",
    params(("id" = i32, Path, description = "Invoice id")),
    request_body = InvoiceInput,
    responses(
        (status = 200, description = "The saved invoice", body = Invoice),
        (status = 400, description = "Missing name, bad date or email, no priced line, or an unknown customer or quote", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such invoice in the company", body = ErrorBody),
        (status = 409, description = "The invoice number is taken", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<InvoiceInput>,
) -> ApiResult<Json<Invoice>> {
    let company_id = user.company_id;
    let current = invoice_or_404(&state, company_id, id).await?;
    let invoice = checked(&state, company_id, &input).await?;
    let number = invoice.invoice_number.unwrap_or(current.invoice_number);
    match update_invoice(&state.pool, company_id, id, number, &invoice).await {
        Ok(true) => Ok(Json(invoice_or_404(&state, company_id, id).await?)),
        Ok(false) => Err(ApiError::NotFound),
        Err(err) if is_duplicate(&err) => Err(taken(number)),
        Err(err) => Err(err.into()),
    }
}

/// Change an invoice's status
///
/// Marks the invoice `draft`, `sent`, `paid` or `void`. `sent` (and `paid`)
/// record when it was first sent; `paid` records when it was paid, and any
/// other status clears that. Call it with `sent` after emailing the invoice
/// to the customer. Returns the invoice.
#[crate::openapi::path(
    put,
    path = "/v1/invoices/{id}/status",
    operation_id = "set_invoice_status",
    tag = "invoices",
    params(("id" = i32, Path, description = "Invoice id")),
    request_body = InvoiceStatusInput,
    responses(
        (status = 200, description = "The invoice with its new status", body = Invoice),
        (status = 400, description = "Unknown status", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such invoice in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn status(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<InvoiceStatusInput>,
) -> ApiResult<Json<Invoice>> {
    if !set_status(&state.pool, user.company_id, id, input.status).await? {
        return Err(ApiError::NotFound);
    }
    Ok(Json(invoice_or_404(&state, user.company_id, id).await?))
}
