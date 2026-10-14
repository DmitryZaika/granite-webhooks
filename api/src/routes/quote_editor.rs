//! One quote: what the quote page and the drawing editor load and change.
//!
//! - `POST   /v1/quotes`                      → a new quote (empty drawing, the price list's defaults)
//! - `GET    /v1/quotes/{id}?revision=`       → the quote, its drawing (or a revision's), its prices and history
//! - `PATCH  /v1/quotes/{id}`                 → info, settings and the price list
//! - `PUT    /v1/quotes/{id}/drawing`         → save the drawing as a new revision
//! - `PUT|DELETE /v1/quotes/{id}/autosave`    → the caller's unsaved drawing
//! - `GET    /v1/quotes/{id}/revisions/{revision_id}` → a revision's drawing
//! - `POST   /v1/quotes/{id}/prices`          → move the quote to today's prices
//! - `POST|DELETE /v1/quotes/{id}/sale`       → mark sold with an option, or undo the sale
//! - `POST   /v1/quotes/{id}/copies`          → duplicate
//! - `POST   /v1/quotes/{id}/emails`          → email the customer a link to the quote
//! - `PATCH  /v1/quotes/{id}/emails/{email_id}` → switch an email's link off or on
//! - `POST   /v1/quotes/{id}/portal-token`    → the customer link
//! - `POST   /v1/quotes/{id}/prints`          → record a print
//! - `GET|POST /v1/me/quote-email-templates`, `PUT|DELETE .../{id}` → the Email dialog's templates
//!
//! Port of `crud/quotes.server.ts`, `quoteSale.server.ts`,
//! `advanceQuoteDeal.server.ts`, `quoteEmailTemplates.server.ts` and the
//! loaders and actions of `employee.quotes.add`, `employee.quote.$quoteId`
//! (and its print page) and `employee.draw.$quoteId`. The browser prices the
//! drawing with the price lists given here, as the editor always did.

use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::quote_pricing::doc::QuoteDoc;
use crate::quote_pricing::{
    self, load_company_quote, mark_quote_sold, pricing_for_quote, quote_for_contract_file,
    sold_out_colors, sold_out_message, with_current_sink_models,
};
use crate::routes::auth::{app_origin, company_subdomain};
use crate::routes::created::CreatedId;
use crate::routes::emails::thread::sending_address;
use crate::routes::quote_extras::load_catalog;
use crate::routes::quote_forms;
use crate::routes::quote_presets::load_quote_settings;
use crate::routes::quote_price_lists::{default_id, find as find_price_list, list_rows};
use crate::routes::templates::order;
use crate::routes::templates::schemas::{OrderingMode, OrderingType};
use crate::state::AppState;
use crate::webhooks_client::OutgoingEmail;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use common::crud::template::fetch_template_variable_data;
use common::utils::template::replace_template_variables;
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::collections::{BTreeMap, HashMap};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(save_drawing))
        .routes(routes!(save_autosave, discard_autosave))
        .routes(routes!(revision_drawing))
        .routes(routes!(update_prices))
        .routes(routes!(mark_sold, undo_sale))
        .routes(routes!(copy_quote))
        .routes(routes!(email_quote))
        .routes(routes!(set_email_link))
        .routes(routes!(portal_token))
        .routes(routes!(record_print))
        .routes(routes!(my_email_templates, create_my_email_template))
        .routes(routes!(update_my_email_template, delete_my_email_template))
}

/// A quote as the CRM loads it (`QuoteRow`).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct QuoteRow {
    pub id: i32,
    pub company_id: i32,
    pub customer_id: i32,
    pub customer_name: Option<String>,
    pub deal_id: Option<i64>,
    pub quote_name: Option<String>,
    pub quote_type: Option<String>,
    pub status: Option<String>,
    pub sales_rep: Option<String>,
    pub salesperson_id: Option<i32>,
    pub created_by: Option<i32>,
    pub created_by_name: Option<String>,
    pub created_date: Option<String>,
    pub price_list_id: Option<i32>,
    pub price_list_revision_id: Option<i32>,
    pub price_list_name: Option<String>,
    pub price_list_revision_number: Option<i32>,
    pub expiration_date: Option<String>,
    pub notes: Option<String>,
    pub tax_pct: f64,
    pub discount_pct: f64,
    pub discount_amount: f64,
    pub payment_terms_json: Option<String>,
    pub address: Option<String>,
    pub phone: Option<String>,
    pub customer_phone: Option<String>,
    pub customer_phone_2: Option<String>,
    pub email: Option<String>,
    pub address_notes: Option<String>,
    pub salesperson_phone: Option<String>,
    pub draft_drawing_json: Option<String>,
    pub current_step: Option<i32>,
    pub current_revision: Option<i32>,
    pub portal_token: Option<String>,
    pub signed_at: Option<String>,
    pub signature_data: Option<String>,
    pub customer_accepted_option_id: Option<i32>,
}

pub const QUOTE_ROW_SELECT: &str = "SELECT q.id, q.company_id, q.customer_id, c.name AS customer_name,
       CAST(q.deal_id AS SIGNED) AS deal_id, q.quote_name, q.quote_type, CAST(q.status AS CHAR) AS status,
       q.sales_rep, q.salesperson_id, q.created_by, cb.name AS created_by_name,
       DATE_FORMAT(q.created_date, '%Y-%m-%dT%H:%i:%sZ') AS created_date,
       q.price_list_id, q.price_list_revision_id, pl.name AS price_list_name,
       plr.revision_number AS price_list_revision_number,
       DATE_FORMAT(q.expiration_date, '%Y-%m-%d') AS expiration_date,
       q.notes, CAST(COALESCE(q.tax_pct, 0) AS DOUBLE) AS tax_pct,
       CAST(COALESCE(q.discount_pct, 0) AS DOUBLE) AS discount_pct,
       CAST(COALESCE(q.discount_amount, 0) AS DOUBLE) AS discount_amount,
       CAST(q.payment_terms_json AS CHAR) AS payment_terms_json, q.address, q.phone,
       c.phone AS customer_phone, c.phone_2 AS customer_phone_2, q.email, q.address_notes,
       sp.phone_number AS salesperson_phone, CAST(q.draft_drawing_json AS CHAR) AS draft_drawing_json,
       q.current_step, q.current_revision, q.portal_token,
       DATE_FORMAT(q.signed_at, '%Y-%m-%dT%H:%i:%sZ') AS signed_at, q.signature_data,
       q.customer_accepted_option_id
  FROM quotes q
  LEFT JOIN customers c ON c.id = q.customer_id
  LEFT JOIN users cb ON cb.id = q.created_by
  LEFT JOIN users sp ON sp.id = q.salesperson_id
  LEFT JOIN price_lists pl ON pl.id = q.price_list_id
  LEFT JOIN price_list_revisions plr ON plr.id = q.price_list_revision_id";

/// `loadCompanyQuote`.
pub async fn find_quote(pool: &MySqlPool, id: i32, company_id: i32) -> ApiResult<Option<QuoteRow>> {
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{QUOTE_ROW_SELECT} WHERE q.id = ? AND q.company_id = ? AND q.deleted_at IS NULL"
    )))
    .bind(id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?)
}

impl QuoteRow {
    /// `quoteDisplayName`: the quote's name, else the customer's, else "Quote".
    pub fn display_name(&self) -> String {
        [self.quote_name.as_deref(), self.customer_name.as_deref()]
            .into_iter()
            .flatten()
            .map(str::trim)
            .find(|n| !n.is_empty())
            .unwrap_or("Quote")
            .to_string()
    }

    /// The stored drawing as JSON (the browser reads it with the CRM's schema).
    pub fn drawing(&self) -> Value {
        self.draft_drawing_json
            .as_deref()
            .and_then(|t| serde_json::from_str(t).ok())
            .unwrap_or(Value::Null)
    }
}

async fn quote_or_404(state: &AppState, id: i32, company_id: i32) -> ApiResult<QuoteRow> {
    find_quote(&state.pool, id, company_id)
        .await?
        .ok_or(ApiError::NotFound)
}

async fn pricing_quote(
    state: &AppState,
    id: i32,
    company_id: i32,
) -> ApiResult<quote_pricing::Quote> {
    load_company_quote(&state.pool, id, company_id)
        .await?
        .ok_or(ApiError::NotFound)
}

/// Payment terms with the CRM's defaults filled in.
fn terms_of(raw: Option<&str>) -> Value {
    let mut terms =
        json!({ "terms": "", "mode": "percent", "deposits": [], "remaining": "Balance Due" });
    if let Some(Value::Object(saved)) = raw.and_then(|t| serde_json::from_str::<Value>(t).ok()) {
        for (key, value) in saved {
            terms[key] = value;
        }
    }
    terms
}

/// A sales rep a quote can name.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct Salesperson {
    /// User id.
    pub id: i32,
    /// Name.
    pub name: Option<String>,
    /// Phone.
    pub phone_number: Option<String>,
}

async fn sales_reps(pool: &MySqlPool, company_id: i32) -> ApiResult<Vec<Salesperson>> {
    Ok(sqlx::query_as(
        "SELECT DISTINCT u.id, u.name, u.phone_number
           FROM users u
           JOIN users_positions up ON up.user_id = u.id
           JOIN positions p ON p.id = up.position_id
          WHERE LOWER(p.name) = 'sales_rep' AND u.is_deleted = 0 AND u.company_id = ?
          ORDER BY u.name",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await?)
}

/// Body of `POST /v1/quotes`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewQuote {
    /// The customer.
    pub customer_id: i32,
    /// Name; blank takes the customer's.
    #[serde(default)]
    pub quote_name: String,
    /// Price list; none takes the company's default.
    #[serde(default)]
    pub price_list_id: Option<i32>,
}

/// `newId(prefix)`.
fn new_id(prefix: &str) -> String {
    quote_pricing::edit::new_id(prefix)
}

/// Create a quote
///
/// An empty drawing (the company's counter depth and rounding) with the
/// price list's "always include" extras, the caller as sales rep, the
/// customer's address, phone and email copied, and the price list's default
/// tax rate and payment terms (else the company's first payment terms).
#[utoipa::path(
    post,
    path = "/v1/quotes",
    operation_id = "create_quote",
    tag = "quotes",
    request_body = NewQuote,
    responses(
        (status = 201, description = "Created", body = CreatedId),
        (status = 400, description = "No customer", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
#[allow(clippy::too_many_lines)]
pub async fn create_quote(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<NewQuote>,
) -> ApiResult<(StatusCode, Json<CreatedId>)> {
    #[derive(sqlx::FromRow)]
    struct Customer {
        name: Option<String>,
        address: Option<String>,
        phone: Option<String>,
        phone_2: Option<String>,
        email: Option<String>,
    }
    let customer: Option<Customer> = sqlx::query_as(
        "SELECT c.name, c.address, c.phone, c.phone_2, ce.email
           FROM customers c
           LEFT JOIN customers_emails ce ON ce.id = c.email_id AND ce.customer_id = c.id
          WHERE c.id = ? AND c.company_id = ?",
    )
    .bind(input.customer_id)
    .bind(user.company_id)
    .fetch_optional(&state.pool)
    .await?;
    let customer = customer.ok_or_else(|| ApiError::BadRequest("Customer is required".into()))?;
    let lists = list_rows(&state.pool, user.company_id).await?;
    let chosen_id = match input.price_list_id {
        Some(id) => Some(id),
        None => default_id(&state.pool, user.company_id, &lists).await?,
    };
    let chosen = match chosen_id {
        Some(id) => find_price_list(&state.pool, id, user.company_id).await?,
        None => None,
    };
    let info = chosen.as_ref().and_then(|(_, info)| info.clone());
    let settings = load_quote_settings(&state.pool, user.company_id).await?;
    // Catalog extras count too ("always include" set in Admin > Quotes > Extras).
    let catalog: Vec<(i64, Value)> = load_catalog(&state.pool, user.company_id)
        .await?
        .into_iter()
        .map(|e| (e.id, e.item))
        .collect();
    let offered = info
        .as_ref()
        .map(|info| quote_pricing::price_list::apply_catalog(info, &catalog));
    let items: Vec<Value> = offered
        .as_ref()
        .map(|info| quote_pricing::js::arr(Some(info), "arrOtherItems").to_vec())
        .unwrap_or_default()
        .into_iter()
        .filter(|o| quote_pricing::js::on(o.get("alwaysIncludeOnQuote")))
        .map(|o| {
            json!({
                "id": new_id("item"),
                "kind": "priceList",
                "areaId": null,
                "name": o.get("name").cloned().unwrap_or(Value::Null),
                "option": null,
                "catalogId": o.get("catalogId").cloned().unwrap_or(Value::Null),
                "optionId": null,
                "quantity": 1,
                "amount": if quote_pricing::js::arr(Some(&o), "arrRanges").is_empty() { Value::Null } else { json!(0) },
                "price": null,
                "hideOnQuote": false,
                "allowDiscount": true,
                "taxable": true,
            })
        })
        .collect();
    let doc = json!({
        "version": 2,
        "defaultDepth": settings.default_depth,
        "counters": [],
        "otherCounters": [],
        "texts": [],
        "pageBreaks": [],
        "areas": [{ "id": "area-1", "name": "Area #1", "quantity": 1 }],
        "colorOptions": [{ "id": "option-1", "byArea": {} }],
        "layouts": {},
        "items": items,
        "priceOverrides": {},
        "rounding": settings.rounding,
    });
    let typed = input.quote_name.trim();
    let name = if typed.is_empty() {
        customer
            .name
            .as_deref()
            .unwrap_or_default()
            .trim()
            .to_string()
    } else {
        typed.to_string()
    };
    let copy = |v: Option<&str>| {
        v.map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    let expiration_days = info
        .as_ref()
        .and_then(|i| quote_pricing::js::num(Some(i), "expirationDays"))
        .unwrap_or(30.0);
    #[allow(clippy::cast_possible_truncation)]
    let expiration_days = expiration_days as i64;
    let id = sqlx::query(
        "INSERT INTO quotes (customer_id, quote_name, quote_type, created_date, sales_rep, company_id,
                             salesperson_id, created_by, price_list_id, price_list_revision_id,
                             expiration_date, deal_id, draft_drawing_json, status, updated_at,
                             address, phone, email)
         VALUES (?, ?, '', NOW(), ?, ?, ?, ?, ?, ?, DATE_ADD(CURDATE(), INTERVAL ? DAY), NULL, ?, 'draft', NOW(),
                 ?, ?, ?)",
    )
    .bind(input.customer_id)
    .bind(&name)
    .bind(&user.name)
    .bind(user.company_id)
    .bind(user.id)
    .bind(user.id)
    .bind(chosen.as_ref().map(|(row, _)| row.id))
    .bind(chosen.as_ref().and_then(|(row, _)| row.current_revision_id))
    .bind(expiration_days)
    .bind(doc.to_string())
    .bind(copy(customer.address.as_deref()))
    .bind(copy(customer.phone.as_deref()).or_else(|| copy(customer.phone_2.as_deref())))
    .bind(copy(customer.email.as_deref()))
    .execute(&state.pool)
    .await?
    .last_insert_id();
    // The list's default tax rate and payment terms prefill; without default
    // payment terms, the company's first ones do.
    let tax_rate: Option<f64> = match info.as_ref().and_then(|i| quote_pricing::js::num(Some(i), "defaultTaxRateId")) {
        Some(rate) => sqlx::query_scalar(
            "SELECT CAST(tax_pct AS DOUBLE) FROM quote_tax_rates WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
        )
        .bind(rate)
        .bind(user.company_id)
        .fetch_optional(&state.pool)
        .await?,
        None => None,
    };
    let default_terms = info
        .as_ref()
        .and_then(|i| quote_pricing::js::num(Some(i), "defaultPaymentTermsId"));
    let terms: Option<String> = sqlx::query_scalar(
        "SELECT terms FROM quote_payment_terms WHERE company_id = ? AND deleted_at IS NULL
          ORDER BY id = ? DESC, sequence, name LIMIT 1",
    )
    .bind(user.company_id)
    .bind(default_terms.unwrap_or(-1.0))
    .fetch_optional(&state.pool)
    .await?;
    sqlx::query(
        "UPDATE quotes SET tax_pct = COALESCE(?, tax_pct),
                payment_terms_json = COALESCE(?, payment_terms_json)
          WHERE id = ?",
    )
    .bind(tax_rate)
    .bind(terms.map(|t| json!({ "terms": t }).to_string()))
    .bind(id)
    .execute(&state.pool)
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(CreatedId {
            id: i64::try_from(id).unwrap_or_default(),
        }),
    ))
}

/// `?revision=`: show a saved revision's drawing.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct RevisionQuery {
    /// Revision id.
    #[serde(default)]
    pub revision: Option<i32>,
}

/// The quote's own fields.
#[derive(Debug, Serialize, ToSchema)]
pub struct QuoteInfo {
    /// Quote id.
    pub id: i32,
    /// Its own name.
    pub quote_name: Option<String>,
    /// What it is called (its name, else the customer's).
    pub name: String,
    /// Expires on (YYYY-MM-DD).
    pub expiration_date: Option<String>,
    /// The price list.
    pub price_list_id: Option<i32>,
    /// Its name.
    pub price_list_name: Option<String>,
    /// The revision the quote prices with.
    pub price_list_revision_id: Option<i32>,
    /// Its number.
    pub price_list_revision_number: Option<i32>,
    /// Sales rep's name.
    pub sales_rep: Option<String>,
    /// Sales rep.
    pub salesperson_id: Option<i32>,
    /// Type.
    pub quote_type: Option<String>,
    /// Notes.
    pub notes: Option<String>,
    /// The customer.
    pub customer_id: i32,
    /// Their name.
    pub customer_name: Option<String>,
    /// The job's address.
    pub address: Option<String>,
    /// The quote's phone.
    pub phone: Option<String>,
    /// The customer's phone.
    pub customer_phone: Option<String>,
    /// The customer's second phone.
    pub phone_2: Option<String>,
    /// The quote's email.
    pub email: Option<String>,
    /// Address notes.
    pub address_notes: Option<String>,
    /// Latest saved revision.
    pub current_revision: Option<i32>,
    /// Editor step.
    pub current_step: Option<i32>,
    /// The customer link's token.
    pub portal_token: Option<String>,
    /// Payment terms (`terms`, `mode`, `deposits`, `remaining`).
    #[schema(value_type = Object)]
    pub terms: Value,
    /// `draft`, `active`, `accepted`, `lost` or `converted`.
    pub status: Option<String>,
    /// Index of the option sold or accepted.
    pub sold_option: Option<i32>,
    /// The deal.
    pub deal_id: Option<i64>,
    /// When the customer signed (ISO 8601).
    pub signed_at: Option<String>,
    /// Tax percent.
    pub tax_pct: f64,
    /// Discount percent.
    pub discount_pct: f64,
    /// Discount in dollars.
    pub discount_amount: f64,
}

/// A saved revision.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct QuoteRevision {
    /// Revision id.
    pub id: i32,
    /// Its number.
    pub revision_number: i32,
    /// When (ISO 8601).
    pub created_at: Option<String>,
    /// Who saved it.
    pub created_by: Option<String>,
    /// What changed.
    pub notes: Option<String>,
}

/// An email that carried the quote's link.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct QuoteEmailRow {
    /// Email id.
    pub id: i32,
    /// The revision sent.
    pub revision_number: Option<i32>,
    /// To.
    pub recipients: Option<String>,
    /// Subject.
    pub subject: Option<String>,
    /// When (ISO 8601).
    pub sent_at: Option<String>,
    /// Who sent it.
    pub sent_by: Option<String>,
    /// How many times the link was opened.
    pub view_count: i32,
    /// Last opened (ISO 8601).
    pub last_viewed_at: Option<String>,
    /// The link expires on (YYYY-MM-DD).
    pub expires_at: Option<String>,
    /// The message as sent.
    pub body_html: Option<String>,
    /// The link was switched off (ISO 8601).
    pub deactivated_at: Option<String>,
}

/// A contract sent for signature from this quote.
#[derive(Debug, Serialize, ToSchema)]
pub struct QuoteContract {
    /// Signature request id.
    pub id: i64,
    /// File name without `.pdf`.
    pub filename: String,
    /// Request status.
    pub status: String,
    /// Signer.
    pub signer_name: String,
    /// Signer's email.
    pub signer_email: String,
    /// Who the email carrying it went to.
    pub sent_to: Option<String>,
    /// When it was sent (ISO 8601).
    pub sent_at: Option<String>,
    /// When it was signed (ISO 8601).
    pub signed_at: Option<String>,
    /// Where to open it (an API path, or the file's URL).
    pub url: String,
}

/// A catalog photo of a color on the quote (`QuoteStoneImage`).
#[derive(Debug, Serialize, ToSchema)]
pub struct StoneImage {
    /// Color.
    pub name: String,
    /// Material.
    pub product: String,
    /// Full photo.
    pub url: String,
    /// Small photo.
    pub icon: String,
}

/// The caller's unsaved drawing.
#[derive(Debug, Serialize, ToSchema)]
pub struct Autosave {
    /// The drawing.
    #[schema(value_type = Object)]
    pub doc: Value,
    /// When (ISO 8601).
    pub saved_at: Option<String>,
}

/// Everything the quote page and the editor show.
#[derive(Debug, Serialize, ToSchema)]
pub struct QuoteDetail {
    /// The quote.
    pub quote: QuoteInfo,
    /// The drawing (or the revision's), as stored.
    #[schema(value_type = Object)]
    pub doc: Value,
    /// The revision shown instead of the current drawing.
    pub viewing_revision: Option<i32>,
    /// The quote's prices: its snapshot with today's new choices.
    #[schema(value_type = Object)]
    pub price_list: Value,
    /// The same list as it prices today.
    #[schema(value_type = Object)]
    pub latest_price_list: Value,
    /// Saved revisions, newest first.
    pub revisions: Vec<QuoteRevision>,
    /// Emails that carried the link, newest first.
    pub emails: Vec<QuoteEmailRow>,
    /// Contracts sent from the quote.
    pub contracts: Vec<QuoteContract>,
    /// The company's sales reps.
    pub salespeople: Vec<Salesperson>,
    /// Small photo per color: by `name\0material` and by name.
    pub stone_icons: BTreeMap<String, String>,
    /// Photos of the quote's colors (the browser builds the previews).
    pub stone_images: Vec<StoneImage>,
    /// The address the caller's mail leaves from.
    pub sender: String,
    /// The caller's unsaved drawing.
    pub autosave: Option<Autosave>,
}

fn icon_url(url: &str) -> String {
    url.rfind('.').map_or_else(
        || format!("{url}-icon"),
        |dot| format!("{}-icon{}", &url[..dot], &url[dot..]),
    )
}

fn lower(text: &str) -> String {
    text.trim().to_lowercase()
}

/// Colors picked on the quote: `(name, material)` once each, in order.
fn picks(doc: &Value) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let areas = quote_pricing::js::arr(Some(doc), "areas");
    for option in quote_pricing::js::arr(Some(doc), "colorOptions") {
        for area in areas {
            let id = area.get("id").and_then(Value::as_str).unwrap_or_default();
            let picked = option.get("byArea").and_then(|b| b.get(id));
            let name = picked
                .and_then(|p| p.get("color"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string();
            if name.is_empty() {
                continue;
            }
            let product = picked
                .and_then(|p| p.get("product"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string();
            if !out
                .iter()
                .any(|(n, p)| lower(n) == lower(&name) && lower(p) == lower(&product))
            {
                out.push((name, product));
            }
        }
    }
    out
}

/// `loadStoneIcons`.
async fn stone_icons(
    pool: &MySqlPool,
    company_id: i32,
    doc: &Value,
) -> ApiResult<BTreeMap<String, String>> {
    let mut names: Vec<String> = picks(doc).into_iter().map(|(n, _)| lower(&n)).collect();
    names.dedup();
    let mut icons = BTreeMap::new();
    if names.is_empty() {
        return Ok(icons);
    }
    let mut sql = sqlx::QueryBuilder::new(
        "SELECT name, COALESCE(type, ''), url FROM stones
          WHERE deleted_at IS NULL AND url IS NOT NULL AND url != '' AND company_id = ",
    );
    sql.push_bind(company_id)
        .push(" AND LOWER(TRIM(name)) IN (");
    let mut list = sql.separated(", ");
    for name in &names {
        list.push_bind(name.clone());
    }
    sql.push(")");
    let rows: Vec<(String, String, String)> = sql.build_query_as().fetch_all(pool).await?;
    for (name, kind, url) in rows {
        let icon = icon_url(&url);
        icons.insert(
            format!("{}\u{0}{}", lower(&name), lower(&kind)),
            icon.clone(),
        );
        icons.entry(lower(&name)).or_insert(icon);
    }
    Ok(icons)
}

/// `loadPickImages`: one photo per color, a stone with slabs left first.
pub async fn stone_images(
    pool: &MySqlPool,
    company_id: i32,
    doc: &Value,
) -> ApiResult<Vec<StoneImage>> {
    let picks = picks(doc);
    if picks.is_empty() {
        return Ok(Vec::new());
    }
    let mut names: Vec<String> = picks.iter().map(|(n, _)| lower(n)).collect();
    names.sort();
    names.dedup();
    let mut sql = sqlx::QueryBuilder::new(
        "SELECT s.name, COALESCE(s.type, ''), s.url FROM stones s
          WHERE s.deleted_at IS NULL AND s.url IS NOT NULL AND TRIM(s.url) != ''
            AND LOWER(TRIM(s.url)) NOT IN ('undefined', 'null') AND s.company_id = ",
    );
    sql.push_bind(company_id)
        .push(" AND LOWER(TRIM(s.name)) IN (");
    let mut list = sql.separated(", ");
    for name in &names {
        list.push_bind(name.clone());
    }
    sql.push(
        ") ORDER BY EXISTS (
             SELECT 1 FROM slab_inventory si
              WHERE si.deleted_at IS NULL AND si.sale_id IS NULL AND si.cut_date IS NULL
                AND (si.stone_id = s.id
                     OR si.stone_id IN (SELECT l.source_stone_id FROM stone_slab_links l WHERE l.stone_id = s.id))
           ) DESC, (
             SELECT COUNT(*) FROM slab_inventory own
              WHERE own.stone_id = s.id AND own.deleted_at IS NULL AND own.sale_id IS NULL AND own.cut_date IS NULL
           ) DESC, s.id DESC",
    );
    let rows: Vec<(String, String, String)> = sql.build_query_as().fetch_all(pool).await?;
    Ok(picks
        .into_iter()
        .filter_map(|(name, product)| {
            let same: Vec<&(String, String, String)> = rows
                .iter()
                .filter(|r| lower(&r.0) == lower(&name))
                .collect();
            let row = same
                .iter()
                .find(|r| lower(&r.1) == lower(&product))
                .or_else(|| same.first())?;
            let url = row.2.trim();
            if url.is_empty() || ["undefined", "null"].contains(&url.to_lowercase().as_str()) {
                return None;
            }
            Some(StoneImage {
                icon: icon_url(&row.2),
                url: row.2.clone(),
                name,
                product,
            })
        })
        .collect())
}

/// Contracts sent from this quote (`listQuoteContracts`).
async fn contracts(
    state: &AppState,
    company_id: i32,
    quote: &QuoteRow,
) -> ApiResult<Vec<QuoteContract>> {
    #[derive(sqlx::FromRow)]
    struct Request {
        id: i64,
        filename: String,
        file_url: String,
        status: String,
        provider: String,
        provider_document_id: Option<String>,
        signer_name: String,
        signer_email: String,
        receiver_email: Option<String>,
        created_at: Option<String>,
        updated_at: Option<String>,
    }
    let requests: Vec<Request> = sqlx::query_as(
        "SELECT CAST(sr.id AS SIGNED) AS id, sr.filename, sr.file_url, CAST(sr.status AS CHAR) AS status,
                sr.provider, sr.provider_document_id, sr.signer_name, sr.signer_email, e.receiver_email,
                DATE_FORMAT(sr.created_at, '%Y-%m-%dT%H:%i:%sZ') AS created_at,
                DATE_FORMAT(sr.updated_at, '%Y-%m-%dT%H:%i:%sZ') AS updated_at
           FROM signature_requests sr
           LEFT JOIN emails e ON e.id = sr.email_id
          WHERE sr.company_id = ? AND sr.customer_id = ? AND sr.status <> 'draft'
          ORDER BY sr.created_at DESC, sr.id DESC",
    )
    .bind(company_id)
    .bind(quote.customer_id)
    .fetch_all(&state.pool)
    .await?;
    let forms: Vec<String> = quote_forms::summaries(&state.pool, company_id, false)
        .await?
        .into_iter()
        .filter(|f| f.kind == "contract")
        .map(|f| f.name)
        .collect();
    let quotes = [quote.quote_name.clone()];
    let mut out = Vec::new();
    for r in requests {
        if quote_for_contract_file(&r.filename, &forms, &quotes, Option::as_deref).is_none() {
            continue;
        }
        let url = if r.provider == "native" {
            format!(
                "/v1/signatures/{}/document?customer_id={}",
                r.id, quote.customer_id
            )
        } else if r.status == "signed"
            && r.provider_document_id
                .as_deref()
                .is_some_and(|d| !d.trim().is_empty())
        {
            format!("/v1/signature-requests/{}/signed-pdf", r.id)
        } else {
            state
                .storage
                .readable_url(&r.file_url, Some("application/pdf"))
                .await
        };
        let sent_to = r.receiver_email.as_deref().and_then(|raw| {
            let address = raw
                .split_once('<')
                .and_then(|(_, rest)| rest.split_once('>'))
                .map_or(raw, |(inner, _)| inner)
                .trim();
            (!address.is_empty()).then(|| address.to_string())
        });
        let filename = r
            .filename
            .strip_suffix(".pdf")
            .or_else(|| r.filename.strip_suffix(".PDF"))
            .unwrap_or(&r.filename)
            .to_string();
        out.push(QuoteContract {
            id: r.id,
            filename,
            signed_at: (r.status == "signed")
                .then(|| r.updated_at.clone())
                .flatten(),
            status: r.status,
            signer_name: r.signer_name,
            signer_email: r.signer_email,
            sent_to,
            sent_at: r.created_at,
            url,
        });
    }
    Ok(out)
}

fn info_of(row: &QuoteRow) -> QuoteInfo {
    QuoteInfo {
        id: row.id,
        quote_name: row.quote_name.clone(),
        name: row.display_name(),
        expiration_date: row.expiration_date.clone(),
        price_list_id: row.price_list_id,
        price_list_name: row.price_list_name.clone(),
        price_list_revision_id: row.price_list_revision_id,
        price_list_revision_number: row.price_list_revision_number,
        sales_rep: row.sales_rep.clone(),
        salesperson_id: row.salesperson_id,
        quote_type: row.quote_type.clone(),
        notes: row.notes.clone(),
        customer_id: row.customer_id,
        customer_name: row.customer_name.clone(),
        address: row.address.clone(),
        phone: row.phone.clone(),
        customer_phone: row.customer_phone.clone(),
        phone_2: row.customer_phone_2.clone(),
        email: row.email.clone(),
        address_notes: row.address_notes.clone(),
        current_revision: row.current_revision,
        current_step: row.current_step,
        portal_token: row.portal_token.clone(),
        terms: terms_of(row.payment_terms_json.as_deref()),
        status: row.status.clone(),
        sold_option: row.customer_accepted_option_id,
        deal_id: row.deal_id,
        signed_at: row.signed_at.clone(),
        tax_pct: row.tax_pct,
        discount_pct: row.discount_pct,
        discount_amount: row.discount_amount,
    }
}

async fn revision_doc(
    pool: &MySqlPool,
    quote_id: i32,
    revision_id: i32,
) -> ApiResult<Option<Value>> {
    let doc: Option<Option<String>> = sqlx::query_scalar(
        "SELECT CAST(drawing_json AS CHAR) FROM quote_revisions WHERE id = ? AND quote_id = ?",
    )
    .bind(revision_id)
    .bind(quote_id)
    .fetch_optional(pool)
    .await?;
    Ok(doc.flatten().and_then(|t| serde_json::from_str(&t).ok()))
}

/// Open a quote
///
/// The quote, its drawing (a saved revision's with `revision`), its prices
/// (the snapshot it was made with plus today's choices, and today's list),
/// revisions, emails, contracts, the sales reps, color photos, the sender
/// address and the caller's unsaved drawing.
#[utoipa::path(
    get,
    path = "/v1/quotes/{id}",
    operation_id = "get_quote",
    tag = "quotes",
    params(("id" = i32, Path, description = "Quote id"), RevisionQuery),
    responses(
        (status = 200, description = "The quote", body = QuoteDetail),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such quote", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get_quote(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    Query(query): Query<RevisionQuery>,
) -> ApiResult<Json<QuoteDetail>> {
    let row = quote_or_404(&state, id, user.company_id).await?;
    let pricing = pricing_quote(&state, id, user.company_id).await?;
    let revision = match query.revision {
        Some(revision) => revision_doc(&state.pool, id, revision).await?,
        None => None,
    };
    let viewing_revision = revision.as_ref().and(query.revision);
    let doc = revision.unwrap_or_else(|| row.drawing());
    let (price_list, latest_price_list) = pricing_for_quote(&state.pool, &pricing).await?;
    let revisions: Vec<QuoteRevision> = sqlx::query_as(
        "SELECT r.id, r.revision_number, DATE_FORMAT(r.created_at, '%Y-%m-%dT%H:%i:%sZ') AS created_at,
                u.name AS created_by, r.notes
           FROM quote_revisions r LEFT JOIN users u ON u.id = r.created_by
          WHERE r.quote_id = ? ORDER BY r.revision_number DESC",
    )
    .bind(id)
    .fetch_all(&state.pool)
    .await?;
    let emails: Vec<QuoteEmailRow> = sqlx::query_as(
        "SELECT e.id, e.revision_number, e.recipients, e.subject,
                DATE_FORMAT(e.sent_at, '%Y-%m-%dT%H:%i:%sZ') AS sent_at, u.name AS sent_by,
                CAST(COALESCE(e.view_count, 0) AS SIGNED) AS view_count,
                DATE_FORMAT(e.last_viewed_at, '%Y-%m-%dT%H:%i:%sZ') AS last_viewed_at,
                DATE_FORMAT(e.expires_at, '%Y-%m-%d') AS expires_at, e.body_html,
                DATE_FORMAT(e.deactivated_at, '%Y-%m-%dT%H:%i:%sZ') AS deactivated_at
           FROM quote_emails e LEFT JOIN users u ON u.id = e.sent_by
          WHERE e.quote_id = ? ORDER BY e.sent_at DESC",
    )
    .bind(id)
    .fetch_all(&state.pool)
    .await?;
    let autosave: Option<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT CAST(drawing_json AS CHAR), DATE_FORMAT(saved_at, '%Y-%m-%dT%H:%i:%sZ')
           FROM quote_autosaves WHERE quote_id = ? AND user_id = ?",
    )
    .bind(id)
    .bind(user.id)
    .fetch_optional(&state.pool)
    .await?;
    let autosave = autosave.and_then(|(doc, saved_at)| {
        let doc: Value = serde_json::from_str(doc.as_deref()?).ok()?;
        QuoteDoc::parse(&doc).map(|_| Autosave { doc, saved_at })
    });
    Ok(Json(QuoteDetail {
        contracts: contracts(&state, user.company_id, &row).await?,
        salespeople: sales_reps(&state.pool, user.company_id).await?,
        stone_icons: stone_icons(&state.pool, user.company_id, &doc).await?,
        stone_images: stone_images(&state.pool, user.company_id, &doc).await?,
        sender: sending_address(&state.pool, user.company_id, &user.email).await?,
        quote: info_of(&row),
        doc,
        viewing_revision,
        price_list,
        latest_price_list,
        revisions,
        emails,
        autosave,
    }))
}

/// Changes to a quote; fields left out stay.
#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QuoteChanges {
    /// Name (blank keeps it).
    #[serde(default)]
    pub quote_name: Option<String>,
    /// Notes (blank clears).
    #[serde(default)]
    pub notes: Option<String>,
    /// Job address (blank clears).
    #[serde(default)]
    pub address: Option<String>,
    /// Phone (blank clears).
    #[serde(default)]
    pub phone: Option<String>,
    /// The customer's second phone (saved on the customer; blank clears).
    #[serde(default)]
    pub phone_2: Option<String>,
    /// Email (blank clears).
    #[serde(default)]
    pub email: Option<String>,
    /// Address notes (blank clears).
    #[serde(default)]
    pub address_notes: Option<String>,
    /// Payment terms.
    #[serde(default)]
    #[schema(value_type = Option<Object>)]
    pub terms: Option<Value>,
    /// A sales rep of the company.
    #[serde(default)]
    pub salesperson_id: Option<i32>,
    /// Another customer of the company.
    #[serde(default)]
    pub customer_id: Option<i32>,
    /// Tax percent.
    #[serde(default)]
    pub tax_pct: Option<f64>,
    /// Discount percent.
    #[serde(default)]
    pub discount_pct: Option<f64>,
    /// Discount in dollars (not negative).
    #[serde(default)]
    pub discount_amount: Option<f64>,
    /// Expires on (YYYY-MM-DD).
    #[serde(default)]
    pub expiration_date: Option<String>,
    /// A price list: the quote takes its newest revision (the same list moves to its newest).
    #[serde(default)]
    pub price_list_id: Option<i32>,
}

#[allow(clippy::option_option, clippy::single_option_map)]
fn blank_is_null(value: Option<String>) -> Option<Option<String>> {
    value.map(|v| {
        let v = v.trim().to_string();
        (!v.is_empty()).then_some(v)
    })
}

/// Change a quote
///
/// Its info (name, notes, contact, terms, sales rep, customer), settings
/// (tax, discounts, expiration) and price list. A new price list or its
/// newest revision gives the quote today's prices on its next load.
#[utoipa::path(
    patch,
    path = "/v1/quotes/{id}",
    operation_id = "update_quote",
    tag = "quotes",
    params(("id" = i32, Path, description = "Quote id")),
    request_body = QuoteChanges,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Customer not found", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such quote", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
#[allow(
    clippy::too_many_lines,
    clippy::collapsible_if,
    clippy::needless_late_init
)]
pub async fn update_quote(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<QuoteChanges>,
) -> ApiResult<StatusCode> {
    let quote = quote_or_404(&state, id, user.company_id).await?;
    let mut sets: Vec<(&str, Value)> = Vec::new();
    if let Some(name) = input
        .quote_name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
    {
        sets.push(("quote_name", json!(name)));
    }
    for (column, value) in [
        ("notes", blank_is_null(input.notes)),
        ("address", blank_is_null(input.address)),
        ("phone", blank_is_null(input.phone)),
        ("email", blank_is_null(input.email)),
        ("address_notes", blank_is_null(input.address_notes)),
    ] {
        if let Some(value) = value {
            sets.push((column, value.map_or(Value::Null, Value::String)));
        }
    }
    if let Some(terms) = input.terms {
        sets.push(("payment_terms_json", Value::String(terms.to_string())));
    }
    if let Some(rep) = input.salesperson_id {
        if let Some(person) = sales_reps(&state.pool, user.company_id)
            .await?
            .into_iter()
            .find(|p| p.id == rep)
        {
            sets.push(("salesperson_id", json!(person.id)));
            sets.push(("sales_rep", json!(person.name)));
        }
    }
    let customer_id = if let Some(customer) = input
        .customer_id
        .filter(|c| *c != quote.customer_id && *c > 0)
    {
        let found: Option<i32> = sqlx::query_scalar(
            "SELECT id FROM customers WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
        )
        .bind(customer)
        .bind(user.company_id)
        .fetch_optional(&state.pool)
        .await?;
        let found = found.ok_or_else(|| ApiError::BadRequest("Customer not found.".into()))?;
        sets.push(("customer_id", json!(found)));
        found
    } else {
        quote.customer_id
    };
    if let Some(tax) = input.tax_pct.filter(|v| v.is_finite()) {
        sets.push(("tax_pct", json!(tax)));
    }
    if let Some(discount) = input.discount_pct.filter(|v| v.is_finite()) {
        sets.push(("discount_pct", json!(discount)));
    }
    if let Some(amount) = input.discount_amount.filter(|v| v.is_finite()) {
        sets.push(("discount_amount", json!(amount.max(0.0))));
    }
    if let Some(date) = input
        .expiration_date
        .as_deref()
        .filter(|d| d.len() == 10 && chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").is_ok())
    {
        sets.push(("expiration_date", json!(date)));
    }
    let mut reprice = false;
    if let Some(list_id) = input.price_list_id.filter(|l| *l > 0) {
        if let Some((list, _)) = find_price_list(&state.pool, list_id, user.company_id).await?
            && let Some(revision) = list.current_revision_id
        {
            if Some(list_id) != quote.price_list_id {
                sets.push(("price_list_id", json!(list.id)));
            }
            sets.push(("price_list_revision_id", json!(revision)));
            reprice = true;
        }
    }
    if let Some(phone_2) = blank_is_null(input.phone_2) {
        sqlx::query("UPDATE customers SET phone_2 = ? WHERE id = ? AND company_id = ?")
            .bind(phone_2)
            .bind(customer_id)
            .bind(user.company_id)
            .execute(&state.pool)
            .await?;
    }
    if !sets.is_empty() {
        let mut sql = sqlx::QueryBuilder::new("UPDATE quotes SET ");
        for (column, value) in sets {
            sql.push(column).push(" = ");
            match value {
                Value::Null => sql.push_bind(None::<String>),
                Value::String(text) => sql.push_bind(text),
                Value::Number(n) if n.is_i64() => sql.push_bind(n.as_i64()),
                Value::Number(n) => sql.push_bind(n.as_f64()),
                other => sql.push_bind(other.to_string()),
            };
            sql.push(", ");
        }
        sql.push("updated_at = NOW() WHERE id = ")
            .push_bind(id)
            .push(" AND company_id = ")
            .push_bind(user.company_id)
            .push(" AND deleted_at IS NULL");
        sql.build().execute(&state.pool).await?;
    }
    if reprice {
        sqlx::query("UPDATE quotes SET price_snapshot_json = NULL WHERE id = ?")
            .bind(id)
            .execute(&state.pool)
            .await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// A drawing to save.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SavedDrawing {
    /// The drawing (`QuoteDoc`).
    #[schema(value_type = Object)]
    pub doc: Value,
    /// The editor step to come back to.
    #[serde(default)]
    pub step: Option<i32>,
    /// What changed (up to 500 characters).
    #[serde(default)]
    pub notes: Option<String>,
}

/// The revision saved.
#[derive(Debug, Serialize, ToSchema)]
pub struct SavedRevision {
    /// Its number.
    pub revision: i64,
}

fn readable(doc: &Value) -> ApiResult<QuoteDoc> {
    QuoteDoc::parse(doc)
        .ok_or_else(|| ApiError::BadRequest("The drawing could not be read.".into()))
}

/// Save the drawing
///
/// A new immutable revision (with each option's total) that becomes the
/// quote's drawing; the caller's unsaved drawing is dropped. A quote not
/// signed yet cannot keep a sold-out color.
#[utoipa::path(
    put,
    path = "/v1/quotes/{id}/drawing",
    operation_id = "save_quote_drawing",
    tag = "quotes",
    params(("id" = i32, Path, description = "Quote id")),
    request_body = SavedDrawing,
    responses(
        (status = 200, description = "Saved", body = SavedRevision),
        (status = 400, description = "Not a drawing, or a sold-out color", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such quote", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn save_drawing(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<SavedDrawing>,
) -> ApiResult<Json<SavedRevision>> {
    let row = quote_or_404(&state, id, user.company_id).await?;
    let doc = readable(&input.doc)?;
    let quote = pricing_quote(&state, id, user.company_id).await?;
    let (price_list, _) = pricing_for_quote(&state.pool, &quote).await?;
    if row.signed_at.is_none() {
        let sold_out = sold_out_colors(&doc, &price_list);
        if !sold_out.is_empty() {
            return Err(ApiError::BadRequest(sold_out_message(&sold_out)));
        }
    }
    let totals: Vec<Value> =
        quote_pricing::pricing::price_quote(&doc, &price_list, quote.options())
            .into_iter()
            .map(|o| json!({ "optionId": o.option_id, "total": o.total, "missing": o.missing }))
            .collect();
    let notes: String = input
        .notes
        .unwrap_or_default()
        .trim()
        .chars()
        .take(500)
        .collect();
    let mut tx = state.pool.begin().await?;
    let latest: Option<i64> = sqlx::query_scalar(
        "SELECT CAST(MAX(revision_number) AS SIGNED) FROM quote_revisions WHERE quote_id = ? FOR UPDATE",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    let next = latest.unwrap_or(0) + 1;
    sqlx::query(
        "INSERT INTO quote_revisions
           (quote_id, revision_number, drawing_json, options_json, totals_json, price_list_revision_id, notes, created_by)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(next)
    .bind(input.doc.to_string())
    .bind(input.doc.get("colorOptions").cloned().unwrap_or(json!([])).to_string())
    .bind(Value::Array(totals).to_string())
    .bind(row.price_list_revision_id)
    .bind((!notes.is_empty()).then_some(notes))
    .bind(user.id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE quotes SET draft_drawing_json = ?, current_step = ?, current_revision = ?,
                status = IF(status = 'draft', 'active', status), updated_at = NOW()
          WHERE id = ? AND company_id = ?",
    )
    .bind(input.doc.to_string())
    .bind(input.step.filter(|s| *s > 0).unwrap_or(1))
    .bind(next)
    .bind(id)
    .bind(user.company_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM quote_autosaves WHERE quote_id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(SavedRevision { revision: next }))
}

/// An unsaved drawing.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UnsavedDrawing {
    /// The drawing (`QuoteDoc`).
    #[schema(value_type = Object)]
    pub doc: Value,
}

/// Keep an unsaved drawing
///
/// The caller's work in progress, offered back when they open the editor.
#[utoipa::path(
    put,
    path = "/v1/quotes/{id}/autosave",
    operation_id = "autosave_quote_drawing",
    tag = "quotes",
    params(("id" = i32, Path, description = "Quote id")),
    request_body = UnsavedDrawing,
    responses(
        (status = 204, description = "Kept"),
        (status = 400, description = "Not a drawing", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such quote", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn save_autosave(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<UnsavedDrawing>,
) -> ApiResult<StatusCode> {
    quote_or_404(&state, id, user.company_id).await?;
    readable(&input.doc)?;
    sqlx::query(
        "INSERT INTO quote_autosaves (quote_id, user_id, drawing_json, saved_at) VALUES (?, ?, ?, NOW())
         ON DUPLICATE KEY UPDATE drawing_json = VALUES(drawing_json), saved_at = NOW()",
    )
    .bind(id)
    .bind(user.id)
    .bind(input.doc.to_string())
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Drop the unsaved drawing
///
/// The caller's work in progress on this quote.
#[utoipa::path(
    delete,
    path = "/v1/quotes/{id}/autosave",
    operation_id = "discard_quote_autosave",
    tag = "quotes",
    params(("id" = i32, Path, description = "Quote id")),
    responses(
        (status = 204, description = "Dropped"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such quote", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn discard_autosave(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    quote_or_404(&state, id, user.company_id).await?;
    sqlx::query("DELETE FROM quote_autosaves WHERE quote_id = ? AND user_id = ?")
        .bind(id)
        .bind(user.id)
        .execute(&state.pool)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// A revision's drawing.
#[derive(Debug, Serialize, ToSchema)]
pub struct RevisionDrawing {
    /// The drawing.
    #[schema(value_type = Object)]
    pub doc: Value,
}

/// Get a revision's drawing
///
/// To load into the editor.
#[utoipa::path(
    get,
    path = "/v1/quotes/{id}/revisions/{revision_id}",
    operation_id = "get_quote_revision",
    tag = "quotes",
    params(
        ("id" = i32, Path, description = "Quote id"),
        ("revision_id" = i32, Path, description = "Revision id"),
    ),
    responses(
        (status = 200, description = "The drawing", body = RevisionDrawing),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such quote or revision", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn revision_drawing(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((id, revision_id)): Path<(i32, i32)>,
) -> ApiResult<Json<RevisionDrawing>> {
    quote_or_404(&state, id, user.company_id).await?;
    let doc = revision_doc(&state.pool, id, revision_id)
        .await?
        .filter(|d| QuoteDoc::parse(d).is_some())
        .ok_or(ApiError::NotFound)?;
    Ok(Json(RevisionDrawing { doc }))
}

/// Move a quote to today's prices
///
/// The price list's newest revision and today's prices; sinks placed from
/// a sink model take its current type and shape.
#[utoipa::path(
    post,
    path = "/v1/quotes/{id}/prices",
    operation_id = "update_quote_prices",
    tag = "quotes",
    params(("id" = i32, Path, description = "Quote id")),
    responses(
        (status = 204, description = "Updated"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such quote", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update_prices(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    let row = quote_or_404(&state, id, user.company_id).await?;
    if let Some(list_id) = row.price_list_id
        && let Some((list, _)) = find_price_list(&state.pool, list_id, user.company_id).await?
        && let Some(revision) = list.current_revision_id
        && Some(revision) != row.price_list_revision_id
    {
        sqlx::query(
            "UPDATE quotes SET price_list_revision_id = ?, updated_at = NOW() WHERE id = ?",
        )
        .bind(revision)
        .bind(id)
        .execute(&state.pool)
        .await?;
    }
    let quote = pricing_quote(&state, id, user.company_id).await?;
    let (_, latest) = pricing_for_quote(&state.pool, &quote).await?;
    sqlx::query("UPDATE quotes SET price_snapshot_json = ? WHERE id = ?")
        .bind(latest.to_string())
        .bind(id)
        .execute(&state.pool)
        .await?;
    if QuoteDoc::parse(&row.drawing()).is_some()
        && let Some(next) = with_current_sink_models(&row.drawing(), &latest)
    {
        sqlx::query("UPDATE quotes SET draft_drawing_json = ?, updated_at = NOW() WHERE id = ?")
            .bind(next.to_string())
            .bind(id)
            .execute(&state.pool)
            .await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// The option sold.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Sale {
    /// Index of the color option sold.
    pub option: i32,
}

/// What selling did.
#[derive(Debug, Serialize, ToSchema)]
pub struct SaleResult {
    /// The deal marked won (none: the quote has no sales rep to make one).
    pub deal_id: Option<i64>,
}

/// Mark a quote sold
///
/// The quote keeps the option sold and its deal is won with that option's
/// total (a won deal is made when the customer has none).
#[utoipa::path(
    post,
    path = "/v1/quotes/{id}/sale",
    operation_id = "mark_quote_sold",
    tag = "quotes",
    params(("id" = i32, Path, description = "Quote id")),
    request_body = Sale,
    responses(
        (status = 200, description = "Sold", body = SaleResult),
        (status = 400, description = "Not an option of the quote", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such quote", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn mark_sold(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<Sale>,
) -> ApiResult<Json<SaleResult>> {
    if input.option < 0 {
        return Err(ApiError::BadRequest(
            "Pick the option that was sold.".into(),
        ));
    }
    let sold = mark_quote_sold(
        &state.pool,
        id,
        user.company_id,
        Some(input.option),
        Some(user.id),
    )
    .await?
    .ok_or(ApiError::NotFound)?;
    Ok(Json(SaleResult {
        deal_id: sold.deal_id,
    }))
}

/// Undo a sale
///
/// The quote is active again (no option, no signature) and its deal reopens.
#[utoipa::path(
    delete,
    path = "/v1/quotes/{id}/sale",
    operation_id = "undo_quote_sale",
    tag = "quotes",
    params(("id" = i32, Path, description = "Quote id")),
    responses(
        (status = 204, description = "Undone"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such quote", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn undo_sale(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    let row = quote_or_404(&state, id, user.company_id).await?;
    sqlx::query(
        "UPDATE quotes SET status = 'active', customer_accepted_option_id = NULL,
                signed_at = NULL, signature_data = NULL
          WHERE id = ? AND company_id = ?",
    )
    .bind(id)
    .bind(user.company_id)
    .execute(&state.pool)
    .await?;
    if let Some(deal) = row.deal_id {
        sqlx::query(
            "UPDATE deals SET is_won = NULL, lost_reason = NULL, updated_at = CURRENT_TIMESTAMP
              WHERE id = ? AND is_won = 1 AND deleted_at IS NULL",
        )
        .bind(deal)
        .execute(&state.pool)
        .await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// A copy of a quote.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QuoteCopy {
    /// The copy's name (required).
    pub quote_name: String,
    /// Type; blank keeps the quote's.
    #[serde(default)]
    pub quote_type: Option<String>,
    /// Another price list (its newest revision); none keeps the quote's.
    #[serde(default)]
    pub price_list_id: Option<i32>,
    /// Another sales rep; none keeps the quote's.
    #[serde(default)]
    pub salesperson_id: Option<i32>,
    /// Copy the address, phone, email and address notes.
    #[serde(default)]
    pub copy_address: bool,
    /// Copy the drawing.
    #[serde(default)]
    pub copy_drawing: bool,
}

/// Duplicate a quote
///
/// A new draft for the same customer and deal with the quote's notes, tax,
/// discounts and terms, optionally its address and drawing.
#[utoipa::path(
    post,
    path = "/v1/quotes/{id}/copies",
    operation_id = "copy_quote",
    tag = "quotes",
    params(("id" = i32, Path, description = "Quote id")),
    request_body = QuoteCopy,
    responses(
        (status = 201, description = "Created", body = CreatedId),
        (status = 400, description = "No name", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such quote", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn copy_quote(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<QuoteCopy>,
) -> ApiResult<(StatusCode, Json<CreatedId>)> {
    let quote = quote_or_404(&state, id, user.company_id).await?;
    let name = input.quote_name.trim();
    if name.is_empty() {
        return Err(ApiError::BadRequest("Quote name is required.".into()));
    }
    let list = match input.price_list_id.filter(|l| *l > 0) {
        Some(list) => find_price_list(&state.pool, list, user.company_id)
            .await?
            .map(|(row, _)| row)
            .filter(|row| row.current_revision_id.is_some()),
        None => None,
    };
    let rep = match input.salesperson_id {
        Some(rep) => sales_reps(&state.pool, user.company_id)
            .await?
            .into_iter()
            .find(|p| p.id == rep),
        None => None,
    };
    let quote_type = input
        .quote_type
        .filter(|t| !t.is_empty())
        .or_else(|| quote.quote_type.clone())
        .unwrap_or_default();
    let copy = input.copy_address;
    let new_id = sqlx::query(
        "INSERT INTO quotes (customer_id, quote_name, quote_type, created_date, sales_rep, company_id,
                             salesperson_id, created_by, price_list_id, price_list_revision_id,
                             expiration_date, deal_id, draft_drawing_json, notes, tax_pct, discount_pct,
                             discount_amount, payment_terms_json, address, phone, email, address_notes,
                             status, updated_at)
         SELECT customer_id, ?, ?, NOW(), ?, company_id, ?, ?, ?, ?, expiration_date, deal_id,
                IF(?, draft_drawing_json, NULL), notes, tax_pct, discount_pct, discount_amount,
                payment_terms_json, IF(?, address, NULL), IF(?, phone, NULL), IF(?, email, NULL),
                IF(?, address_notes, NULL), 'draft', NOW()
           FROM quotes WHERE id = ? AND company_id = ?",
    )
    .bind(name)
    .bind(quote_type)
    .bind(rep.as_ref().and_then(|r| r.name.clone()).or_else(|| quote.sales_rep.clone()))
    .bind(rep.as_ref().map(|r| r.id).or(quote.salesperson_id))
    .bind(user.id)
    .bind(list.as_ref().map(|l| l.id).or(quote.price_list_id))
    .bind(list.as_ref().and_then(|l| l.current_revision_id).or(quote.price_list_revision_id))
    .bind(input.copy_drawing)
    .bind(copy)
    .bind(copy)
    .bind(copy)
    .bind(copy)
    .bind(id)
    .bind(user.company_id)
    .execute(&state.pool)
    .await?
    .last_insert_id();
    Ok((
        StatusCode::CREATED,
        Json(CreatedId {
            id: i64::try_from(new_id).unwrap_or_default(),
        }),
    ))
}

/// 24 random bytes as hex (`generateToken`).
fn token() -> ApiResult<String> {
    let mut bytes = [0u8; 24];
    getrandom::fill(&mut bytes).map_err(|_| ApiError::Internal("no randomness".into()))?;
    Ok(bytes.iter().fold(String::new(), |mut out, b| {
        use std::fmt::Write;
        let _ = write!(out, "{b:02x}");
        out
    }))
}

/// `ensureQuotePortalToken`.
pub async fn ensure_portal_token(pool: &MySqlPool, quote: &QuoteRow) -> ApiResult<String> {
    if let Some(token) = quote.portal_token.clone().filter(|t| !t.is_empty()) {
        return Ok(token);
    }
    let token = token()?;
    sqlx::query("UPDATE quotes SET portal_token = ? WHERE id = ? AND company_id = ?")
        .bind(&token)
        .bind(quote.id)
        .bind(quote.company_id)
        .execute(pool)
        .await?;
    Ok(token)
}

/// The customer link's token.
#[derive(Debug, Serialize, ToSchema)]
pub struct PortalToken {
    /// Open `/quote-portal/{token}`.
    pub token: String,
}

/// Get the customer link
///
/// Made the first time it is asked for.
#[utoipa::path(
    post,
    path = "/v1/quotes/{id}/portal-token",
    operation_id = "quote_portal_token",
    tag = "quotes",
    params(("id" = i32, Path, description = "Quote id")),
    responses(
        (status = 200, description = "The token", body = PortalToken),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such quote", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn portal_token(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<PortalToken>> {
    let quote = quote_or_404(&state, id, user.company_id).await?;
    Ok(Json(PortalToken {
        token: ensure_portal_token(&state.pool, &quote).await?,
    }))
}

/// `moveQuoteCustomerToGetAQuote`: after a quote is emailed or printed, its
/// deal moves to "Got a Quote" when still Not Contacted Yet or Contacted.
async fn move_deal_to_got_a_quote(pool: &MySqlPool, quote: &QuoteRow) -> ApiResult<()> {
    let lists: Vec<(i32, String, i32)> = sqlx::query_as(
        "SELECT dl.id, dl.name, dl.group_id FROM deals_list dl
           JOIN groups_list g ON g.id = dl.group_id AND g.deleted_at IS NULL
          WHERE dl.deleted_at IS NULL AND (g.company_id = ? OR g.id = 1)",
    )
    .bind(quote.company_id)
    .fetch_all(pool)
    .await?;
    let targets: Vec<&(i32, String, i32)> = lists
        .iter()
        .filter(|l| lower(&l.1) == "got a quote")
        .collect();
    if targets.is_empty() {
        return Ok(());
    }
    let deals: Vec<(i64, i32, Option<String>, Option<i32>)> = sqlx::query_as(
        "SELECT CAST(d.id AS SIGNED), d.list_id, dl.name, dl.group_id
           FROM deals d
           JOIN customers c ON c.id = d.customer_id
           LEFT JOIN deals_list dl ON dl.id = d.list_id AND dl.deleted_at IS NULL
          WHERE d.deleted_at IS NULL AND d.is_won IS NULL AND c.company_id = ? AND d.customer_id = ?
            AND (? IS NULL OR d.id = ?)",
    )
    .bind(quote.company_id)
    .bind(quote.customer_id)
    .bind(quote.deal_id)
    .bind(quote.deal_id)
    .fetch_all(pool)
    .await?;
    for (deal, list_id, list_name, group) in deals {
        let stage = lower(list_name.as_deref().unwrap_or_default());
        if stage != "not contacted yet" && stage != "contacted" {
            continue;
        }
        let Some(target) = targets.iter().find(|t| Some(t.2) == group) else {
            continue;
        };
        if target.0 == list_id {
            continue;
        }
        let moved = sqlx::query(
            "UPDATE deals SET list_id = ?, status = ?, updated_at = NOW()
              WHERE id = ? AND deleted_at IS NULL AND is_won IS NULL AND list_id = ?",
        )
        .bind(target.0)
        .bind(&target.1)
        .bind(deal)
        .bind(list_id)
        .execute(pool)
        .await?
        .rows_affected();
        if moved > 0 {
            crate::routes::deals::service::transition_stage(pool, deal, target.0).await?;
        }
    }
    Ok(())
}

/// An email with the quote's link.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QuoteEmail {
    /// Recipients (required).
    pub to: Vec<String>,
    /// Copies.
    #[serde(default)]
    pub cc: Vec<String>,
    /// Blind copies.
    #[serde(default)]
    pub bcc: Vec<String>,
    /// Subject (required); `{{variables}}` are filled in.
    pub subject: String,
    /// Plain text; `{{variables}}` are filled in and `[QuoteLink]`/`[PDFLink]`
    /// become the link (added at the end when missing).
    pub body: String,
    /// A company form the link shows the quote in.
    #[serde(default)]
    pub form_id: Option<i32>,
    /// The template used (counts toward the default).
    #[serde(default)]
    pub template_id: Option<i32>,
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn link_tags(text: &str, with: &str) -> String {
    text.replace("[QuoteLink]", with).replace("[PDFLink]", with)
}

/// Email the quote
///
/// Sends the customer a link to the quote (from the caller's sending
/// address, replies to the caller), records it with the revision and
/// expiration, and moves the deal to "Got a Quote".
#[utoipa::path(
    post,
    path = "/v1/quotes/{id}/emails",
    operation_id = "email_quote",
    tag = "quotes",
    params(("id" = i32, Path, description = "Quote id")),
    request_body = QuoteEmail,
    responses(
        (status = 204, description = "Sent"),
        (status = 400, description = "No recipient or subject", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such quote", body = ErrorBody),
        (status = 502, description = "The email could not be sent", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
#[allow(clippy::too_many_lines)]
pub async fn email_quote(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<QuoteEmail>,
) -> ApiResult<StatusCode> {
    let quote = quote_or_404(&state, id, user.company_id).await?;
    let clean = |list: Vec<String>| -> Vec<String> {
        list.iter()
            .flat_map(|v| v.split([',', ';', ' ', '\n', '\t']))
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
            .collect()
    };
    let (to, cc, bcc) = (clean(input.to), clean(input.cc), clean(input.bcc));
    let mut subject = input.subject.trim().to_string();
    let mut body = input.body;
    if to.is_empty() || subject.is_empty() {
        return Err(ApiError::BadRequest(
            "Recipient and subject are required.".into(),
        ));
    }
    if subject.contains("{{") || body.contains("{{") {
        let data = fetch_template_variable_data(
            &state.pool,
            user.id,
            quote.deal_id.and_then(|d| i32::try_from(d).ok()),
            Some(quote.customer_id),
            user.company_id,
        )
        .await
        .map_err(|_| {
            ApiError::Upstream("Could not fill the template variables. Try again.".into())
        })?;
        subject = replace_template_variables(&subject, &data);
        body = replace_template_variables(&body, &data);
    }
    let form = match input.form_id {
        Some(form) => quote_forms::summaries(&state.pool, user.company_id, false)
            .await?
            .into_iter()
            .find(|f| f.id == form)
            .map(|f| f.id.to_string()),
        None => None,
    };
    let portal = ensure_portal_token(&state.pool, &quote).await?;
    let email_token = token()?;
    sqlx::query(
        "INSERT INTO quote_emails (quote_id, revision_number, token, recipients, subject, form, sent_by, expires_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(quote.current_revision.unwrap_or(0))
    .bind(&email_token)
    .bind(to.join(", "))
    .bind(&subject)
    .bind(form)
    .bind(user.id)
    .bind(quote.expiration_date.as_deref())
    .execute(&state.pool)
    .await?;
    let subdomain = company_subdomain(&state.pool, user.company_id).await?;
    let origin = app_origin(&state.app_url, subdomain.as_deref());
    let link = format!("{origin}/quote-portal/{portal}?e={email_token}");
    let anchor = format!("<a href=\"{link}\">View your quote</a>");
    let html = link_tags(&escape(&body).replace('\n', "<br/>"), &anchor);
    let html = if html.contains(&link) {
        html
    } else {
        format!("{html}<br/><br/>{anchor}")
    };
    let template = match input.template_id {
        Some(template) => {
            sqlx::query_scalar::<_, i32>(
                "SELECT id FROM quote_email_templates WHERE id = ? AND company_id = ?
               AND deleted_at IS NULL AND (user_id IS NULL OR user_id = ?)",
            )
            .bind(template)
            .bind(user.company_id)
            .bind(user.id)
            .fetch_optional(&state.pool)
            .await?
        }
        None => None,
    };
    let from = sending_address(&state.pool, user.company_id, &user.email).await?;
    let email = OutgoingEmail {
        from_name: None,
        from_address: from,
        to,
        cc,
        bcc,
        subject,
        body: html.clone(),
        signature: None,
        reply_to: vec![user.email.clone()],
        in_reply_to: None,
        references: Vec::new(),
        attachments: Vec::new(),
        transactional: true,
    };
    if let Err(error) = state.webhooks.send_email(&email).await {
        tracing::warn!(?error, quote = id, "quote email failed");
        sqlx::query("DELETE FROM quote_emails WHERE token = ?")
            .bind(&email_token)
            .execute(&state.pool)
            .await?;
        return Err(ApiError::Upstream(
            "Could not send the email. Try again.".into(),
        ));
    }
    sqlx::query("UPDATE quote_emails SET body_html = ?, template_id = ? WHERE token = ?")
        .bind(&html)
        .bind(template)
        .bind(&email_token)
        .execute(&state.pool)
        .await?;
    // The email already went out: a stage update must not turn that into a failed send.
    if let Err(error) = move_deal_to_got_a_quote(&state.pool, &quote).await {
        tracing::error!(?error, quote = id, "could not move the deal to Got a Quote");
    }
    Ok(StatusCode::NO_CONTENT)
}

/// An email link on or off.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct EmailLink {
    /// Whether the link opens the quote.
    pub active: bool,
}

/// Switch an email's link off or on
///
/// The customer link from that email stops (or starts again) opening the quote.
#[utoipa::path(
    patch,
    path = "/v1/quotes/{id}/emails/{email_id}",
    operation_id = "set_quote_email_link",
    tag = "quotes",
    params(
        ("id" = i32, Path, description = "Quote id"),
        ("email_id" = i32, Path, description = "Quote email id"),
    ),
    request_body = EmailLink,
    responses(
        (status = 204, description = "Saved"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such quote", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn set_email_link(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((id, email_id)): Path<(i32, i32)>,
    JsonBody(input): JsonBody<EmailLink>,
) -> ApiResult<StatusCode> {
    quote_or_404(&state, id, user.company_id).await?;
    sqlx::query(
        "UPDATE quote_emails SET deactivated_at = IF(?, NULL, NOW()) WHERE id = ? AND quote_id = ?",
    )
    .bind(input.active)
    .bind(email_id)
    .bind(id)
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// A print of the quote.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QuotePrint {
    /// The form printed, as the deal's history names it.
    #[serde(default)]
    pub form_label: String,
}

/// Record a print
///
/// Shown in the deal's history as "Quote printed"; the deal moves to "Got a
/// Quote". Never blocks the print.
#[utoipa::path(
    post,
    path = "/v1/quotes/{id}/prints",
    operation_id = "record_quote_print",
    tag = "quotes",
    params(("id" = i32, Path, description = "Quote id")),
    request_body = QuotePrint,
    responses(
        (status = 204, description = "Recorded"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such quote", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn record_print(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<QuotePrint>,
) -> ApiResult<StatusCode> {
    let quote = quote_or_404(&state, id, user.company_id).await?;
    let label: String = input.form_label.trim().chars().take(100).collect();
    if let Err(error) = sqlx::query(
        "INSERT INTO quote_prints (quote_id, revision_number, form, printed_by) VALUES (?, ?, ?, ?)",
    )
    .bind(id)
    .bind(quote.current_revision.unwrap_or(1))
    .bind((!label.is_empty()).then_some(label))
    .bind(user.id)
    .execute(&state.pool)
    .await
    {
        tracing::error!(?error, quote = id, "failed to record a quote print");
    }
    if let Err(error) = move_deal_to_got_a_quote(&state.pool, &quote).await {
        tracing::error!(?error, quote = id, "could not move the deal to Got a Quote");
    }
    Ok(StatusCode::NO_CONTENT)
}

/// A template of the Email dialog.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct EmailTemplate {
    /// Template id.
    pub id: i32,
    /// The caller's own (only they see it).
    pub personal: bool,
    /// Name.
    pub name: String,
    /// Subject.
    pub subject: String,
    /// Body.
    pub body: String,
    /// Order.
    pub sequence: i32,
}

/// The Email dialog's templates.
#[derive(Debug, Serialize, ToSchema)]
pub struct MyEmailTemplates {
    /// The company's and the caller's own, in the caller's order.
    pub templates: Vec<EmailTemplate>,
    /// `automatic` (most used first) or `custom` (the caller's drag order).
    pub ordering_mode: String,
    /// The one the dialog starts with: what the caller sends most, else the
    /// company's most used, else the first.
    pub default_id: Option<i32>,
}

/// List the Email dialog's templates
///
/// The company's quote email templates and the caller's own, in the
/// caller's order, with the one to start with.
#[utoipa::path(
    get,
    path = "/v1/me/quote-email-templates",
    operation_id = "list_my_quote_email_templates",
    tag = "quotes",
    responses(
        (status = 200, description = "The templates", body = MyEmailTemplates),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn my_email_templates(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<MyEmailTemplates>> {
    let mut templates: Vec<EmailTemplate> = sqlx::query_as(
        "SELECT id, user_id IS NOT NULL AS personal, name, subject, body, sequence
           FROM quote_email_templates
          WHERE company_id = ? AND deleted_at IS NULL AND (user_id IS NULL OR user_id = ?)
          ORDER BY user_id IS NOT NULL, sequence, name",
    )
    .bind(user.company_id)
    .bind(user.id)
    .fetch_all(&state.pool)
    .await?;
    let position: HashMap<i32, usize> = templates
        .iter()
        .enumerate()
        .map(|(i, t)| (t.id, i))
        .collect();
    let context = order::context(&state.pool, user.id, OrderingType::Quote).await?;
    order::sort(
        &mut templates,
        |t| t.id,
        &context,
        |a, b| position[&a.id].cmp(&position[&b.id]),
    );
    let default_id = if templates.is_empty() {
        None
    } else {
        let usage: Vec<(Option<i32>,)> = sqlx::query_as(
            "SELECT e.template_id FROM quote_emails e JOIN quotes q ON q.id = e.quote_id
              WHERE q.company_id = ? AND e.template_id IS NOT NULL
              GROUP BY e.template_id ORDER BY SUM(e.sent_by = ?) DESC, COUNT(*) DESC",
        )
        .bind(user.company_id)
        .bind(user.id)
        .fetch_all(&state.pool)
        .await?;
        usage
            .into_iter()
            .filter_map(|(id,)| id)
            .find(|id| templates.iter().any(|t| t.id == *id))
            .or_else(|| templates.first().map(|t| t.id))
    };
    Ok(Json(MyEmailTemplates {
        ordering_mode: match context.mode {
            OrderingMode::Custom => "custom",
            OrderingMode::Automatic => "automatic",
        }
        .into(),
        templates,
        default_id,
    }))
}

/// One of the caller's own templates.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct MyTemplateInput {
    /// Name (required, up to 120 characters).
    pub name: String,
    /// Subject (required, up to 255 characters).
    pub subject: String,
    /// Body; `[PDFLink]` is added when it has no link.
    #[serde(default)]
    pub body: String,
}

/// `withPdfLink`.
fn with_pdf_link(body: &str) -> String {
    if body.contains("[QuoteLink]") || body.contains("[PDFLink]") {
        return body.to_string();
    }
    let text = body.trim_end();
    if text.is_empty() {
        "[PDFLink]".into()
    } else {
        format!("{text}\n\n[PDFLink]")
    }
}

fn my_template(input: &MyTemplateInput) -> ApiResult<(String, String, String)> {
    let name: String = input.name.trim().chars().take(120).collect();
    let subject: String = input.subject.trim().chars().take(255).collect();
    if name.is_empty() {
        return Err(ApiError::BadRequest("Name the template to save it.".into()));
    }
    if subject.is_empty() {
        return Err(ApiError::BadRequest(
            "Add a subject before saving the template.".into(),
        ));
    }
    Ok((name, subject, with_pdf_link(&input.body)))
}

/// Save a template of my own
///
/// Only the caller sees it.
#[utoipa::path(
    post,
    path = "/v1/me/quote-email-templates",
    operation_id = "create_my_quote_email_template",
    tag = "quotes",
    request_body = MyTemplateInput,
    responses(
        (status = 201, description = "Saved", body = CreatedId),
        (status = 400, description = "No name or subject", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create_my_email_template(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<MyTemplateInput>,
) -> ApiResult<(StatusCode, Json<CreatedId>)> {
    let (name, subject, body) = my_template(&input)?;
    let id = sqlx::query(
        "INSERT INTO quote_email_templates (company_id, user_id, name, subject, body, sequence, created_by)
         VALUES (?, ?, ?, ?, ?, 1, ?)",
    )
    .bind(user.company_id)
    .bind(user.id)
    .bind(name)
    .bind(subject)
    .bind(body)
    .bind(user.id)
    .execute(&state.pool)
    .await?
    .last_insert_id();
    Ok((
        StatusCode::CREATED,
        Json(CreatedId {
            id: i64::try_from(id).unwrap_or_default(),
        }),
    ))
}

/// Edit a template of my own
///
/// Never a company template.
#[utoipa::path(
    put,
    path = "/v1/me/quote-email-templates/{id}",
    operation_id = "update_my_quote_email_template",
    tag = "quotes",
    params(("id" = i32, Path, description = "Template id")),
    request_body = MyTemplateInput,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "No name or subject", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "Not one of the caller's templates", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update_my_email_template(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<MyTemplateInput>,
) -> ApiResult<StatusCode> {
    let (name, subject, body) = my_template(&input)?;
    let updated = sqlx::query(
        "UPDATE quote_email_templates SET name = ?, subject = ?, body = ?
          WHERE id = ? AND company_id = ? AND user_id = ? AND deleted_at IS NULL",
    )
    .bind(name)
    .bind(subject)
    .bind(body)
    .bind(id)
    .bind(user.company_id)
    .bind(user.id)
    .execute(&state.pool)
    .await?
    .rows_affected();
    if updated == 0 {
        let exists: Option<i32> = sqlx::query_scalar(
            "SELECT id FROM quote_email_templates WHERE id = ? AND company_id = ? AND user_id = ? AND deleted_at IS NULL",
        )
        .bind(id)
        .bind(user.company_id)
        .bind(user.id)
        .fetch_optional(&state.pool)
        .await?;
        if exists.is_none() {
            return Err(ApiError::NotFound);
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Delete a template of my own
///
/// Never a company template.
#[utoipa::path(
    delete,
    path = "/v1/me/quote-email-templates/{id}",
    operation_id = "delete_my_quote_email_template",
    tag = "quotes",
    params(("id" = i32, Path, description = "Template id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_my_email_template(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    sqlx::query(
        "UPDATE quote_email_templates SET deleted_at = NOW()
          WHERE id = ? AND company_id = ? AND user_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(user.company_id)
    .bind(user.id)
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}
