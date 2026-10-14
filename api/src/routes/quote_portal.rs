//! The customer's quote link (no session: the token in the link is the key).
//!
//! - `GET  /v1/public/quote-portal/{token}?e=`      → the quote as the customer sees it
//! - `POST /v1/public/quote-portal/{token}/acceptance` → accept an option and sign
//!
//! Prices are worked out here: the customer never gets the price list. An
//! emailed link (`?e=`) counts as a view and shows the quote in the form it
//! was sent with. Port of the `quote-portal.$token` route.

use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::quote_pricing::doc::QuoteDoc;
use crate::quote_pricing::pricing::{OptionPrice, price_quote};
use crate::quote_pricing::{
    customer_doc, load_company_quote, mark_quote_sold, option_sold_out_colors, pricing_for_quote,
};
use crate::routes::auth::{app_origin, company_subdomain};
use crate::routes::quote_editor::{QUOTE_ROW_SELECT, QuoteRow, StoneImage};
use crate::routes::quote_forms;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(portal_quote))
        .routes(routes!(accept_quote))
}

async fn by_token(pool: &MySqlPool, token: &str) -> ApiResult<QuoteRow> {
    if token.trim().is_empty() {
        return Err(ApiError::NotFound);
    }
    let row: Option<QuoteRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{QUOTE_ROW_SELECT} WHERE q.portal_token = ? AND q.deleted_at IS NULL LIMIT 1"
    )))
    .bind(token)
    .fetch_optional(pool)
    .await?;
    row.ok_or(ApiError::NotFound)
}

/// `quoteLinkActive`: an emailed link opens while that email's link is on;
/// a link without one while any of the quote's email links is on.
async fn link_active(pool: &MySqlPool, quote_id: i32, email: Option<&str>) -> ApiResult<bool> {
    let rows: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT token, DATE_FORMAT(deactivated_at, '%Y-%m-%d') FROM quote_emails WHERE quote_id = ?",
    )
    .bind(quote_id)
    .fetch_all(pool)
    .await?;
    if let Some(mine) = email.and_then(|e| rows.iter().find(|r| r.0 == e)) {
        return Ok(mine.1.is_none());
    }
    Ok(rows.is_empty() || rows.iter().any(|r| r.1.is_none()))
}

/// `?e=`: the email the link came from.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct EmailToken {
    /// The quote email's token.
    #[serde(default)]
    pub e: Option<String>,
}

/// The quote's own fields the customer sees.
#[derive(Debug, Serialize, ToSchema)]
pub struct PortalQuoteInfo {
    /// Quote id.
    pub id: i32,
    /// Its name.
    pub name: String,
    /// When the customer signed (ISO 8601).
    pub signed_at: Option<String>,
    /// The option accepted.
    pub accepted: Option<i32>,
    /// The signature (a PNG data URL).
    pub signature_data: Option<String>,
    /// Expires on (YYYY-MM-DD).
    pub expiration_date: Option<String>,
    /// It has expired.
    pub expired: bool,
}

/// A quote as the customer sees it.
#[derive(Debug, Serialize, ToSchema)]
pub struct PortalQuote {
    /// The link was switched off: only this is set.
    pub inactive: bool,
    /// The quote.
    pub quote: Option<PortalQuoteInfo>,
    /// The company's public stone catalog.
    pub inventory_url: Option<String>,
    /// The drawing, without supplier costs.
    #[schema(value_type = Object)]
    pub doc: Value,
    /// Each option priced.
    #[schema(value_type = Vec<Object>)]
    pub prices: Vec<OptionPrice>,
    /// Per option: it has a sold-out color and cannot be accepted.
    pub sold_out: Vec<bool>,
    /// The company form to show the quote in (`{config, data}`), if any.
    #[schema(value_type = Option<Object>)]
    pub form_view: Option<Value>,
    /// Photos of the quote's colors.
    pub stone_images: Vec<StoneImage>,
}

/// `loadQuoteFormData`: what a printed form shows for the quote.
async fn form_data(
    pool: &MySqlPool,
    row: &QuoteRow,
    prices: &[OptionPrice],
    doc: &Value,
) -> ApiResult<Value> {
    let letterhead = quote_forms::load_letterhead(pool, row.company_id).await?;
    let terms = row
        .payment_terms_json
        .as_deref()
        .and_then(|t| serde_json::from_str::<Value>(t).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    Ok(json!({
        "quote": {
            "name": row.display_name(),
            "quoteType": row.quote_type,
            "created": row.created_date,
            "expiration": row.expiration_date,
            "salesRep": row.sales_rep,
            "salesRepPhone": row.salesperson_phone,
            "customerName": row.customer_name,
            "address": row.address,
            "phone": row.phone,
            "phone2": row.customer_phone_2,
            "email": row.email,
            "addressNotes": row.address_notes,
            "notes": row.notes,
            "priceList": row.price_list_name,
            "createdBy": row.created_by_name,
            "status": row.status,
            "revision": row.current_revision,
        },
        "company": letterhead,
        "terms": terms,
        "doc": doc,
        "taxPct": row.tax_pct,
        "prices": prices,
    }))
}

/// Open a customer's quote link
///
/// The quote with its prices, drawing (no supplier costs), color photos
/// and, when it was emailed with a company form, that form. An emailed
/// link (`e`) counts as a view. A switched-off link answers `inactive`.
#[utoipa::path(
    get,
    path = "/v1/public/quote-portal/{token}",
    operation_id = "get_quote_portal",
    tag = "quotes",
    params(("token" = String, Path, description = "The quote's link token"), EmailToken),
    responses(
        (status = 200, description = "The quote", body = PortalQuote),
        (status = 404, description = "No such quote", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    )
)]
#[allow(clippy::too_many_lines, clippy::single_match_else)]
pub async fn portal_quote(
    State(state): State<AppState>,
    Path(token): Path<String>,
    Query(query): Query<EmailToken>,
) -> ApiResult<Json<PortalQuote>> {
    let row = by_token(&state.pool, &token).await?;
    let email = query.e.as_deref().filter(|e| !e.is_empty());
    if !link_active(&state.pool, row.id, email).await? {
        return Ok(Json(PortalQuote {
            inactive: true,
            quote: None,
            inventory_url: None,
            doc: Value::Null,
            prices: Vec::new(),
            sold_out: Vec::new(),
            form_view: None,
            stone_images: Vec::new(),
        }));
    }
    // An emailed link counts as a view.
    let mut viewed_form: Option<String> = None;
    if let Some(email) = email {
        let found: Option<(i32, i32, Option<String>)> =
            sqlx::query_as("SELECT id, quote_id, form FROM quote_emails WHERE token = ?")
                .bind(email)
                .fetch_optional(&state.pool)
                .await?;
        if let Some((id, quote_id, form)) = found {
            sqlx::query(
                "UPDATE quote_emails SET view_count = view_count + 1, last_viewed_at = NOW(),
                        first_viewed_at = COALESCE(first_viewed_at, NOW())
                  WHERE id = ?",
            )
            .bind(id)
            .execute(&state.pool)
            .await?;
            if quote_id == row.id {
                viewed_form = form.filter(|f| !f.is_empty());
            }
        }
    }
    // The email's form, else the latest emailed form, else the company's first quote form.
    let form_id = match viewed_form {
        Some(form) => Some(form),
        None => {
            let latest: Option<Option<String>> = sqlx::query_scalar(
                "SELECT form FROM quote_emails WHERE quote_id = ? AND form IS NOT NULL AND form != ''
                  ORDER BY id DESC LIMIT 1",
            )
            .bind(row.id)
            .fetch_optional(&state.pool)
            .await?;
            match latest.flatten() {
                Some(form) => Some(form),
                None => quote_forms::summaries(&state.pool, row.company_id, true)
                    .await?
                    .into_iter()
                    .find(|f| f.kind == "quote")
                    .map(|f| f.id.to_string()),
            }
        }
    };
    let quote = load_company_quote(&state.pool, row.id, row.company_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let (price_list, _) = pricing_for_quote(&state.pool, &quote).await?;
    let stored = row.drawing();
    let typed = QuoteDoc::from_stored(&stored);
    let doc = if QuoteDoc::parse(&stored).is_some() {
        customer_doc(&stored)
    } else {
        Value::Null
    };
    let prices = price_quote(&typed, &price_list, quote.options());
    let sold_out: Vec<bool> = (0..typed.color_options.len())
        .map(|i| {
            row.signed_at.is_none() && !option_sold_out_colors(&typed, &price_list, i).is_empty()
        })
        .collect();
    let form_view = match form_id.and_then(|f| f.parse::<i32>().ok()) {
        Some(id) => match quote_forms::find(&state.pool, row.company_id, id).await? {
            Some(form) => Some(json!({
                "config": form.config,
                "data": form_data(&state.pool, &row, &prices, &doc).await?,
            })),
            None => None,
        },
        None => None,
    };
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let expired = row
        .expiration_date
        .as_deref()
        .is_some_and(|d| d < today.as_str());
    let inventory_url = company_subdomain(&state.pool, row.company_id)
        .await?
        .map(|sub| {
            format!(
                "{}/customer/{}/stones",
                app_origin(&state.app_url, Some(&sub)),
                row.company_id
            )
        });
    Ok(Json(PortalQuote {
        inactive: false,
        quote: Some(PortalQuoteInfo {
            id: row.id,
            name: row.display_name(),
            signed_at: row.signed_at.clone(),
            accepted: row.customer_accepted_option_id,
            signature_data: row.signature_data.clone(),
            expiration_date: row.expiration_date.clone(),
            expired,
        }),
        inventory_url,
        stone_images: crate::routes::quote_editor::stone_images(
            &state.pool,
            row.company_id,
            &stored,
        )
        .await?,
        doc,
        prices,
        sold_out,
        form_view,
    }))
}

/// The customer's acceptance.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Acceptance {
    /// Index of the option accepted.
    pub option: i32,
    /// The signature (a PNG data URL).
    pub signature: String,
    /// The email the link came from.
    #[serde(default)]
    pub e: Option<String>,
}

/// Accept and sign a quote
///
/// Signing is a sale: the quote is sold with the option and its deal won.
/// Refused once signed, for an option with a sold-out color, or when the
/// link is switched off.
#[utoipa::path(
    post,
    path = "/v1/public/quote-portal/{token}/acceptance",
    operation_id = "accept_quote_portal",
    tag = "quotes",
    params(("token" = String, Path, description = "The quote's link token")),
    request_body = Acceptance,
    responses(
        (status = 204, description = "Signed"),
        (status = 400, description = "No such option, no signature, or a sold-out color", body = ErrorBody),
        (status = 404, description = "No such quote", body = ErrorBody),
        (status = 409, description = "Already signed, or the link is off", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    )
)]
pub async fn accept_quote(
    State(state): State<AppState>,
    Path(token): Path<String>,
    JsonBody(input): JsonBody<Acceptance>,
) -> ApiResult<StatusCode> {
    let row = by_token(&state.pool, &token).await?;
    if !link_active(
        &state.pool,
        row.id,
        input.e.as_deref().filter(|e| !e.is_empty()),
    )
    .await?
    {
        return Err(ApiError::Conflict("This link is no longer active.".into()));
    }
    if row.signed_at.is_some() {
        return Err(ApiError::Conflict("This quote is already signed.".into()));
    }
    let quote = load_company_quote(&state.pool, row.id, row.company_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let doc = quote.doc();
    let index = usize::try_from(input.option)
        .ok()
        .filter(|i| *i < doc.color_options.len());
    let (Some(index), false) = (index, input.signature.trim().is_empty()) else {
        return Err(ApiError::BadRequest("Pick an option and sign.".into()));
    };
    let (price_list, _) = pricing_for_quote(&state.pool, &quote).await?;
    if !option_sold_out_colors(&doc, &price_list, index).is_empty() {
        return Err(ApiError::BadRequest(
            "This option has a sold-out color and cannot be accepted.".into(),
        ));
    }
    let signed = sqlx::query(
        "UPDATE quotes SET customer_accepted_option_id = ?, signature_data = ?, signed_at = NOW(), status = 'accepted'
          WHERE id = ? AND portal_token = ? AND signed_at IS NULL",
    )
    .bind(input.option)
    .bind(&input.signature)
    .bind(row.id)
    .bind(&token)
    .execute(&state.pool)
    .await?
    .rows_affected();
    if signed > 0
        && let Err(error) = mark_quote_sold(
            &state.pool,
            row.id,
            row.company_id,
            Some(input.option),
            None,
        )
        .await
    {
        tracing::error!(
            ?error,
            quote = row.id,
            "could not mark the quote sold after signing"
        );
    }
    Ok(StatusCode::NO_CONTENT)
}
