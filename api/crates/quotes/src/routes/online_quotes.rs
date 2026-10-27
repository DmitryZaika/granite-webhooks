//! The online quote: a customer draws their countertops, picks a backsplash,
//! a sink, a cooktop and a few colors, and gets a price, with no session.
//!
//! - `GET  /v1/public/companies/{company_id}/online-quote`        → what the builder offers
//! - `POST /v1/public/companies/{company_id}/online-quote/prices` → each color's total
//! - `POST /v1/public/companies/{company_id}/online-quotes`       → save the quote, email it, tell the team
//!
//! The browser never gets the price list: it sends the drawing and gets the
//! totals back, priced as the quote page prices (`quote_pricing`) with the
//! company's default price list, its stock and its extras. Only simple pieces
//! are taken: 1 to 3 runs at right angles, edges finished, unfinished or with
//! a backsplash, and sink and cooktop cutouts. The rest of the quote is the
//! company's: its depth and rounding, its "always include" extras, its tax.
//!
//! A saved quote is an ordinary quote (active, revision 1). A customer the
//! company does not know (by email or phone) becomes a lead with no sales rep
//! and the sales managers get it in Telegram, with the salespeople to assign
//! it to, like every new lead. A known customer's sales rep is told instead;
//! a known customer with no rep goes to the managers.

use crate::app_origin::{app_origin, company_subdomain};
use crate::entitlements::{AddonKey, company_entitlements, require_addon};
use crate::error::{AddonRequiredBody, ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::openapi::{OpenApiRouter, routes};
use crate::phone::crm_push::{LeadKind, push_lead_logged};
use crate::quote_pricing::doc::QuoteDoc;
use crate::quote_pricing::edit::new_id;
use crate::quote_pricing::js::{arr, num, on, truthy};
use crate::quote_pricing::price_list::{key, material_name, sync_with_stock};
use crate::quote_pricing::pricing::{PriceOptions, price_group_of, price_quote, variant_price};
use crate::quote_pricing::takeoff::js_round;
use crate::quote_pricing::{load_stock, sink_shape_by_code};
use crate::routes::customers::duplicates::phone_last10;
use crate::routes::emails::signature_mail::{Branding, EMAIL_FONT, button_html, card_html};
use crate::routes::quote_editor::{
    STONE_IN_STOCK_SQL, always_included_items, icon_url, price_list_item, token,
};
use crate::routes::quote_portal::money;
use crate::routes::quote_presets::{QuoteSettings, enabled_online_options, load_quote_settings};
use crate::routes::quote_price_lists::{default_id, find as find_price_list, list_rows};
use crate::state::AppState;
use crate::telegram::Bot;
use crate::webhooks_client::OutgoingEmail;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use common::crud::phone_app::users_with_app;
use common::utils::email_body::escape_html;
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(setup))
        .routes(routes!(prices))
        .routes(routes!(create))
}

/// `positions.id` of a sales rep and of a sales manager.
const SALES_REP: i32 = 1;
const SALES_MANAGER: i32 = 2;

const MAX_PIECES: usize = 8;
const MAX_COLORS: usize = 10;
const MAX_CUTOUTS: usize = 6;
/// Online quotes one address may get a day, and a company may get an hour.
const QUOTES_PER_EMAIL_PER_DAY: i64 = 3;
const QUOTES_PER_COMPANY_PER_HOUR: i64 = 30;

/// The one area of an online quote.
const AREA_ID: &str = "area-1";
/// The note of the quote's first revision, for the team.
const REVISION_NOTE: &str = "Made by the customer with the online quote";
const NO_REPLY: &str = "noreply@granite-manager.com";

/// A color the customer can pick: a stone in stock with a photo that the
/// price list prices.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct OnlineColor {
    /// Material, e.g. `Quartz`; send it back as `product`.
    pub product: String,
    /// Color, e.g. `Calacatta Laza`; send it back as `color`.
    pub color: String,
    /// Photo of the stone.
    pub url: String,
    /// Small copy of the photo.
    pub icon: String,
    /// A stone of this color: its installed photos are at
    /// `GET /v1/public/companies/{company_id}/stones/{stone_id}/images`.
    pub stone_id: i32,
}

/// A sink the customer can pick: one of the company's sinks on display, with
/// a photo.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct OnlineSink {
    /// Model name; send it back as the cutout's `sinkModel`.
    pub name: String,
    /// `undermount`, `dropIn` or `farmer`.
    pub sink_type: String,
    /// Cutout shape, e.g. `rectangle` or `60/40`.
    pub sink_shape: String,
    /// Cutout width, inches.
    pub width: f64,
    /// Cutout depth, inches.
    pub depth: f64,
    /// Faucet holes drilled for it.
    pub faucet_holes: i64,
    /// Photo of the sink.
    pub url: String,
    /// Small copy of the photo.
    pub icon: String,
}

/// The cutout every cooktop gets: the price list's default cooktop cutout.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, ToSchema)]
pub struct OnlineCooktop {
    /// Width, inches.
    pub width: f64,
    /// Depth, inches.
    pub depth: f64,
    /// Burners drawn on it.
    pub burners: i64,
}

/// What the online quote offers.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct OnlineQuoteSetup {
    /// Company name.
    pub company_name: String,
    /// Company logo; null when it has none.
    pub logo_url: Option<String>,
    /// The company prices quotes online: it has a price list with prices and
    /// at least one color to offer. When false, show "call us" instead.
    pub available: bool,
    /// Counter depth (front to back) to start with, inches.
    pub default_depth: f64,
    /// Colors to pick from, by material then name. Prices are never listed.
    pub colors: Vec<OnlineColor>,
    /// Sinks to pick from.
    pub sinks: Vec<OnlineSink>,
    /// The cooktop cutout.
    pub cooktop: OnlineCooktop,
    /// Extras the customer can add (Admin > Quotes > Customer Quote), in
    /// order. Empty: the builder has no Extras step.
    pub extras: Vec<String>,
}

/// An edge: polished, plain against a wall, or with a backsplash on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub enum OnlineEdgeType {
    Finished,
    Unfinished,
    Splash,
}

/// One edge of a piece.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct OnlineEdge {
    /// `finished` (polished with the color's first edge profile),
    /// `unfinished` (against a wall) or `splash` (a backsplash on it).
    #[serde(rename = "type")]
    pub kind: OnlineEdgeType,
    /// Backsplash height, inches: 1 to 96 for `splash`; ignored otherwise.
    #[serde(default)]
    pub splash_height: f64,
}

/// `sink` or `cooktop`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub enum OnlineCutoutKind {
    Sink,
    Cooktop,
}

/// How the customer's own sink is mounted: `undermount`, `dropIn` (top-mount)
/// or `farmer` (farmhouse).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub enum OnlineSinkType {
    Undermount,
    DropIn,
    Farmer,
}

/// A sink or cooktop cutout in a run.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct OnlineCutout {
    /// Cutout id: letters, digits, `-` and `_`, up to 40, unique in the drawing.
    pub id: String,
    /// What it is cut for.
    pub kind: OnlineCutoutKind,
    /// Run corner its position is measured from: 0 start-left, 1 end-left,
    /// 2 end-right, 3 start-right.
    pub anchor_corner: i64,
    /// Inches along the run from that corner to the cutout's center.
    pub along: f64,
    /// Inches across the run from that corner to the cutout's center.
    pub across: f64,
    /// A sink from the builder's `sinks` (its name); its size, type, shape
    /// and faucet holes are the model's. Null for the customer's own sink:
    /// a `sink_type` cutout of `width` × `depth`. Ignored for cooktops,
    /// which are always the builder's `cooktop`.
    #[serde(default)]
    pub sink_model: Option<String>,
    /// Own sink: how it is mounted; null = undermount. Ignored otherwise.
    #[serde(default)]
    pub sink_type: Option<OnlineSinkType>,
    /// Own sink: cutout width, 10 to 60 inches.
    #[serde(default)]
    pub width: f64,
    /// Own sink: cutout depth, 10 to 30 inches.
    #[serde(default)]
    pub depth: f64,
}

/// A straight run of a piece.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct OnlineRun {
    /// Length, 6 to 600 inches, measured on the outside of its turns.
    pub length: f64,
    /// Depth (front to back), 6 to 600 inches.
    pub depth: f64,
    /// Turn from the run before, in 22.5° steps: 0 for the first run, 4 or
    /// -4 (a right angle) for the others.
    #[serde(default)]
    pub turn: i64,
    /// Its sink and cooktop cutouts.
    #[serde(default)]
    pub cutouts: Vec<OnlineCutout>,
}

/// A countertop: 1 run (straight or an island), 2 (an L) or 3 (a U), as the
/// quote drawing stores counters.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct OnlinePiece {
    /// Piece id: letters, digits, `-` and `_`, up to 40, unique in the drawing.
    pub id: String,
    /// Where the first run starts on the drawing, inches.
    pub x: f64,
    /// Where the first run starts on the drawing, inches (down is positive).
    pub y: f64,
    /// Direction of the first run in 22.5° steps (0 right, 4 down, -4 up).
    #[serde(default)]
    pub angle: i64,
    /// 1 to 3 runs.
    pub runs: Vec<OnlineRun>,
    /// 2 × runs + 2 edges in drawing order: the left side of each run, the
    /// end, the right side of each run (last run first), then the start.
    pub edges: Vec<OnlineEdge>,
}

/// A color to price: one of the builder's `colors`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct OnlineColorPick {
    /// Its `product`.
    pub product: String,
    /// Its `color`.
    pub color: String,
}

/// Body of `POST /v1/public/companies/{company_id}/online-quote/prices`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct OnlineDrawing {
    /// 1 to 8 countertops.
    pub pieces: Vec<OnlinePiece>,
    /// 1 to 10 colors to compare.
    pub colors: Vec<OnlineColorPick>,
    /// The builder's `extras` the customer wants, by name.
    #[serde(default)]
    pub extras: Vec<String>,
}

/// One color's price.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct OnlinePrice {
    /// Material.
    pub product: String,
    /// Color.
    pub color: String,
    /// Total with tax, dollars; null when the price list cannot price all of
    /// it (the company gives that price itself).
    pub total: Option<f64>,
}

/// Each color's total.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct OnlinePrices {
    /// In the order the colors were sent.
    pub prices: Vec<OnlinePrice>,
}

/// How the customer wants to hear from the team: `call`, `text` or `email`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub enum OnlineContactBy {
    Call,
    Text,
    Email,
}

/// Body of `POST /v1/public/companies/{company_id}/online-quotes`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct OnlineQuoteInput {
    /// The customer's name, 1 to 100 characters.
    pub name: String,
    /// Where the quote is emailed.
    pub email: String,
    /// US phone with its area code: 10 digits, or 11 starting with 1.
    pub phone: String,
    /// 1 to 8 countertops.
    pub pieces: Vec<OnlinePiece>,
    /// 1 to 10 colors, one quote option each.
    pub colors: Vec<OnlineColorPick>,
    /// The builder's `extras` the customer wants, by name; each goes on the
    /// quote as an extra.
    #[serde(default)]
    pub extras: Vec<String>,
    /// A hidden form field people leave empty. When it is filled, the request
    /// is answered as sent and nothing is saved or sent.
    #[serde(default)]
    pub website: Option<String>,
    /// How the customer wants to hear from the team about the quote; the
    /// team's notice says so. Null: no preference.
    #[serde(default)]
    pub contact_by: Option<OnlineContactBy>,
}

/// The saved quote.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct OnlineQuoteSent {
    /// Where the quote was emailed.
    pub email: String,
    /// False when the quote was saved but the email could not be sent; the
    /// team is told and sends it.
    pub emailed: bool,
    /// Each color's total, as on the quote.
    pub prices: Vec<OnlinePrice>,
}

fn bad(message: &str) -> ApiError {
    ApiError::BadRequest(message.to_string())
}

fn within(value: f64, lo: f64, hi: f64) -> bool {
    value.is_finite() && value >= lo && value <= hi
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 40
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[derive(sqlx::FromRow)]
struct Company {
    name: Option<String>,
    logo_url: Option<String>,
}

impl Company {
    fn name(&self) -> String {
        self.name.as_deref().unwrap_or_default().trim().to_string()
    }
}

async fn company_of(pool: &MySqlPool, company_id: i32) -> ApiResult<Company> {
    let company: Option<Company> =
        sqlx::query_as("SELECT name, logo_url FROM company WHERE id = ? AND deleted_at IS NULL")
            .bind(company_id)
            .fetch_optional(pool)
            .await?;
    company.ok_or(ApiError::NotFound)
}

/// The company's default price list as a new quote prices with it today
/// (`pricingForQuote` of a quote made now), and what a new quote gets.
struct Offer {
    price_list_id: i32,
    revision_id: Option<i32>,
    /// Synced with the stock and the extras catalog.
    prices: Value,
    expiration_days: i64,
    tax_pct: f64,
    terms: Option<String>,
    settings: QuoteSettings,
}

async fn load_offer(pool: &MySqlPool, company_id: i32) -> ApiResult<Option<Offer>> {
    let lists = list_rows(pool, company_id).await?;
    let Some(id) = default_id(pool, company_id, &lists).await? else {
        return Ok(None);
    };
    let Some((row, Some(info))) = find_price_list(pool, id, company_id).await? else {
        return Ok(None);
    };
    let stock = load_stock(pool, company_id).await?;
    let prices = sync_with_stock(&info, &stock);
    // As `create_quote`: the list's tax rate, its payment terms or the company's first.
    let tax_pct: Option<f64> = match num(Some(&info), "defaultTaxRateId") {
        Some(rate) => {
            sqlx::query_scalar(
                "SELECT CAST(tax_pct AS DOUBLE) FROM quote_tax_rates
                  WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
            )
            .bind(rate)
            .bind(company_id)
            .fetch_optional(pool)
            .await?
        }
        None => None,
    };
    let terms: Option<String> = sqlx::query_scalar(
        "SELECT terms FROM quote_payment_terms WHERE company_id = ? AND deleted_at IS NULL
          ORDER BY id = ? DESC, sequence, name LIMIT 1",
    )
    .bind(company_id)
    .bind(num(Some(&info), "defaultPaymentTermsId").unwrap_or(-1.0))
    .fetch_optional(pool)
    .await?;
    #[allow(clippy::cast_possible_truncation)]
    let expiration_days = num(Some(&info), "expirationDays").unwrap_or(30.0) as i64;
    Ok(Some(Offer {
        price_list_id: row.id,
        revision_id: row.current_revision_id,
        prices,
        expiration_days,
        tax_pct: tax_pct.unwrap_or(0.0),
        terms,
        settings: load_quote_settings(pool, company_id).await?,
    }))
}

/// Shown in the catalog: a missing flag counts as shown.
fn displayed(item: &Value) -> bool {
    item.get("isDisplay")
        .is_none_or(|shown| shown.is_null() || on(Some(shown)))
}

fn name_of(item: &Value) -> &str {
    item.get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
}

/// Colors the builder offers, by material then name: priced by the list, not
/// sold out, shown in the catalog, with a photo of a stone in stock.
async fn offered_colors(
    pool: &MySqlPool,
    company_id: i32,
    prices: &Value,
) -> ApiResult<Vec<OnlineColor>> {
    let photos: Vec<(i32, String, String, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT s.id, COALESCE(s.name, ''), COALESCE(s.type, ''), TRIM(s.url) FROM stones s
          WHERE s.company_id = ? AND s.deleted_at IS NULL AND COALESCE(s.is_display, 0) = 1
            AND s.url IS NOT NULL AND TRIM(s.url) != ''
            AND LOWER(TRIM(s.url)) NOT IN ('undefined', 'null')
            AND {STONE_IN_STOCK_SQL}
          ORDER BY s.id DESC"
    )))
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    let mut by_color: HashMap<(String, String), (i32, String)> = HashMap::new();
    for (id, name, kind, url) in photos {
        by_color
            .entry((key(&material_name(&kind)), key(&name)))
            .or_insert((id, url));
    }
    let mut colors = Vec::new();
    for product in arr(Some(prices), "arrProducts") {
        let material = name_of(product);
        if material.is_empty() {
            continue;
        }
        for variant in arr(Some(product), "arrVariants") {
            let color = name_of(variant);
            if color.is_empty()
                || truthy(variant.get("soldOut"))
                || truthy(variant.get("noImage"))
                || !displayed(variant)
                || variant_price(Some(variant), price_group_of(product, Some(variant))).is_none()
            {
                continue;
            }
            let Some((stone_id, url)) = by_color.get(&(key(material), key(color))) else {
                continue;
            };
            colors.push(OnlineColor {
                product: material.to_string(),
                color: color.to_string(),
                stone_id: *stone_id,
                icon: icon_url(url),
                url: url.clone(),
            });
        }
    }
    colors.sort_by_key(|c| (key(&c.product), key(&c.color)));
    Ok(colors)
}

/// The list's sink models the builder offers: shown in the catalog, with a
/// cutout size and a photo.
async fn offered_sinks(
    pool: &MySqlPool,
    company_id: i32,
    prices: &Value,
) -> ApiResult<Vec<OnlineSink>> {
    let photos: Vec<(String, String)> = sqlx::query_as(
        "SELECT COALESCE(name, ''), TRIM(url) FROM sink_type
          WHERE company_id = ? AND is_deleted = 0 AND COALESCE(is_display, 0) = 1
            AND url IS NOT NULL AND TRIM(url) != '' AND LOWER(TRIM(url)) NOT IN ('undefined', 'null')
          ORDER BY id",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    let mut by_name: HashMap<String, String> = HashMap::new();
    for (name, url) in photos {
        by_name.entry(key(&name)).or_insert(url);
    }
    let mut sinks = Vec::new();
    for model in arr(Some(prices), "arrSinkModels") {
        let name = name_of(model);
        let width = num(Some(model), "cutoutWidth").filter(|w| *w > 0.0);
        let depth = num(Some(model), "cutoutDepth").filter(|d| *d > 0.0);
        let (Some(width), Some(depth), Some(url)) = (width, depth, by_name.get(&key(name))) else {
            continue;
        };
        if name.is_empty()
            || !displayed(model)
            || sinks.iter().any(|s: &OnlineSink| key(&s.name) == key(name))
        {
            continue;
        }
        let sink_type = match num(Some(model), "sinkType") {
            Some(code) if (code - 1.0).abs() < f64::EPSILON => "dropIn",
            Some(code) if (code - 3.0).abs() < f64::EPSILON => "farmer",
            _ => "undermount",
        };
        #[allow(clippy::cast_possible_truncation)]
        let faucet_holes = num(Some(model), "faucetHoleCount")
            .unwrap_or(0.0)
            .clamp(0.0, 5.0) as i64;
        sinks.push(OnlineSink {
            name: name.to_string(),
            sink_type: sink_type.into(),
            sink_shape: sink_shape_by_code(num(Some(model), "sinkShape").unwrap_or(1.0)).into(),
            width,
            depth,
            faucet_holes,
            icon: icon_url(url),
            url: url.clone(),
        });
    }
    Ok(sinks)
}

/// `defaultCooktopCutout`: field by field, else 23" × 14", 4 burners.
fn cooktop_cutout(prices: &Value) -> OnlineCooktop {
    let set = prices.get("defaultCooktopCutout");
    let positive = |value: Option<f64>, fallback: f64| {
        value
            .filter(|v| v.is_finite() && *v > 0.0)
            .unwrap_or(fallback)
    };
    #[allow(clippy::cast_possible_truncation)]
    let burners = positive(num(set, "burners"), 4.0).round() as i64;
    OnlineCooktop {
        width: positive(num(set, "cutoutWidth"), 23.0),
        depth: positive(num(set, "cutoutDepth"), 14.0),
        burners,
    }
}

/// A full drawing cutout.
#[allow(clippy::too_many_arguments)]
fn cutout_json(
    cut: &OnlineCutout,
    kind: &str,
    width: f64,
    depth: f64,
    model: Option<&str>,
    sink_type: &str,
    sink_shape: &str,
    faucet_holes: i64,
    burners: i64,
) -> Value {
    json!({
        "id": cut.id, "kind": kind, "anchorCorner": cut.anchor_corner,
        "along": cut.along, "across": cut.across, "turn": 0,
        "width": width, "depth": depth, "centerline": "none",
        "sinkModel": model, "sinkType": sink_type, "sinkShape": sink_shape,
        "faucetHoles": faucet_holes, "burners": burners, "name": "", "price": null,
        "allowDiscount": true, "oval": false,
    })
}

/// What a quote is made of: the drawing and the colors, checked.
struct Drawing {
    /// The quote document (`draft_drawing_json`).
    doc: Value,
    /// `(material, color)` of each option, as the builder names them.
    colors: Vec<(String, String)>,
}

/// A cutout in its run, checked: a cooktop is the builder's cooktop, a sink
/// model the builder offers takes the model's size and type, and the
/// customer's own sink is an undermount cutout of the size sent.
fn checked_cutout(
    cut: &OnlineCutout,
    sinks: &[OnlineSink],
    cooktop: OnlineCooktop,
) -> ApiResult<Value> {
    if !(0..=3).contains(&cut.anchor_corner)
        || !within(cut.along, -600.0, 600.0)
        || !within(cut.across, -600.0, 600.0)
    {
        return Err(bad("A sink or cooktop is off its countertop."));
    }
    let model = cut
        .sink_model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty());
    Ok(match (cut.kind, model) {
        (OnlineCutoutKind::Cooktop, _) => cutout_json(
            cut,
            "cooktop",
            cooktop.width,
            cooktop.depth,
            None,
            "undermount",
            "rectangle",
            0,
            cooktop.burners,
        ),
        (OnlineCutoutKind::Sink, Some(model)) => {
            let sink = sinks
                .iter()
                .find(|s| key(&s.name) == key(model))
                .ok_or_else(|| bad("That sink is no longer available. Pick another one."))?;
            cutout_json(
                cut,
                "sink",
                sink.width,
                sink.depth,
                Some(&sink.name),
                &sink.sink_type,
                &sink.sink_shape,
                sink.faucet_holes,
                4,
            )
        }
        (OnlineCutoutKind::Sink, None) => {
            if !within(cut.width, 10.0, 60.0) || !within(cut.depth, 10.0, 30.0) {
                return Err(bad(
                    "Your own sink must be 10 to 60 inches wide and 10 to 30 inches deep.",
                ));
            }
            cutout_json(
                cut,
                "sink",
                cut.width,
                cut.depth,
                None,
                match cut.sink_type.unwrap_or(OnlineSinkType::Undermount) {
                    OnlineSinkType::Undermount => "undermount",
                    OnlineSinkType::DropIn => "dropIn",
                    OnlineSinkType::Farmer => "farmer",
                },
                "rectangle",
                0,
                4,
            )
        }
    })
}

fn piece_counter(
    index: usize,
    piece: &OnlinePiece,
    ids: &mut HashSet<String>,
    cutouts_left: &mut usize,
    sinks: &[OnlineSink],
    cooktop: OnlineCooktop,
) -> ApiResult<Value> {
    if !valid_id(&piece.id) || !ids.insert(piece.id.clone()) {
        return Err(bad("Every countertop needs an id of its own."));
    }
    if !within(piece.x, -10_000.0, 10_000.0)
        || !within(piece.y, -10_000.0, 10_000.0)
        || !(-16..=16).contains(&piece.angle)
    {
        return Err(bad("A countertop is off the drawing."));
    }
    let n = piece.runs.len();
    if !(1..=3).contains(&n) {
        return Err(bad("A countertop is straight, an L or a U."));
    }
    if piece.edges.len() != 2 * n + 2 {
        return Err(bad("A countertop's edges do not match its sides."));
    }
    let mut runs = Vec::with_capacity(n);
    for (i, run) in piece.runs.iter().enumerate() {
        let square = if i == 0 {
            run.turn == 0
        } else {
            run.turn.abs() == 4
        };
        if !square {
            return Err(bad("The corners of a countertop must be square."));
        }
        if !within(run.length, 6.0, 600.0) {
            return Err(bad("Each side must be 6 to 600 inches long."));
        }
        if !within(run.depth, 6.0, 600.0) {
            return Err(bad("A countertop must be 6 to 600 inches deep."));
        }
        let mut cutouts = Vec::with_capacity(run.cutouts.len());
        for cut in &run.cutouts {
            if *cutouts_left == 0 {
                return Err(bad("Add at most 6 sinks and cooktops."));
            }
            *cutouts_left -= 1;
            if !valid_id(&cut.id) || !ids.insert(cut.id.clone()) {
                return Err(bad("Every sink and cooktop needs an id of its own."));
            }
            cutouts.push(checked_cutout(cut, sinks, cooktop)?);
        }
        runs.push(json!({
            "length": run.length, "depth": run.depth, "turn": run.turn, "cutouts": cutouts,
        }));
    }
    let mut edges = Vec::with_capacity(piece.edges.len());
    for edge in &piece.edges {
        let (kind, height) = match edge.kind {
            OnlineEdgeType::Finished => ("finished", 0.0),
            OnlineEdgeType::Unfinished => ("unfinished", 0.0),
            OnlineEdgeType::Splash => {
                if !within(edge.splash_height, 1.0, 96.0) {
                    return Err(bad("A backsplash must be 1 to 96 inches tall."));
                }
                ("splash", edge.splash_height)
            }
        };
        edges.push(json!({
            "type": kind, "profile": 0, "splashHeight": height, "miterHeight": 0,
            "fullRadius": 0, "bumps": [], "splashBumps": [],
        }));
    }
    Ok(json!({
        "id": piece.id, "name": format!("Counter #{}", index + 1), "areaId": AREA_ID,
        "x": piece.x, "y": piece.y, "angle": piece.angle, "runs": runs, "edges": edges,
        "corners": vec![json!([null, null]); n + 1], "outlets": 0,
    }))
}

/// Checks the pieces and colors against what the builder offers and makes
/// the quote document: one area, one option per color, the company's depth,
/// rounding and "always include" extras, and the extras the customer picked.
fn build_drawing(
    offer: &Offer,
    offered: &[OnlineColor],
    sinks: &[OnlineSink],
    pieces: &[OnlinePiece],
    picks: &[OnlineColorPick],
    extras: &[String],
) -> ApiResult<Drawing> {
    if pieces.is_empty() || pieces.len() > MAX_PIECES {
        return Err(bad("Add 1 to 8 countertops."));
    }
    let cooktop = cooktop_cutout(&offer.prices);
    let mut ids = HashSet::new();
    let mut cutouts_left = MAX_CUTOUTS;
    let counters = pieces
        .iter()
        .enumerate()
        .map(|(i, piece)| piece_counter(i, piece, &mut ids, &mut cutouts_left, sinks, cooktop))
        .collect::<ApiResult<Vec<Value>>>()?;
    if picks.is_empty() || picks.len() > MAX_COLORS {
        return Err(bad("Pick 1 to 10 colors."));
    }
    let mut colors: Vec<(String, String)> = Vec::with_capacity(picks.len());
    for pick in picks {
        let found = offered
            .iter()
            .find(|c| key(&c.product) == key(&pick.product) && key(&c.color) == key(&pick.color))
            .ok_or_else(|| bad("One of your colors is no longer available. Pick another one."))?;
        if colors
            .iter()
            .any(|(p, c)| *p == found.product && *c == found.color)
        {
            return Err(bad("Pick each color once."));
        }
        colors.push((found.product.clone(), found.color.clone()));
    }
    let options: Vec<Value> = colors
        .iter()
        .enumerate()
        .map(|(i, (product, color))| {
            let mut by_area = serde_json::Map::new();
            by_area.insert(
                AREA_ID.into(),
                json!({ "product": product, "color": color, "edges": [], "otherPrice": null }),
            );
            json!({ "id": format!("option-{}", i + 1), "byArea": by_area })
        })
        .collect();
    let doc = json!({
        "version": 2,
        "defaultDepth": offer.settings.default_depth,
        "counters": counters,
        "otherCounters": [],
        "texts": [],
        "pageBreaks": [],
        "areas": [{ "id": AREA_ID, "name": "Area #1", "quantity": 1 }],
        "colorOptions": options,
        "layouts": {},
        "items": quote_items(&offer.prices, extras),
        "priceOverrides": {},
        "rounding": offer.settings.rounding,
    });
    Ok(Drawing { doc, colors })
}

/// The list's "always include" extras, then each extra the customer picked
/// that is not one of them: the list's extra of that name, or a line with no
/// price for the team to price.
fn quote_items(prices: &Value, extras: &[String]) -> Vec<Value> {
    let mut items = always_included_items(prices);
    let listed = arr(Some(prices), "arrOtherItems");
    for extra in extras {
        if items.iter().any(|i| key(name_of(i)) == key(extra)) {
            continue;
        }
        items.push(
            match listed.iter().find(|o| key(name_of(o)) == key(extra)) {
                Some(def) => price_list_item(def),
                None => json!({
                    "id": new_id("item"),
                    "kind": "misc",
                    "areaId": null,
                    "name": extra,
                    "option": null,
                    "catalogId": null,
                    "optionId": null,
                    "quantity": 1,
                    "amount": null,
                    "price": null,
                    "hideOnQuote": false,
                    "allowDiscount": true,
                    "taxable": true,
                }),
            },
        );
    }
    items
}

/// The customer's extras, checked against what the company offers.
async fn checked_extras(
    pool: &MySqlPool,
    company_id: i32,
    picks: &[String],
) -> ApiResult<Vec<String>> {
    let offered = enabled_online_options(pool, company_id).await?;
    let mut extras: Vec<String> = Vec::with_capacity(picks.len());
    for pick in picks {
        let found = offered
            .iter()
            .find(|o| key(o) == key(pick))
            .ok_or_else(|| {
                bad("One of your extras is no longer offered. Go back and check them.")
            })?;
        if !extras.contains(found) {
            extras.push(found.clone());
        }
    }
    Ok(extras)
}

/// Each color's total (to the cent, null when not all of it is priced) and
/// the revision's `totals_json` entries.
fn price_drawing(offer: &Offer, drawing: &Drawing) -> ApiResult<(Vec<OnlinePrice>, Vec<Value>)> {
    let doc = QuoteDoc::parse(&drawing.doc).ok_or_else(|| bad("The drawing could not be read."))?;
    let options = price_quote(
        &doc,
        &offer.prices,
        PriceOptions {
            discount_pct: 0.0,
            discount_amount: 0.0,
            tax_pct: offer.tax_pct,
        },
    );
    let prices = drawing
        .colors
        .iter()
        .zip(&options)
        .map(|((product, color), option)| OnlinePrice {
            product: product.clone(),
            color: color.clone(),
            total: (!option.missing && option.total.is_finite())
                .then(|| js_round(option.total * 100.0) / 100.0),
        })
        .collect();
    let totals = options
        .iter()
        .map(|o| json!({ "optionId": o.option_id, "total": o.total, "missing": o.missing }))
        .collect();
    Ok((prices, totals))
}

/// What the builder offers today, or a 409 when the company has no prices.
async fn ready_offer(
    pool: &MySqlPool,
    company_id: i32,
) -> ApiResult<(Offer, Vec<OnlineColor>, Vec<OnlineSink>)> {
    let Some(offer) = load_offer(pool, company_id).await? else {
        return Err(ApiError::Conflict(
            "Online quotes are not available right now. Please call us for a quote.".into(),
        ));
    };
    let colors = offered_colors(pool, company_id, &offer.prices).await?;
    let sinks = offered_sinks(pool, company_id, &offer.prices).await?;
    Ok((offer, colors, sinks))
}

/// Start an online quote
///
/// What the company's online quote builder offers: the colors (stones in
/// stock with a photo that its default price list prices, never their
/// prices), the sinks on display with a photo, the cooktop cutout and the
/// counter depth to start with. `available` is false when the company has no
/// price list with prices or no color to offer, or its plan lacks the Quotes
/// add-on. Next, price the drawing with
/// `POST .../online-quote/prices`. No session needed.
#[crate::openapi::path(
    get,
    path = "/v1/public/companies/{company_id}/online-quote",
    operation_id = "get_online_quote_setup",
    tag = "public",
    params(("company_id" = i32, Path, description = "Company id")),
    responses(
        (status = 200, description = "What the builder offers", body = OnlineQuoteSetup),
        (status = 404, description = "No such company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    )
)]
pub async fn setup(
    State(state): State<AppState>,
    Path(company_id): Path<i32>,
) -> ApiResult<Json<OnlineQuoteSetup>> {
    let company = company_of(&state.pool, company_id).await?;
    let offer = if company_entitlements(&state.pool, company_id)
        .await?
        .has(AddonKey::Quotes)
    {
        load_offer(&state.pool, company_id).await?
    } else {
        None
    };
    let Some(offer) = offer else {
        let settings = load_quote_settings(&state.pool, company_id).await?;
        return Ok(Json(OnlineQuoteSetup {
            company_name: company.name(),
            logo_url: company.logo_url,
            available: false,
            default_depth: settings.default_depth,
            colors: Vec::new(),
            sinks: Vec::new(),
            cooktop: cooktop_cutout(&Value::Null),
            extras: Vec::new(),
        }));
    };
    let colors = offered_colors(&state.pool, company_id, &offer.prices).await?;
    let sinks = offered_sinks(&state.pool, company_id, &offer.prices).await?;
    Ok(Json(OnlineQuoteSetup {
        company_name: company.name(),
        logo_url: company.logo_url,
        available: !colors.is_empty(),
        default_depth: offer.settings.default_depth,
        cooktop: cooktop_cutout(&offer.prices),
        colors,
        sinks,
        extras: enabled_online_options(&state.pool, company_id).await?,
    }))
}

/// Price an online quote
///
/// The total with tax of each color for the drawing, priced like the quote
/// page prices a quote made now with the company's default price list. A
/// color's total is null when the list cannot price all of it. Nothing is
/// saved; save and email it with `POST .../online-quotes`. No session needed.
#[crate::openapi::path(
    post,
    path = "/v1/public/companies/{company_id}/online-quote/prices",
    operation_id = "price_online_quote",
    tag = "public",
    params(("company_id" = i32, Path, description = "Company id")),
    request_body = OnlineDrawing,
    responses(
        (status = 200, description = "Each color's total", body = OnlinePrices),
        (status = 400, description = "A piece the builder does not draw (more than 3 sides, corners that are not square, sizes out of range, unknown fields), more than 6 cutouts, a sink or color it does not offer, or not 1 to 10 colors", body = ErrorBody),
        (status = 403, description = "The plan lacks the Quotes add-on (`addon`: `quotes`)", body = AddonRequiredBody),
        (status = 404, description = "No such company", body = ErrorBody),
        (status = 409, description = "The company has no prices for online quotes", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    )
)]
pub async fn prices(
    State(state): State<AppState>,
    Path(company_id): Path<i32>,
    JsonBody(input): JsonBody<OnlineDrawing>,
) -> ApiResult<Json<OnlinePrices>> {
    require_addon(&state.pool, company_id, AddonKey::Quotes).await?;
    company_of(&state.pool, company_id).await?;
    let (offer, colors, sinks) = ready_offer(&state.pool, company_id).await?;
    let extras = checked_extras(&state.pool, company_id, &input.extras).await?;
    let drawing = build_drawing(
        &offer,
        &colors,
        &sinks,
        &input.pieces,
        &input.colors,
        &extras,
    )?;
    let (prices, _) = price_drawing(&offer, &drawing)?;
    Ok(Json(OnlinePrices { prices }))
}

fn looks_like_email(email: &str) -> bool {
    if email.len() > 254
        || email
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, ',' | ';' | '<' | '>' | '"'))
    {
        return false;
    }
    let Some((local, domain)) = email.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.contains('@')
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !domain.contains("..")
}

/// Name, email and phone, trimmed and checked.
fn contact(input: &OnlineQuoteInput) -> ApiResult<(String, String, String)> {
    let name = input.name.trim();
    if name.is_empty() || name.chars().count() > 100 {
        return Err(bad("Type your name (up to 100 characters)."));
    }
    let email = input.email.trim();
    if !looks_like_email(email) {
        return Err(bad("Type a valid email address."));
    }
    let phone = input.phone.trim();
    let digits: String = phone.chars().filter(char::is_ascii_digit).collect();
    let us = digits.len() == 10 || (digits.len() == 11 && digits.starts_with('1'));
    if phone.chars().count() > 30 || !us {
        return Err(bad("Type your phone number with its area code."));
    }
    Ok((name.to_string(), email.to_string(), phone.to_string()))
}

/// At most `QUOTES_PER_EMAIL_PER_DAY` online quotes to one address a day and
/// `QUOTES_PER_COMPANY_PER_HOUR` to a company an hour.
async fn throttle(pool: &MySqlPool, company_id: i32, email: &str) -> ApiResult<()> {
    let (today, this_hour): (i64, i64) = sqlx::query_as(
        "SELECT CAST(COALESCE(SUM(LOWER(e.recipients) = LOWER(?)), 0) AS SIGNED),
                CAST(COALESCE(SUM(e.sent_at > NOW() - INTERVAL 1 HOUR), 0) AS SIGNED)
           FROM quote_emails e JOIN quotes q ON q.id = e.quote_id
          WHERE q.company_id = ? AND q.created_by IS NULL AND e.sent_by IS NULL
            AND e.sent_at > NOW() - INTERVAL 1 DAY",
    )
    .bind(email)
    .bind(company_id)
    .fetch_one(pool)
    .await?;
    if today >= QUOTES_PER_EMAIL_PER_DAY {
        return Err(ApiError::TooManyRequests(
            "You already sent us your details today. We will be in touch soon.".into(),
        ));
    }
    if this_hour >= QUOTES_PER_COMPANY_PER_HOUR {
        return Err(ApiError::TooManyRequests(
            "We are getting a lot of quotes right now. Please try again in an hour.".into(),
        ));
    }
    Ok(())
}

#[derive(sqlx::FromRow)]
struct Known {
    id: i32,
    name: Option<String>,
    sales_rep: Option<i32>,
}

/// The company's newest live customer with this email or phone (last 10
/// digits), as the lead pipeline matches leads.
async fn known_customer(
    pool: &MySqlPool,
    company_id: i32,
    email: &str,
    phone: &str,
) -> ApiResult<Option<Known>> {
    let last10 = phone_last10(Some(phone));
    Ok(sqlx::query_as(
        "SELECT c.id, c.name, c.sales_rep FROM customers c
          WHERE c.company_id = ? AND c.deleted_at IS NULL
            AND (EXISTS (SELECT 1 FROM customers_emails ce
                          WHERE ce.customer_id = c.id AND LOWER(TRIM(ce.email)) = LOWER(?))
                 OR (? IS NOT NULL
                     AND (RIGHT(REGEXP_REPLACE(COALESCE(c.phone, ''), '[^0-9]', ''), 10) = ?
                          OR RIGHT(REGEXP_REPLACE(COALESCE(c.phone_2, ''), '[^0-9]', ''), 10) = ?)))
          ORDER BY c.id DESC LIMIT 1",
    )
    .bind(company_id)
    .bind(email)
    .bind(&last10)
    .bind(&last10)
    .bind(&last10)
    .fetch_optional(pool)
    .await?)
}

/// A user the team notice goes to.
#[derive(Debug, Clone, sqlx::FromRow)]
struct Staff {
    id: i32,
    name: Option<String>,
    email: Option<String>,
    telegram_id: Option<i64>,
}

impl Staff {
    fn email(&self) -> Option<&str> {
        self.email
            .as_deref()
            .map(str::trim)
            .filter(|e| e.contains('@'))
    }

    fn name(&self) -> &str {
        self.name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .unwrap_or("Unknown")
    }
}

async fn rep_of(
    pool: &MySqlPool,
    company_id: i32,
    user_id: Option<i32>,
) -> ApiResult<Option<Staff>> {
    let Some(user_id) = user_id else {
        return Ok(None);
    };
    Ok(sqlx::query_as(
        "SELECT id, name, email, telegram_id FROM users
          WHERE id = ? AND company_id = ? AND is_deleted = 0 LIMIT 1",
    )
    .bind(user_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?)
}

/// A sales rep or manager of the company, with this month's leads.
#[derive(sqlx::FromRow)]
struct SalesUser {
    id: i32,
    name: Option<String>,
    email: Option<String>,
    telegram_id: Option<i64>,
    position_id: i32,
    user_position_id: i32,
    mtd_lead_count: i64,
}

/// `get_sales_users` of the lead pipeline, kept to the company's own positions.
async fn sales_users(pool: &MySqlPool, company_id: i32) -> Result<Vec<SalesUser>, sqlx::Error> {
    sqlx::query_as(
        "SELECT u.id, u.name, u.email, u.telegram_id, up.position_id, up.id AS user_position_id,
                COUNT(c.id) AS mtd_lead_count
           FROM users u
           JOIN users_positions up ON up.user_id = u.id AND up.company_id = u.company_id
           LEFT JOIN customers c ON c.sales_rep = u.id AND c.source = 'leads'
                AND c.assigned_date >= DATE_FORMAT(NOW(), '%Y-%m-01')
                AND c.company_id = u.company_id AND c.deleted_at IS NULL
          WHERE u.company_id = ? AND u.is_deleted = 0 AND up.position_id IN (?, ?)
          GROUP BY u.id, u.name, u.email, u.telegram_id, up.position_id, up.id
          ORDER BY u.name, up.id",
    )
    .bind(company_id)
    .bind(SALES_REP)
    .bind(SALES_MANAGER)
    .fetch_all(pool)
    .await
}

/// A new lead, as every lead comes in: source `leads`, from the website.
async fn insert_lead(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    company_id: i32,
    name: &str,
    email: &str,
    phone: &str,
) -> ApiResult<i32> {
    let id = sqlx::query(
        "INSERT INTO customers (name, phone, company_id, source, referral_source, form_name, created_by)
         VALUES (?, ?, ?, 'leads', 'website', 'online_quote', 'Online quote')",
    )
    .bind(name)
    .bind(phone)
    .bind(company_id)
    .execute(&mut **tx)
    .await?
    .last_insert_id();
    let email_id = sqlx::query("INSERT INTO customers_emails (customer_id, email) VALUES (?, ?)")
        .bind(id)
        .bind(email)
        .execute(&mut **tx)
        .await?
        .last_insert_id();
    sqlx::query("UPDATE customers SET email_id = ? WHERE id = ?")
        .bind(email_id)
        .bind(id)
        .execute(&mut **tx)
        .await?;
    Ok(i32::try_from(id).unwrap_or(i32::MAX))
}

fn price_text(price: &OnlinePrice) -> String {
    price
        .total
        .map_or_else(|| "price to be confirmed".to_string(), money)
}

/// The customer's email: we got their details and will contact them, the
/// colors they picked (never a price) and a button to what they sent.
fn customer_html(
    branding: &Branding,
    name: &str,
    colors: &[OnlineColorPick],
    link: &str,
) -> String {
    let cell = format!(
        "{EMAIL_FONT}font-size:15px;color:#3f3f46;padding:12px 10px;border-bottom:1px solid #e4e4e7;"
    );
    let rows = colors.iter().fold(String::new(), |mut out, pick| {
        let _ = write!(
            out,
            "<tr><td style=\"{cell}\">{}</td></tr>",
            escape_html(&format!("{} {}", pick.product, pick.color)),
        );
        out
    });
    let first = name.split_whitespace().next().unwrap_or(name);
    card_html(
        branding,
        &format!(
            "<tr><td align=\"center\" style=\"{EMAIL_FONT}font-size:24px;color:#18181b;padding-bottom:6px;\">We Got Your Details</td></tr>
          <tr><td align=\"center\" style=\"{EMAIL_FONT}font-size:15px;line-height:1.6;color:#3f3f46;padding-bottom:24px;\">Hi {}, thank you for sending us your countertop sizes. We will look them over and contact you soon. The colors you picked:</td></tr>
          <tr><td style=\"padding-bottom:24px;\"><table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\">{rows}</table></td></tr>
          <tr><td align=\"center\" style=\"padding:4px 0 24px;\">{}</td></tr>",
            escape_html(first),
            button_html(link, "See What You Sent"),
        ),
    )
}

/// What the team hears about the quote.
struct Notice<'a> {
    customer: &'a str,
    phone: &'a str,
    email: &'a str,
    prices: &'a [OnlinePrice],
    /// The quote in the app.
    link: &'a str,
    emailed: bool,
    /// How the customer asked to be contacted about the quote.
    contact_by: Option<OnlineContactBy>,
}

impl Notice<'_> {
    /// The customer and each color's price, one per line.
    fn details(&self) -> String {
        let mut text = format!(
            "Name: {}\nPhone: {}\nEmail: {}",
            self.customer, self.phone, self.email
        );
        for price in self.prices {
            let _ = write!(
                text,
                "\n{} {}: {}",
                price.product,
                price.color,
                price_text(price)
            );
        }
        if let Some(how) = self.contact_by {
            let how = match how {
                OnlineContactBy::Call => "a phone call",
                OnlineContactBy::Text => "a text message",
                OnlineContactBy::Email => "an email",
            };
            let _ = write!(
                text,
                "\nContact: the customer asked for {how} about the quote."
            );
        }
        if !self.emailed {
            text.push_str(
                "\nThe quote could not be emailed to the customer: email it from the quote.",
            );
        }
        text
    }

    /// The same as an email: the branded card with a button to the quote.
    fn html(&self, branding: &Branding, title: &str) -> String {
        let rows = self.details().lines().fold(String::new(), |mut out, line| {
            let (label, value) = line.split_once(": ").unwrap_or(("", line));
            let _ = write!(
                out,
                "<tr><td style=\"{EMAIL_FONT}font-size:14px;color:#71717a;padding:4px 12px 4px 0;\">{}</td><td style=\"{EMAIL_FONT}font-size:15px;color:#18181b;padding:4px 0;font-weight:600;\">{}</td></tr>",
                escape_html(label),
                escape_html(value)
            );
            out
        });
        card_html(
            branding,
            &format!(
                "<tr><td align=\"center\" style=\"{EMAIL_FONT}font-size:22px;color:#18181b;padding-bottom:24px;\">{}</td></tr>
              <tr><td style=\"padding-bottom:24px;\"><table role=\"presentation\" cellpadding=\"0\" cellspacing=\"0\">{rows}</table></td></tr>
              <tr><td align=\"center\" style=\"padding:4px 0;\">{}</td></tr>",
                escape_html(title),
                button_html(self.link, "Open the Quote"),
            ),
        )
    }
}

fn staff_email(to: Vec<String>, subject: String, body: String) -> OutgoingEmail {
    OutgoingEmail {
        from_name: None,
        from_address: NO_REPLY.into(),
        to,
        cc: Vec::new(),
        bcc: Vec::new(),
        subject,
        body,
        signature: None,
        reply_to: Vec::new(),
        in_reply_to: None,
        references: Vec::new(),
        attachments: Vec::new(),
        transactional: true,
    }
}

async fn keep_lead_message(
    state: &AppState,
    customer_id: i32,
    company_id: i32,
    chat_id: i64,
    message_id: i64,
) {
    if let Err(error) = sqlx::query(
        "INSERT INTO telegram_lead_messages (customer_id, company_id, chat_id, message_id) VALUES (?, ?, ?, ?)",
    )
    .bind(customer_id)
    .bind(company_id)
    .bind(chat_id)
    .bind(message_id)
    .execute(&state.pool)
    .await
    {
        tracing::error!(?error, customer_id, chat_id, "could not keep the telegram lead message");
    }
}

/// The sales managers get the lead in Telegram with a button per sales rep
/// (this month's leads on it) to assign it, as the lead pipeline sends new
/// leads, plus one to open the quote. Managers who could not be reached in
/// Telegram are emailed when none was. Their phones get a `lead` push, and
/// managers with the app get only that.
async fn notify_managers(
    state: &AppState,
    company_id: i32,
    customer_id: i32,
    title: &str,
    notice: &Notice<'_>,
    branding: &Branding,
) {
    let users = match sales_users(&state.pool, company_id).await {
        Ok(users) => users,
        Err(error) => {
            tracing::error!(
                ?error,
                company_id,
                "could not load the sales team for an online quote"
            );
            return;
        }
    };
    let assign: Vec<Value> = users
        .iter()
        .filter(|u| u.position_id == SALES_REP)
        .map(|u| {
            json!({
                "text": format!("{}: {}", u.name.as_deref().unwrap_or("Unknown"), u.mtd_lead_count),
                "callback_data": format!("assign:{customer_id}:{}", u.user_position_id),
            })
        })
        .collect();
    let mut rows: Vec<Vec<Value>> = assign.chunks(2).map(<[Value]>::to_vec).collect();
    rows.push(vec![
        json!({ "text": "Open the quote", "url": notice.link }),
    ]);
    let text = format!("{title}\n{}\nChoose a salesperson.", notice.details());
    let mut managers: Vec<&SalesUser> = Vec::new();
    for user in users.iter().filter(|u| u.position_id == SALES_MANAGER) {
        if !managers.iter().any(|m| m.id == user.id) {
            managers.push(user);
        }
    }
    let manager_ids: Vec<i32> = managers.iter().map(|m| m.id).collect();
    push_lead_logged(
        state,
        company_id,
        &manager_ids,
        LeadKind::Quote,
        customer_id,
        None,
    )
    .await;
    // Managers with the app get it there only.
    let with_app = users_with_app(&state.pool, company_id, &manager_ids).await;
    let mut reached = !with_app.is_empty();
    for manager in &managers {
        if with_app.contains(&manager.id) {
            continue;
        }
        let Some(chat_id) = manager.telegram_id else {
            continue;
        };
        if let Some(message_id) = state
            .telegram
            .send_with_keyboard(Bot::Leads, chat_id, &text, &rows)
            .await
        {
            reached = true;
            keep_lead_message(state, customer_id, company_id, chat_id, message_id).await;
        }
    }
    if reached {
        return;
    }
    let to: Vec<String> = managers
        .iter()
        .filter_map(|m| {
            m.email
                .as_deref()
                .map(str::trim)
                .filter(|e| e.contains('@'))
        })
        .map(str::to_string)
        .collect();
    if to.is_empty() {
        tracing::error!(
            company_id,
            customer_id,
            "an online quote reached no sales manager"
        );
        return;
    }
    let email = staff_email(
        to,
        format!("{title}: {}", notice.customer),
        notice.html(branding, title),
    );
    if let Err(error) = state.webhooks.send_email(&email).await {
        tracing::error!(
            ?error,
            company_id,
            customer_id,
            "could not email the managers an online quote"
        );
    }
}

/// The customer's sales rep gets the quote in Telegram (lead alerts), else
/// by email, and a `lead` push on their phones (only that with the app).
async fn notify_rep(
    state: &AppState,
    company_id: i32,
    customer_id: i32,
    rep: &Staff,
    notice: &Notice<'_>,
    branding: &Branding,
) {
    let title = format!("{} finished the online quote", notice.customer);
    push_lead_logged(
        state,
        company_id,
        &[rep.id],
        LeadKind::Quote,
        customer_id,
        None,
    )
    .await;
    // A rep with the app gets it there only.
    if users_with_app(&state.pool, company_id, &[rep.id])
        .await
        .contains(&rep.id)
    {
        return;
    }
    if let Some(chat_id) = rep.telegram_id {
        let text = format!("{title}.\n{}", notice.details());
        let rows = vec![vec![
            json!({ "text": "Open the quote", "url": notice.link }),
        ]];
        if let Some(message_id) = state
            .telegram
            .send_with_keyboard(Bot::Leads, chat_id, &text, &rows)
            .await
        {
            keep_lead_message(state, customer_id, company_id, chat_id, message_id).await;
            return;
        }
    }
    let Some(address) = rep.email() else {
        tracing::warn!(
            user = rep.id,
            "the sales rep of an online quote has no email"
        );
        return;
    };
    let email = staff_email(
        vec![address.to_string()],
        title.clone(),
        notice.html(branding, &title),
    );
    if let Err(error) = state.webhooks.send_email(&email).await {
        tracing::error!(
            ?error,
            user = rep.id,
            "could not email the sales rep an online quote"
        );
    }
}

/// Send an online quote
///
/// Saves the drawing as a quote of the customer (active, revision 1, priced
/// like `POST .../online-quote/prices`, the price list's tax, payment terms
/// and expiration), emails the customer a link to it and tells the team. A
/// customer the company does not know by this email or phone (last 10
/// digits) becomes a lead (source `leads`, from the `website`) and the quote
/// has no sales rep; the sales managers get it in Telegram (lead alerts) with
/// a button per sales rep to assign it. A known customer's quote gets their
/// sales rep, who is told in Telegram or by email; a known customer without
/// one goes to the managers. When the email cannot be sent the quote is still
/// saved (`emailed` false) and the team is told. At most 3 quotes an address
/// a day. No session needed.
#[crate::openapi::path(
    post,
    path = "/v1/public/companies/{company_id}/online-quotes",
    operation_id = "create_online_quote",
    tag = "public",
    params(("company_id" = i32, Path, description = "Company id")),
    request_body = OnlineQuoteInput,
    responses(
        (status = 201, description = "Saved (and emailed unless `emailed` is false)", body = OnlineQuoteSent),
        (status = 400, description = "No name, an email that is not one, a phone without 10 digits, or a drawing or color `POST .../online-quote/prices` refuses", body = ErrorBody),
        (status = 403, description = "The plan lacks the Quotes add-on (`addon`: `quotes`)", body = AddonRequiredBody),
        (status = 404, description = "No such company", body = ErrorBody),
        (status = 409, description = "The company has no prices for online quotes", body = ErrorBody),
        (status = 429, description = "This address already got 3 online quotes today, or the company 30 this hour", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    )
)]
#[allow(clippy::too_many_lines)]
pub async fn create(
    State(state): State<AppState>,
    Path(company_id): Path<i32>,
    JsonBody(input): JsonBody<OnlineQuoteInput>,
) -> ApiResult<(StatusCode, Json<OnlineQuoteSent>)> {
    require_addon(&state.pool, company_id, AddonKey::Quotes).await?;
    if input
        .website
        .as_deref()
        .is_some_and(|w| !w.trim().is_empty())
    {
        // Only a bot fills the hidden field: it is told the quote went out.
        return Ok((
            StatusCode::CREATED,
            Json(OnlineQuoteSent {
                email: input.email.trim().to_string(),
                emailed: true,
                prices: Vec::new(),
            }),
        ));
    }
    let (name, email, phone) = contact(&input)?;
    let company = company_of(&state.pool, company_id).await?;
    let (offer, colors, sinks) = ready_offer(&state.pool, company_id).await?;
    let extras = checked_extras(&state.pool, company_id, &input.extras).await?;
    let drawing = build_drawing(
        &offer,
        &colors,
        &sinks,
        &input.pieces,
        &input.colors,
        &extras,
    )?;
    let (prices, totals) = price_drawing(&offer, &drawing)?;
    throttle(&state.pool, company_id, &email).await?;

    let known = known_customer(&state.pool, company_id, &email, &phone).await?;
    let rep = match &known {
        Some(customer) => rep_of(&state.pool, company_id, customer.sales_rep).await?,
        None => None,
    };
    let expiration: Option<String> =
        sqlx::query_scalar("SELECT DATE_FORMAT(DATE_ADD(CURDATE(), INTERVAL ? DAY), '%Y-%m-%d')")
            .bind(offer.expiration_days)
            .fetch_one(&state.pool)
            .await?;
    let company_name = company.name();
    let subject = if company_name.is_empty() {
        "We got your countertop details".to_string()
    } else {
        format!("{company_name} got your countertop details")
    };
    let portal = token()?;
    let email_token = token()?;
    let doc = drawing.doc.to_string();

    let mut tx = state.pool.begin().await?;
    let customer_id = match &known {
        Some(customer) => customer.id,
        None => insert_lead(&mut tx, company_id, &name, &email, &phone).await?,
    };
    let quote_name = known
        .as_ref()
        .and_then(|c| c.name.as_deref())
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .unwrap_or(&name)
        .to_string();
    let quote_id = sqlx::query(
        "INSERT INTO quotes (customer_id, quote_name, quote_type, created_date, sales_rep, company_id,
                             salesperson_id, created_by, price_list_id, price_list_revision_id,
                             expiration_date, deal_id, draft_drawing_json, status, updated_at,
                             phone, email, tax_pct, payment_terms_json, price_snapshot_json,
                             portal_token, current_step, current_revision)
         VALUES (?, ?, '', NOW(), ?, ?, ?, NULL, ?, ?, ?, NULL, ?, 'active', NOW(), ?, ?, ?, ?, ?, ?, 1, 1)",
    )
    .bind(customer_id)
    .bind(&quote_name)
    .bind(rep.as_ref().map_or("", Staff::name))
    .bind(company_id)
    .bind(rep.as_ref().map(|r| r.id))
    .bind(offer.price_list_id)
    .bind(offer.revision_id)
    .bind(&expiration)
    .bind(&doc)
    .bind(&phone)
    .bind(&email)
    .bind(offer.tax_pct)
    .bind(offer.terms.as_ref().map(|t| json!({ "terms": t }).to_string()))
    .bind(offer.prices.to_string())
    .bind(&portal)
    .execute(&mut *tx)
    .await?
    .last_insert_id();
    sqlx::query(
        "INSERT INTO quote_revisions
           (quote_id, revision_number, drawing_json, options_json, totals_json, price_list_revision_id, notes, created_by)
         VALUES (?, 1, ?, ?, ?, ?, ?, NULL)",
    )
    .bind(quote_id)
    .bind(&doc)
    .bind(drawing.doc["colorOptions"].to_string())
    .bind(Value::Array(totals).to_string())
    .bind(offer.revision_id)
    .bind(REVISION_NOTE)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO quote_emails (quote_id, revision_number, token, recipients, subject, form, sent_by, expires_at)
         VALUES (?, 1, ?, ?, ?, NULL, NULL, ?)",
    )
    .bind(quote_id)
    .bind(&email_token)
    .bind(&email)
    .bind(&subject)
    .bind(&expiration)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    let subdomain = company_subdomain(&state.pool, company_id).await?;
    let origin = app_origin(&state.app_url, subdomain.as_deref());
    let branding = Branding {
        company_name: company_name.clone(),
        logo_url: company.logo_url.clone(),
        sender_name: company_name.clone(),
    };
    // Replies reach the customer's rep, else the sales managers.
    let reply_to: Vec<String> = match rep.as_ref().and_then(Staff::email) {
        Some(address) => vec![address.to_string()],
        None => sales_users(&state.pool, company_id)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|u| u.position_id == SALES_MANAGER)
            .filter_map(|u| {
                u.email
                    .map(|e| e.trim().to_string())
                    .filter(|e| e.contains('@'))
            })
            .fold(Vec::new(), |mut out, address| {
                if !out.contains(&address) {
                    out.push(address);
                }
                out
            }),
    };
    // The button opens the customer's own page (unlocked with an emailed
    // code), where their online quotes stay.
    let view_id: Option<String> =
        sqlx::query_scalar("SELECT BIN_TO_UUID(view_id) FROM customers WHERE id = ?")
            .bind(customer_id)
            .fetch_optional(&state.pool)
            .await?
            .flatten();
    let customer_link = view_id.map_or_else(
        || format!("{origin}/quote-portal/{portal}?e={email_token}"),
        |view| format!("{origin}/customer/{company_id}/{view}?quote={quote_id}"),
    );
    let html = customer_html(&branding, &name, &input.colors, &customer_link);
    let outgoing = OutgoingEmail {
        from_name: Some(company_name.clone()).filter(|n| !n.is_empty()),
        from_address: NO_REPLY.into(),
        to: vec![email.clone()],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject,
        body: html.clone(),
        signature: None,
        reply_to,
        in_reply_to: None,
        references: Vec::new(),
        attachments: Vec::new(),
        transactional: true,
    };
    let emailed = match state.webhooks.send_email(&outgoing).await {
        Ok(_) => {
            sqlx::query("UPDATE quote_emails SET body_html = ? WHERE token = ?")
                .bind(&html)
                .bind(&email_token)
                .execute(&state.pool)
                .await?;
            true
        }
        Err(error) => {
            tracing::warn!(?error, quote = quote_id, "online quote email failed");
            sqlx::query("DELETE FROM quote_emails WHERE token = ?")
                .bind(&email_token)
                .execute(&state.pool)
                .await?;
            false
        }
    };

    // The quote is saved: telling the team is best effort.
    let link = format!("{origin}/employee/quote/{quote_id}");
    let notice = Notice {
        customer: &name,
        phone: &phone,
        email: &email,
        prices: &prices,
        link: &link,
        emailed,
        contact_by: input.contact_by,
    };
    match (&known, &rep) {
        (None, _) => {
            notify_managers(
                &state,
                company_id,
                customer_id,
                "New lead from the online quote",
                &notice,
                &branding,
            )
            .await;
            state.webhooks.sync_contacts(company_id, customer_id).await;
        }
        (Some(_), Some(rep)) => {
            notify_rep(&state, company_id, customer_id, rep, &notice, &branding).await;
        }
        (Some(_), None) => {
            let title = format!(
                "{} finished the online quote (in the CRM already, no sales rep)",
                notice.customer
            );
            notify_managers(&state, company_id, customer_id, &title, &notice, &branding).await;
        }
    }
    Ok((
        StatusCode::CREATED,
        Json(OnlineQuoteSent {
            email,
            emailed,
            prices,
        }),
    ))
}

/// One of a customer's online quotes, as their own page shows it.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CustomerOnlineQuote {
    /// Quote id.
    pub id: i32,
    /// Its name.
    pub name: String,
    /// When it was made (ISO 8601, UTC).
    pub created_date: Option<String>,
    /// Prices good until (YYYY-MM-DD).
    pub expiration_date: Option<String>,
    /// The drawing (the quote document, no supplier costs): its `counters`
    /// are the pieces.
    #[schema(value_type = Object)]
    pub doc: Value,
    /// Each color's total, as on the quote; no breakdown.
    pub prices: Vec<OnlinePrice>,
}

/// The customer's quotes made with the online quote, newest first, each
/// priced as the quote is (its price snapshot and tax).
pub async fn customer_online_quotes(
    pool: &MySqlPool,
    company_id: i32,
    customer_id: i32,
) -> ApiResult<Vec<CustomerOnlineQuote>> {
    let rows: Vec<(i32, Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT q.id, q.quote_name, DATE_FORMAT(q.created_date, '%Y-%m-%dT%H:%i:%sZ'),
                DATE_FORMAT(q.expiration_date, '%Y-%m-%d')
           FROM quotes q
          WHERE q.customer_id = ? AND q.company_id = ? AND q.deleted_at IS NULL
            AND EXISTS (SELECT 1 FROM quote_revisions r
                         WHERE r.quote_id = q.id AND r.revision_number = 1 AND r.notes = ?)
          ORDER BY q.created_date DESC, q.id DESC",
    )
    .bind(customer_id)
    .bind(company_id)
    .bind(REVISION_NOTE)
    .fetch_all(pool)
    .await?;
    let mut quotes = Vec::with_capacity(rows.len());
    for (id, name, created_date, expiration_date) in rows {
        let Some(quote) = crate::quote_pricing::load_company_quote(pool, id, company_id).await?
        else {
            continue;
        };
        let (price_list, _) = crate::quote_pricing::pricing_for_quote(pool, &quote).await?;
        let doc = quote.doc();
        let options = price_quote(&doc, &price_list, quote.options());
        let prices = doc
            .color_options
            .iter()
            .zip(&options)
            .map(|(option, priced)| {
                let picked = option.by_area.get(AREA_ID);
                OnlinePrice {
                    product: picked.map(|p| p.product.clone()).unwrap_or_default(),
                    color: picked.map(|p| p.color.clone()).unwrap_or_default(),
                    total: (!priced.missing && priced.total.is_finite())
                        .then(|| js_round(priced.total * 100.0) / 100.0),
                }
            })
            .collect();
        let stored = quote
            .draft_drawing_json
            .as_deref()
            .and_then(|t| serde_json::from_str::<Value>(t).ok())
            .unwrap_or(Value::Null);
        quotes.push(CustomerOnlineQuote {
            id,
            name: name.unwrap_or_default(),
            created_date,
            expiration_date,
            doc: crate::quote_pricing::customer_doc(&stored),
            prices,
        });
    }
    Ok(quotes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emails_are_checked_loosely() {
        assert!(looks_like_email("jane@example.com"));
        assert!(looks_like_email("j.doe+kitchen@mail.co.uk"));
        assert!(!looks_like_email("jane@example"));
        assert!(!looks_like_email("jane example@x.com"));
        assert!(!looks_like_email("a@b.com, c@d.com"));
        assert!(!looks_like_email("@example.com"));
        assert!(!looks_like_email("jane@@example.com"));
        assert!(!looks_like_email("jane@example..com"));
    }

    #[test]
    fn ids_are_short_and_plain() {
        assert!(valid_id("counter-ab12"));
        assert!(!valid_id(""));
        assert!(!valid_id("a b"));
        assert!(!valid_id(&"x".repeat(41)));
    }

    #[test]
    fn the_cooktop_falls_back_field_by_field() {
        let set = json!({ "defaultCooktopCutout": { "cutoutWidth": 30, "burners": 0 } });
        assert_eq!(
            cooktop_cutout(&set),
            OnlineCooktop {
                width: 30.0,
                depth: 14.0,
                burners: 4
            }
        );
    }
}
