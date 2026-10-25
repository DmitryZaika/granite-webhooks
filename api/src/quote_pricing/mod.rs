//! Quote prices and sales, ported from the CRM.
//!
//! A signed contract sells its quote (`quoteSale.server.ts`): the quote
//! document, the price list it prices with (its snapshot, the company's
//! stock, the extras catalog) and `priceQuote`.
//!
//! The numbers must come out as the CRM's do, to the cent and the slab: the
//! arithmetic is written as the JavaScript is (no fused multiply-add, the
//! same comparisons and loops), so the lints that would rewrite it are off.
#![allow(
    clippy::suboptimal_flops,
    clippy::manual_midpoint,
    clippy::while_float,
    clippy::neg_cmp_op_on_partial_ord,
    clippy::many_single_char_names,
    clippy::float_cmp,
    clippy::suspicious_operation_groupings
)]

pub mod doc;
pub mod edit;
pub mod geometry;
pub mod js;
pub mod price_list;
pub mod pricing;
pub mod takeoff;

use crate::error::ApiResult;
use crate::routes::events::history::{History, record};
use doc::QuoteDoc;
use price_list::{Stock, StockSink, StockStone};
use pricing::{PriceOptions, price_quote};
use serde_json::Value;
use sqlx::MySqlPool;
use std::collections::{HashMap, HashSet};
use takeoff::js_round;

/// A quote, as pricing and selling it need.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Quote {
    pub id: i32,
    pub company_id: i32,
    pub customer_id: i32,
    pub deal_id: Option<i64>,
    pub quote_name: Option<String>,
    pub salesperson_id: Option<i32>,
    pub created_by: Option<i32>,
    pub price_list_id: Option<i32>,
    pub price_list_revision_id: Option<i32>,
    pub tax_pct: f64,
    pub discount_pct: f64,
    pub discount_amount: f64,
    pub draft_drawing_json: Option<String>,
    pub customer_accepted_option_id: Option<i32>,
}

const QUOTE_SELECT: &str =
    "SELECT id, company_id, customer_id, CAST(deal_id AS SIGNED) AS deal_id, quote_name,
        salesperson_id, created_by, price_list_id, price_list_revision_id,
        CAST(tax_pct AS DOUBLE) AS tax_pct, CAST(discount_pct AS DOUBLE) AS discount_pct,
        CAST(discount_amount AS DOUBLE) AS discount_amount,
        CAST(draft_drawing_json AS CHAR) AS draft_drawing_json, customer_accepted_option_id
   FROM quotes";

/// `loadCompanyQuote`.
pub async fn load_company_quote(
    pool: &MySqlPool,
    quote_id: i32,
    company_id: i32,
) -> ApiResult<Option<Quote>> {
    Ok(sqlx::query_as::<_, Quote>(sqlx::AssertSqlSafe(format!(
        "{QUOTE_SELECT} WHERE id = ? AND company_id = ? AND deleted_at IS NULL"
    )))
    .bind(quote_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?)
}

fn json_value(text: Option<&str>) -> Value {
    text.and_then(|t| serde_json::from_str(t).ok())
        .unwrap_or(Value::Null)
}

impl Quote {
    /// `docFromQuote`.
    pub fn doc(&self) -> QuoteDoc {
        QuoteDoc::from_stored(&json_value(self.draft_drawing_json.as_deref()))
    }

    pub const fn options(&self) -> PriceOptions {
        PriceOptions {
            discount_pct: self.discount_pct,
            discount_amount: self.discount_amount,
            tax_pct: self.tax_pct,
        }
    }
}

/// JS `Number(v)` for a column read as text.
fn number(text: Option<String>) -> Option<f64> {
    Some(js::to_number(&Value::String(text?)))
}

/// `loadPricingStock` + `loadExtrasCatalog`.
#[allow(clippy::too_many_lines, clippy::items_after_statements)]
pub async fn load_stock(pool: &MySqlPool, company_id: i32) -> ApiResult<Stock> {
    type StoneRow = (
        i32,
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<i64>,
        i64,
    );
    let stones: Vec<StoneRow> = sqlx::query_as(
        "SELECT s.id, COALESCE(s.name, ''), COALESCE(s.type, ''), CAST(s.retail_price AS CHAR),
                CAST(s.level AS CHAR), CAST(s.length AS CHAR), CAST(s.width AS CHAR),
                CAST(s.is_display AS SIGNED), CAST(s.regular_stock AS SIGNED),
                CAST(s.url IS NOT NULL AND LOWER(TRIM(s.url)) NOT IN ('', 'undefined', 'null') AS SIGNED)
           FROM stones s
          WHERE s.company_id = ? AND s.deleted_at IS NULL
          ORDER BY s.name",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    let with_slabs: Vec<(i32, i64)> = sqlx::query_as(
        "SELECT si.stone_id, COUNT(*)
           FROM slab_inventory si
           JOIN stones s ON s.id = si.stone_id
          WHERE s.company_id = ? AND si.sale_id IS NULL
            AND si.cut_date IS NULL AND si.deleted_at IS NULL
          GROUP BY si.stone_id",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    let links: Vec<(i32, i32)> = sqlx::query_as(
        "SELECT l.stone_id, l.source_stone_id
           FROM stone_slab_links l
           JOIN stones s ON s.id = l.stone_id
          WHERE s.company_id = ?",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    let slab_count: HashMap<i32, i64> = with_slabs.into_iter().collect();
    let mut sources: HashMap<i32, Vec<i32>> = HashMap::new();
    for (stone, source) in links {
        sources.entry(stone).or_default().push(source);
    }
    let mut all: Vec<(StockStone, bool)> = Vec::new();
    let mut available = HashSet::new();
    for (id, name, kind, retail, level, length, width, display, regular, image) in stones {
        let own = sources.get(&id).map(Vec::as_slice).unwrap_or_default();
        #[allow(clippy::cast_precision_loss)]
        let slabs_left = std::iter::once(&id)
            .chain(own)
            .map(|i| slab_count.get(i).copied().unwrap_or(0))
            .sum::<i64>() as f64;
        let regular_stock = regular == Some(1);
        let stocked = regular_stock
            || slab_count.contains_key(&id)
            || own.iter().any(|i| slab_count.contains_key(i));
        let stone = StockStone {
            name,
            kind,
            retail_price: number(retail),
            level: number(level),
            length: number(length),
            width: number(width),
            is_display: display.map(|d| d == 1),
            has_image: image == 1,
            slabs_left: Some(slabs_left),
            regular_stock,
        };
        if stocked {
            available.insert(price_list::available_key(&stone));
        }
        all.push((stone, stocked));
    }
    // `inStockFirst`: per name, stones in stock first, then the highest price.
    let name_key = |s: &StockStone| {
        format!(
            "{}|{}",
            s.kind.trim().to_lowercase(),
            s.name.trim().to_lowercase()
        )
    };
    all.sort_by(|a, b| {
        name_key(&a.0)
            .cmp(&name_key(&b.0))
            .then(b.1.cmp(&a.1))
            .then(
                b.0.retail_price
                    .unwrap_or(0.0)
                    .partial_cmp(&a.0.retail_price.unwrap_or(0.0))
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
    });

    type SinkRow = (
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let sinks: Vec<SinkRow> = sqlx::query_as(
        "SELECT COALESCE(t.name, ''), COALESCE(t.type, ''), CAST(t.length AS CHAR), CAST(t.width AS CHAR),
                CAST(t.retail_price AS CHAR)
           FROM sink_type t
           LEFT JOIN sinks ON sinks.sink_type_id = t.id AND sinks.is_deleted = 0 AND sinks.slab_id IS NULL
          WHERE t.company_id = ? AND t.is_deleted = 0
          GROUP BY t.id, t.name, t.type, t.length, t.width, t.retail_price, t.regular_stock
         HAVING COUNT(sinks.id) > 0 OR COALESCE(t.regular_stock, 0) <> 0
          ORDER BY t.name ASC",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    type CutoutRow = (String, Option<i64>, Option<i64>, Option<i64>, Option<i64>);
    let cutouts: Vec<CutoutRow> = sqlx::query_as(
        "SELECT name, CAST(cutout_type AS SIGNED), CAST(cutout_shape AS SIGNED),
                CAST(faucet_hole_count AS SIGNED), CAST(is_display AS SIGNED)
           FROM sink_type WHERE company_id = ? AND is_deleted = 0",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let by_name: HashMap<String, CutoutRow> = cutouts
        .into_iter()
        .map(|r| (price_list::key(&r.0), r))
        .collect();
    #[allow(clippy::cast_precision_loss)]
    let small = |v: Option<i64>| v.map(|v| v as f64);
    let sinks = sinks
        .into_iter()
        .map(|(name, kind, length, width, retail)| {
            let cut = by_name.get(&price_list::key(&name));
            StockSink {
                length: number(length),
                width: number(width),
                retail_price: number(retail),
                cutout_type: cut.and_then(|c| small(c.1)),
                cutout_shape: cut.and_then(|c| small(c.2)),
                faucet_hole_count: cut.and_then(|c| small(c.3)),
                is_display: cut.and_then(|c| c.4).map(|d| d == 1),
                name,
                kind,
            }
        })
        .collect();
    let types: Vec<String> =
        sqlx::query_scalar("SELECT name FROM stone_types WHERE company_id = ? ORDER BY name ASC")
            .bind(company_id)
            .fetch_all(pool)
            .await
            .unwrap_or_default();
    let extras: Vec<(i32, Option<String>)> = sqlx::query_as(
        "SELECT id, CAST(item_json AS CHAR) FROM quote_extras
          WHERE company_id = ? AND deleted_at IS NULL ORDER BY sequence, id",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let extras = extras
        .into_iter()
        .filter_map(|(id, text)| {
            let mut item = json_value(text.as_deref());
            if let Value::String(inner) = &item {
                item = serde_json::from_str(inner).ok()?;
            }
            item.get("name")?;
            Some((i64::from(id), item))
        })
        .collect();
    Ok(Stock {
        stones: all.into_iter().map(|(s, _)| s).collect(),
        available,
        sinks,
        types,
        extras,
    })
}

/// `pricingForQuote(quote).priceList`: the snapshot the quote was made with
/// (taken now from its revision if it has none), with today's new choices.
pub async fn price_list_for_quote(pool: &MySqlPool, quote: &Quote) -> ApiResult<Value> {
    Ok(pricing_for_quote(pool, quote).await?.0)
}

/// `pricingForQuote`: the quote's prices (its snapshot with today's new
/// choices) and `latest`, the same list as it prices today.
pub async fn pricing_for_quote(pool: &MySqlPool, quote: &Quote) -> ApiResult<(Value, Value)> {
    let snapshot: Option<Value> = sqlx::query_scalar::<_, Option<String>>(
        "SELECT CAST(price_snapshot_json AS CHAR) FROM quotes WHERE id = ?",
    )
    .bind(quote.id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .flatten()
    .map(|t| json_value(Some(&t)))
    .filter(|v| js::truthy(Some(v)))
    .and_then(|v| price_list::parse(&v));
    let list: Option<Option<Value>> = match quote.price_list_id {
        Some(id) => sqlx::query_scalar::<_, Option<String>>(
            "SELECT CAST(r.info_json AS CHAR) FROM price_lists pl
               LEFT JOIN price_list_revisions r ON r.id = pl.current_revision_id
              WHERE pl.id = ? AND pl.company_id = ? AND pl.deleted_at IS NULL",
        )
        .bind(id)
        .bind(quote.company_id)
        .fetch_optional(pool)
        .await?
        .map(|info| price_list::parse(&json_value(info.as_deref()))),
        None => None,
    };
    let revision: Option<Value> = match quote.price_list_revision_id {
        Some(id) => sqlx::query_scalar::<_, Option<String>>(
            "SELECT CAST(r.info_json AS CHAR) FROM price_list_revisions r
               JOIN price_lists pl ON pl.id = r.price_list_id
              WHERE r.id = ? AND pl.company_id = ?",
        )
        .bind(id)
        .bind(quote.company_id)
        .fetch_optional(pool)
        .await?
        .and_then(|info| price_list::parse(&json_value(info.as_deref()))),
        None => None,
    };
    let stock = load_stock(pool, quote.company_id).await?;
    let priced = |info: Option<&Value>| {
        let empty = price_list::empty();
        price_list::sync_with_stock(info.unwrap_or(&empty), &stock)
    };
    let latest = priced(list.flatten().as_ref().or(revision.as_ref()));
    let taken = if let Some(snapshot) = snapshot {
        snapshot
    } else {
        let taken = priced(revision.as_ref());
        // Before the snapshot migration quotes keep pricing live.
        let _ = sqlx::query("UPDATE quotes SET price_snapshot_json = ? WHERE id = ?")
            .bind(taken.to_string())
            .bind(quote.id)
            .execute(pool)
            .await;
        taken
    };
    Ok((price_list::with_live_choices(&taken, &latest), latest))
}

fn lower(text: &str) -> String {
    text.trim().to_lowercase()
}

/// `isSoldOutColor`.
fn is_sold_out(price_list: &Value, product: &str, color: &str) -> bool {
    if color.trim().is_empty() {
        return false;
    }
    js::arr(Some(price_list), "arrProducts")
        .iter()
        .find(|p| {
            lower(p.get("name").and_then(Value::as_str).unwrap_or_default()) == lower(product)
        })
        .is_some_and(|p| {
            js::arr(Some(p), "arrVariants").iter().any(|v| {
                js::truthy(v.get("soldOut"))
                    && lower(v.get("name").and_then(Value::as_str).unwrap_or_default())
                        == lower(color)
            })
        })
}

/// `optionSoldOutColors`: sold-out colors on one option, once each.
pub fn option_sold_out_colors(doc: &QuoteDoc, price_list: &Value, index: usize) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let Some(option) = doc.color_options.get(index) else {
        return found;
    };
    for area in &doc.areas {
        let Some(picked) = option.by_area.get(&area.id) else {
            continue;
        };
        let color = picked.color.trim();
        if is_sold_out(price_list, &picked.product, color)
            && !found.iter().any(|c| lower(c) == lower(color))
        {
            found.push(color.to_string());
        }
    }
    found
}

/// `soldOutColorsInDoc`: sold-out colors anywhere on the quote, once each.
pub fn sold_out_colors(doc: &QuoteDoc, price_list: &Value) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for i in 0..doc.color_options.len() {
        for color in option_sold_out_colors(doc, price_list, i) {
            if !found.iter().any(|c| lower(c) == lower(&color)) {
                found.push(color);
            }
        }
    }
    found
}

/// `soldOutMessage`.
pub fn sold_out_message(colors: &[String]) -> String {
    let names = colors.join(", ");
    if colors.len() == 1 {
        format!("{names} is sold out and cannot be on a quote. Pick another color.")
    } else {
        format!("{names} are sold out and cannot be on a quote. Pick other colors.")
    }
}

/// `sinkShapeByCode`.
pub(crate) fn sink_shape_by_code(code: f64) -> &'static str {
    const SPLITS: [&str; 5] = ["double", "60/40", "40/60", "70/30", "30/70"];
    const SQUARE: [&str; 5] = [
        "double-square",
        "60/40-square",
        "40/60-square",
        "70/30-square",
        "30/70-square",
    ];
    const ROUNDED: [&str; 5] = [
        "double-rounded",
        "60/40-rounded",
        "40/60-rounded",
        "70/30-rounded",
        "30/70-rounded",
    ];
    #[allow(clippy::cast_possible_truncation)]
    let code = if code.fract() == 0.0 { code as i64 } else { -1 };
    match code {
        8 => "square",
        9 => "rounded",
        2 => "oval",
        3..=7 => SPLITS[usize::try_from(code - 3).unwrap_or_default()],
        10..=19 => {
            let i = usize::try_from((code - 10) / 2).unwrap_or_default();
            if code % 2 == 0 { SQUARE[i] } else { ROUNDED[i] }
        }
        _ => "rectangle",
    }
}

/// `withCurrentSinkModels`: sinks placed from a sink model take the
/// model's current type and shape. `None` when nothing changed.
pub fn with_current_sink_models(doc: &Value, price_list: &Value) -> Option<Value> {
    let models = js::arr(Some(price_list), "arrSinkModels");
    let mut doc = doc.clone();
    let mut changed = false;
    for counter in doc.get_mut("counters")?.as_array_mut()? {
        let Some(runs) = counter.get_mut("runs").and_then(Value::as_array_mut) else {
            continue;
        };
        for run in runs {
            let Some(cutouts) = run.get_mut("cutouts").and_then(Value::as_array_mut) else {
                continue;
            };
            for cut in cutouts {
                let model_name = cut
                    .get("sinkModel")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if cut.get("kind").and_then(Value::as_str) != Some("sink") || model_name.is_empty()
                {
                    continue;
                }
                let Some(model) = models.iter().find(|m| {
                    lower(m.get("name").and_then(Value::as_str).unwrap_or_default())
                        == lower(model_name)
                }) else {
                    continue;
                };
                let sink_type = match js::num(Some(model), "sinkType") {
                    Some(c) if (c - 1.0).abs() < f64::EPSILON => "dropIn",
                    Some(c) if (c - 2.0).abs() < f64::EPSILON => "undermount",
                    Some(c) if (c - 3.0).abs() < f64::EPSILON => "farmer",
                    _ => cut
                        .get("sinkType")
                        .and_then(Value::as_str)
                        .unwrap_or("undermount"),
                }
                .to_string();
                let sink_shape =
                    sink_shape_by_code(js::num(Some(model), "sinkShape").unwrap_or(0.0));
                if cut.get("sinkType").and_then(Value::as_str) == Some(sink_type.as_str())
                    && cut.get("sinkShape").and_then(Value::as_str) == Some(sink_shape)
                {
                    continue;
                }
                changed = true;
                cut["sinkType"] = Value::String(sink_type);
                cut["sinkShape"] = Value::String(sink_shape.into());
            }
        }
    }
    changed.then_some(doc)
}

/// `customerQuoteDoc`: the quote as a customer may see it, without
/// supplier costs.
pub fn customer_doc(doc: &Value) -> Value {
    let mut doc = doc.clone();
    if let Some(options) = doc.get_mut("colorOptions").and_then(Value::as_array_mut) {
        for option in options {
            if let Some(areas) = option.get_mut("byArea").and_then(Value::as_object_mut) {
                for color in areas.values_mut() {
                    if let Some(color) = color.as_object_mut() {
                        color.remove("supplier");
                    }
                }
            }
        }
    }
    doc
}

/// The quote's total for one color option (what the quote shows), or none.
pub async fn option_total(
    pool: &MySqlPool,
    quote: &Quote,
    option_index: usize,
) -> ApiResult<Option<f64>> {
    let doc = quote.doc();
    if option_index >= doc.color_options.len() {
        return Ok(None);
    }
    let pl = price_list_for_quote(pool, quote).await?;
    let total = price_quote(&doc, &pl, quote.options())
        .get(option_index)
        .map(|o| o.total)
        .filter(|t| t.is_finite());
    Ok(total.map(|t| js_round(t * 100.0) / 100.0))
}

/// The option a quote email talks about: the one the customer took, else the
/// first; none when the quote has no options.
pub fn email_option_index(quote: &Quote, doc: &QuoteDoc) -> Option<usize> {
    let count = doc.color_options.len();
    quote
        .customer_accepted_option_id
        .and_then(|i| usize::try_from(i).ok())
        .filter(|i| *i < count)
        .or((count > 0).then_some(0))
}

/// Material and color of one option, once each across areas
/// ("Quartz Calacatta Laza, Granite Black Pearl"); none when nothing is picked.
pub fn option_color_text(doc: &QuoteDoc, index: usize) -> Option<String> {
    let option = doc.color_options.get(index)?;
    let mut names: Vec<String> = Vec::new();
    for area in &doc.areas {
        let Some(picked) = option.by_area.get(&area.id) else {
            continue;
        };
        let name = [picked.product.trim(), picked.color.trim()]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if !name.is_empty() && !names.iter().any(|n| lower(n) == lower(&name)) {
            names.push(name);
        }
    }
    (!names.is_empty()).then(|| names.join(", "))
}

/// What selling a quote did: its deal and the amount put on it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QuoteSale {
    pub deal_id: Option<i64>,
    pub amount: Option<f64>,
}

/// SET clause for marking a deal won (`DEAL_MARK_WON_SET`).
const DEAL_MARK_WON_SET: &str =
    "is_won = 1, lost_reason = NULL, due_date = NULL, updated_at = CURRENT_TIMESTAMP";

/// A new deal for a sale with no deal yet: already won, on the first stage
/// of the company's own default group. No drips start: the customer bought.
async fn create_won_deal(
    pool: &MySqlPool,
    quote: &Quote,
    user_id: Option<i32>,
    amount: Option<f64>,
) -> ApiResult<Option<i64>> {
    let sales_rep = quote.salesperson_id.or(quote.created_by).or(user_id);
    let list: Option<(i64, String)> = sqlx::query_as(
        "SELECT CAST(dl.id AS SIGNED), dl.name FROM groups_list g
           JOIN deals_list dl ON dl.group_id = g.id AND dl.deleted_at IS NULL
          WHERE g.deleted_at IS NULL
            AND ((g.company_id = ? AND g.is_default = 1) OR g.id = 1)
          ORDER BY g.id = 1 ASC, dl.position ASC
          LIMIT 1",
    )
    .bind(quote.company_id)
    .fetch_optional(pool)
    .await?;
    let Some((list_id, list_name)) = list else {
        return Ok(None);
    };
    let res = sqlx::query(
        "INSERT INTO deals (customer_id, status, list_id, position, user_id, amount, is_won, updated_at)
         VALUES (?, ?, ?, 0, ?, ?, 1, CURRENT_TIMESTAMP)",
    )
    .bind(quote.customer_id)
    .bind(&list_name)
    .bind(list_id)
    .bind(sales_rep)
    .bind(amount)
    .execute(pool)
    .await?;
    let deal_id = i64::try_from(res.last_insert_id()).unwrap_or_default();
    sqlx::query("INSERT INTO deal_stage_history (deal_id, list_id) VALUES (?, ?)")
        .bind(deal_id)
        .bind(list_id)
        .execute(pool)
        .await?;
    Ok(Some(deal_id))
}

/// `markQuoteSold`: the quote is sold with option `option_index` (none: not
/// known yet, the deal is still won) and its deal is won with that total.
pub async fn mark_quote_sold(
    pool: &MySqlPool,
    quote_id: i32,
    company_id: i32,
    option_index: Option<i32>,
    user_id: Option<i32>,
) -> ApiResult<Option<QuoteSale>> {
    let Some(quote) = load_company_quote(pool, quote_id, company_id).await? else {
        return Ok(None);
    };
    let amount = match option_index.and_then(|i| usize::try_from(i).ok()) {
        Some(i) => option_total(pool, &quote, i).await?,
        None => None,
    };
    // The quote's own deal, else the customer's open or won deal.
    let deal: Option<(i64, Option<i64>)> = sqlx::query_as(
        "SELECT CAST(d.id AS SIGNED), CAST(d.is_won AS SIGNED) FROM deals d
          WHERE d.deleted_at IS NULL
            AND (d.id = ? OR (d.customer_id = ? AND (d.is_won IS NULL OR d.is_won = 1)))
          ORDER BY d.id = ? DESC, d.updated_at DESC, d.id DESC
          LIMIT 1",
    )
    .bind(quote.deal_id.unwrap_or(0))
    .bind(quote.customer_id)
    .bind(quote.deal_id.unwrap_or(0))
    .fetch_optional(pool)
    .await?;
    let deal_id = match deal {
        Some((id, Some(1))) => {
            // Already won (e.g. on the deals board): only the amount changes.
            if let Some(amount) = amount {
                sqlx::query("UPDATE deals SET amount = ? WHERE id = ?")
                    .bind(amount)
                    .bind(id)
                    .execute(pool)
                    .await?;
            }
            Some(id)
        }
        Some((id, _)) => {
            let sql = format!(
                "UPDATE deals SET {DEAL_MARK_WON_SET}{} WHERE id = ? AND deleted_at IS NULL",
                if amount.is_some() { ", amount = ?" } else { "" }
            );
            let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
            if let Some(amount) = amount {
                q = q.bind(amount);
            }
            q.bind(id).execute(pool).await?;
            Some(id)
        }
        None => create_won_deal(pool, &quote, user_id, amount).await?,
    };
    sqlx::query(
        "UPDATE quotes
            SET status = ?, customer_accepted_option_id = ?, deal_id = COALESCE(?, deal_id)
          WHERE id = ? AND company_id = ?",
    )
    .bind(if option_index.is_none() {
        "accepted"
    } else {
        "converted"
    })
    .bind(option_index.or(quote.customer_accepted_option_id))
    .bind(deal_id)
    .bind(quote.id)
    .bind(quote.company_id)
    .execute(pool)
    .await?;
    add_waiting_jobs(pool, &quote, user_id).await?;
    Ok(Some(QuoteSale { deal_id, amount }))
}

/// A sold quote's template and installation: one unscheduled event on each
/// of those calendars, "not ready", for the schedule manager to date. Once
/// per quote: selling it again adds nothing.
async fn add_waiting_jobs(
    pool: &MySqlPool,
    quote: &Quote,
    user_id: Option<i32>,
) -> Result<(), sqlx::Error> {
    let Some(creator) = user_id.or(quote.salesperson_id).or(quote.created_by) else {
        return Ok(());
    };
    let added: Option<i32> = sqlx::query_scalar(
        "SELECT id FROM events WHERE quote_id = ? AND deleted_date IS NULL LIMIT 1",
    )
    .bind(quote.id)
    .fetch_optional(pool)
    .await?;
    if added.is_some() {
        return Ok(());
    }
    let calendars: Vec<i32> = sqlx::query_scalar(
        "SELECT id FROM calendars
          WHERE company_id = ? AND slug IN ('templates', 'installations') AND deleted_at IS NULL
          ORDER BY slug = 'installations', id",
    )
    .bind(quote.company_id)
    .fetch_all(pool)
    .await?;
    let customer: Option<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT NULLIF(TRIM(name), ''), NULLIF(TRIM(address), '') FROM customers
          WHERE id = ? AND company_id = ?",
    )
    .bind(quote.customer_id)
    .bind(quote.company_id)
    .fetch_optional(pool)
    .await?;
    let (name, address) = customer.unwrap_or_default();
    // Named after the customer, else the quote.
    let title: String = name
        .or_else(|| {
            quote
                .quote_name
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_else(|| format!("Customer #{}", quote.customer_id))
        .chars()
        .take(255)
        .collect();
    for calendar_id in calendars {
        let id = sqlx::query(
            "INSERT INTO events (title, location, start_date, end_date, all_day, unscheduled,
                color, status, created_user_id, customer_id, calendar_id, quote_id)
             VALUES (?, ?, UTC_TIMESTAMP(), UTC_TIMESTAMP(), 1, 1, 'blue', 'not_ready', ?, ?, ?, ?)",
        )
        .bind(&title)
        .bind(address.as_deref())
        .bind(creator)
        .bind(quote.customer_id)
        .bind(calendar_id)
        .bind(quote.id)
        .execute(pool)
        .await?
        .last_insert_id();
        if let Ok(event_id) = i32::try_from(id) {
            let line = History::new(event_id, quote.company_id, "created", None)
                .message("Added when the quote was sold");
            record(pool, &line).await;
        }
    }
    Ok(())
}

/// Same cleaning as the file name the quote page gives a contract PDF.
fn clean_file_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                ' '
            } else {
                c
            }
        })
        .collect::<String>()
        .trim()
        .to_string()
}

/// `quoteForContractFile`: the quote a signed file is the contract of
/// ("<contract form> - <quote name>.pdf").
pub fn quote_for_contract_file<'a, Q>(
    filename: &str,
    contract_forms: &[String],
    quotes: &'a [Q],
    name_of: impl Fn(&Q) -> Option<&str>,
) -> Option<&'a Q> {
    let len = filename.len();
    let pdf = len >= 4
        && filename.is_char_boundary(len - 4)
        && filename[len - 4..].eq_ignore_ascii_case(".pdf");
    let base = if pdf { &filename[..len - 4] } else { filename };
    let base = base.trim().to_lowercase();
    let forms: Vec<String> = std::iter::once("Contract")
        .chain(contract_forms.iter().map(String::as_str))
        .map(|f| clean_file_name(f).to_lowercase())
        .collect();
    quotes.iter().find(|q| {
        let name = name_of(q).filter(|n| !n.is_empty()).unwrap_or("Quote");
        let quote_name = clean_file_name(name).to_lowercase();
        forms
            .iter()
            .any(|form| base == clean_file_name(&format!("{form} - {quote_name}")))
    })
}

/// `sellQuoteForSignedContract`: a contract sent from a quote and signed
/// sells that quote. Errors are the caller's to report; the signature stands.
#[allow(clippy::items_after_statements)]
pub async fn sell_quote_for_signed_contract(
    pool: &MySqlPool,
    company_id: i32,
    customer_id: Option<i64>,
    filename: &str,
) -> ApiResult<()> {
    let Some(customer_id) = customer_id else {
        return Ok(());
    };
    type QuoteRow = (i32, Option<String>, Option<i32>, Option<String>);
    let quotes: Vec<QuoteRow> = sqlx::query_as(
        "SELECT id, quote_name, customer_accepted_option_id, CAST(draft_drawing_json AS CHAR) FROM quotes
          WHERE company_id = ? AND customer_id = ? AND deleted_at IS NULL
            AND status <> 'converted'
          ORDER BY updated_at DESC, id DESC",
    )
    .bind(company_id)
    .bind(customer_id)
    .fetch_all(pool)
    .await?;
    let forms: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM quote_forms
          WHERE company_id = ? AND deleted_at IS NULL
            AND JSON_UNQUOTE(JSON_EXTRACT(config_json, '$.kind')) = 'contract'
          ORDER BY sequence, name",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    let Some(quote) = quote_for_contract_file(filename, &forms, &quotes, |q| q.1.as_deref()) else {
        return Ok(());
    };
    let options = QuoteDoc::from_stored(&json_value(quote.3.as_deref()))
        .color_options
        .len();
    let option_index = quote.2.or_else(|| (options == 1).then_some(0));
    mark_quote_sold(pool, quote.0, company_id, option_index, None).await?;
    Ok(())
}

#[cfg(test)]
mod sold_out_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sold_out_colors_are_found_once() {
        let doc = QuoteDoc::parse(&json!({
            "version": 2,
            "areas": [{ "id": "a", "name": "A", "quantity": 1 }, { "id": "b", "name": "B", "quantity": 1 }],
            "colorOptions": [
                { "id": "o1", "byArea": { "a": { "product": "Granite", "color": "Black" }, "b": { "product": "granite", "color": " black " } } },
                { "id": "o2", "byArea": { "a": { "product": "Granite", "color": "Ubatuba" } } },
            ],
        }))
        .unwrap();
        let list = json!({ "arrProducts": [{ "name": "Granite", "arrVariants": [
            { "name": "Black", "soldOut": true }, { "name": "Ubatuba" },
        ] }] });
        assert_eq!(
            option_sold_out_colors(&doc, &list, 0),
            vec!["Black".to_string()]
        );
        assert_eq!(option_sold_out_colors(&doc, &list, 1), Vec::<String>::new());
        assert_eq!(
            sold_out_message(&sold_out_colors(&doc, &list)),
            "Black is sold out and cannot be on a quote. Pick another color."
        );
    }

    #[test]
    fn email_option_colors_and_index() {
        let doc = QuoteDoc::parse(&json!({
            "version": 2,
            "areas": [{ "id": "a", "name": "A", "quantity": 1 }, { "id": "b", "name": "B", "quantity": 1 }, { "id": "c", "name": "C", "quantity": 1 }],
            "colorOptions": [
                { "id": "o1", "byArea": { "a": { "product": "Quartz", "color": "Calacatta Laza" }, "b": { "product": "quartz", "color": " calacatta laza " }, "c": { "product": "Granite", "color": "Black Pearl" } } },
                { "id": "o2", "byArea": {} },
            ],
        }))
        .unwrap();
        assert_eq!(
            option_color_text(&doc, 0).as_deref(),
            Some("Quartz Calacatta Laza, Granite Black Pearl")
        );
        assert_eq!(option_color_text(&doc, 1), None);
        assert_eq!(option_color_text(&doc, 5), None);

        let mut quote = Quote {
            id: 1,
            company_id: 1,
            customer_id: 1,
            deal_id: None,
            quote_name: None,
            salesperson_id: None,
            created_by: None,
            price_list_id: None,
            price_list_revision_id: None,
            tax_pct: 0.0,
            discount_pct: 0.0,
            discount_amount: 0.0,
            draft_drawing_json: None,
            customer_accepted_option_id: None,
        };
        assert_eq!(email_option_index(&quote, &doc), Some(0));
        quote.customer_accepted_option_id = Some(1);
        assert_eq!(email_option_index(&quote, &doc), Some(1));
        quote.customer_accepted_option_id = Some(9);
        assert_eq!(email_option_index(&quote, &doc), Some(0));
    }

    #[test]
    fn sinks_follow_their_model() {
        let doc = json!({ "counters": [{ "runs": [{ "cutouts": [
            { "kind": "sink", "sinkModel": "LSB 32", "sinkType": "farmer", "sinkShape": "rectangle" },
        ] }] }] });
        let list =
            json!({ "arrSinkModels": [{ "name": "lsb 32", "sinkType": 2, "sinkShape": 13 }] });
        let next = with_current_sink_models(&doc, &list).unwrap();
        let cut = &next["counters"][0]["runs"][0]["cutouts"][0];
        assert_eq!(cut["sinkType"], "undermount");
        assert_eq!(cut["sinkShape"], "60/40-rounded");
        assert!(with_current_sink_models(&next, &list).is_none());
    }
}
