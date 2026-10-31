//! What the composer's autocomplete knows about stones and prices.
//!
//! Each displayed stone (colors, price, slab size, stock) and what the
//! default price list charges for edges, cutouts and fabrication.

use crate::error::ApiResult;
use crate::followups::js::trim;
use crate::quote_pricing::js::{arr, get, num, on, string, truthy};
use crate::quote_pricing::pricing::{price_group_of, variant_price};
use crate::quote_pricing::{load_stock, price_list};
use crate::routes::quote_price_lists;
use crate::routes::stones::queries::list_stones;
use crate::routes::stones::schemas::{ListStonesQuery, Stone};
use lambda_http::tracing;
use serde_json::Value;
use sqlx::MySqlPool;
use std::collections::HashMap;

fn plural(count: i64, one: &str) -> String {
    if count == 1 {
        format!("1 {one}")
    } else {
        format!("{count} {one}s")
    }
}

/// `$65`, `$12.50`.
fn money(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("${value:.0}")
    } else {
        format!("${value:.2}")
    }
}

/// The price list's price for a stone, as a quote charges it.
fn quote_price(price_list: &Value, stone: &Stone) -> Option<String> {
    let material = price_list::key(&price_list::material_name(stone.kind.as_deref()?));
    let name = price_list::key(stone.name.as_deref()?);
    let product = arr(Some(price_list), "arrProducts")
        .iter()
        .find(|p| string(Some(p), "name").is_some_and(|n| price_list::key(n) == material))?;
    let variant = arr(Some(product), "arrVariants")
        .iter()
        .find(|v| string(Some(v), "name").is_some_and(|n| price_list::key(n) == name));
    let group = price_group_of(product, variant);
    let price = variant_price(variant, group).filter(|p| *p > 0.0)?;
    if on(get(variant, "isSlabPricing")) || on(get(group, "isSlabPricing")) {
        return Some(format!("{} per slab", money(price)));
    }
    let catalog = stone.retail_price.map_or(0.0, f64::from);
    #[allow(clippy::float_cmp)]
    (price != catalog).then(|| format!("{}/sq ft", money(price)))
}

/// One line per stone, with its colors, price, slab size and stock.
///
/// The price is per sq ft (0: by the slab), plus the quote price when the
/// price list charges another; stock is whole slabs and remnants, else
/// regular stock or sold out.
pub fn format_stones<S: std::hash::BuildHasher>(
    stones: &[Stone],
    colors: &HashMap<i32, Vec<String>, S>,
    price_list: Option<&Value>,
) -> String {
    stones
        .iter()
        .filter_map(|stone| {
            let name = stone.name.as_deref().map(trim).filter(|n| !n.is_empty())?;
            let about = [
                stone
                    .kind
                    .as_deref()
                    .map(trim)
                    .filter(|k| !k.is_empty())
                    .map(str::to_string),
                colors
                    .get(&stone.id)
                    .map(|c| c.join(", "))
                    .filter(|c| !c.is_empty()),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
            let about = if about.is_empty() {
                String::new()
            } else {
                format!(" ({})", about.join("; "))
            };
            let catalog = match stone.retail_price {
                Some(price) if price > 0 => format!("${price}/sq ft"),
                _ => "priced by the slab".to_string(),
            };
            let price = match price_list.and_then(|pl| quote_price(pl, stone)) {
                Some(quoted) => format!("{catalog} ({quoted} on quotes)"),
                None => catalog,
            };
            let size = match (stone.length, stone.width) {
                (Some(length), Some(width)) if length > 0.0 && width > 0.0 => {
                    format!(", slabs {length}x{width} in")
                }
                _ => String::new(),
            };
            let remnants = stone.slabs.available - stone.slabs.whole_available;
            let stock = [
                (stone.slabs.whole_available > 0)
                    .then(|| format!("{} available", plural(stone.slabs.whole_available, "slab"))),
                (remnants > 0).then(|| plural(remnants, "remnant")),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
            let stock = if !stock.is_empty() {
                stock.join(", ")
            } else if stone.regular_stock {
                "regular stock (always orderable)".to_string()
            } else {
                "sold out".to_string()
            };
            let sale = if stone.on_sale { ", on sale" } else { "" };
            Some(format!("- {name}{about}: {price}{size}, {stock}{sale}"))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// "Quartz: " for a rule of one material ("Quartz Viraldi: " of one color);
/// `None` when quotes hide its price.
fn rule_label(rule: &Value) -> Option<String> {
    let filter = get(Some(rule), "materialFilter");
    if on(get(filter, "priceHiddenOnQuote")) {
        return None;
    }
    let parts = [string(filter, "product"), string(filter, "productVariant")]
        .into_iter()
        .flatten()
        .map(trim)
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>();
    Some(if parts.is_empty() {
        String::new()
    } else {
        format!("{}: ", parts.join(" "))
    })
}

fn edge_lines(price_list: &Value) -> Vec<String> {
    arr(Some(price_list), "arrEdgeProfileSets")
        .iter()
        .filter_map(|set| {
            let label = rule_label(set)?;
            let profiles = arr(Some(set), "arrEdgeProfiles")
                .iter()
                .filter_map(|profile| {
                    let name = string(Some(profile), "name")
                        .map(trim)
                        .filter(|n| !n.is_empty())?;
                    let group = truthy(get(Some(profile), "priceGroup"))
                        .then(|| {
                            arr(Some(set), "arrPriceGroups")
                                .iter()
                                .find(|g| g.get("name") == profile.get("priceGroup"))
                        })
                        .flatten();
                    let (price, flat) = group.map_or_else(
                        || {
                            (
                                num(Some(profile), "price"),
                                on(get(Some(profile), "isFlatPrice")),
                            )
                        },
                        |g| (num(Some(g), "price"), false),
                    );
                    Some(match price {
                        Some(p) if p > 0.0 && flat => format!("{name} {} flat", money(p)),
                        Some(p) if p > 0.0 => format!("{name} {}/lin ft", money(p)),
                        Some(_) => format!("{name} included"),
                        None => name.to_string(),
                    })
                })
                .collect::<Vec<_>>();
            (!profiles.is_empty()).then(|| format!("- {label}Edges: {}", profiles.join(", ")))
        })
        .collect()
}

/// Sink and cooktop cutouts, priced by the sink's (cooktop's) width.
fn cutout_lines(price_list: &Value) -> Vec<String> {
    [
        ("arrSinkUndermounts", "Undermount sink cutout", "price"),
        ("arrSinkDropIns", "Drop-in sink cutout", "price"),
        ("arrSinkFarmers", "Farmhouse sink cutout", "price"),
        ("arrCooktops", "Cooktop cutout", "dropInPrice"),
    ]
    .into_iter()
    .flat_map(|(key, what, price_key)| {
        arr(Some(price_list), key).iter().filter_map(move |rule| {
            let label = rule_label(rule)?;
            let ranges = arr(Some(rule), "arrRanges");
            let prices = ranges
                .iter()
                .filter_map(|range| {
                    let price =
                        num(Some(range), price_key).or_else(|| num(Some(range), "price"))?;
                    Some(match num(Some(range), "maxDimension") {
                        Some(max) => format!("up to {max}\" wide {}", money(price)),
                        None if ranges.len() > 1 => format!("wider {}", money(price)),
                        None => money(price),
                    })
                })
                .collect::<Vec<_>>();
            (!prices.is_empty()).then(|| format!("- {label}{what}: {}", prices.join(", ")))
        })
    })
    .collect()
}

/// Fabrication and installation per sq ft. A stone priced by the slab always
/// pays fabrication; one priced by the sq ft only while it is enabled.
fn charge_lines(price_list: &Value) -> Vec<String> {
    let enabled = |rule: &Value| rule.get("enabled").is_none_or(|e| on(Some(e)));
    let mut lines = Vec::new();
    for rule in arr(Some(price_list), "arrFabricationCharges") {
        let (Some(label), Some(price)) = (rule_label(rule), num(Some(rule), "price")) else {
            continue;
        };
        if price > 0.0 {
            let only = if enabled(rule) {
                ""
            } else {
                " (stones priced by the slab only)"
            };
            lines.push(format!(
                "- {label}Fabrication{only}: {}/sq ft",
                money(price)
            ));
        }
    }
    for rule in arr(Some(price_list), "arrInstallationCharges") {
        let (Some(label), Some(price)) = (rule_label(rule), num(Some(rule), "price")) else {
            continue;
        };
        if price > 0.0 && enabled(rule) {
            lines.push(format!("- {label}Installation: {}/sq ft", money(price)));
        }
    }
    lines
}

/// What the list charges on a quote besides the stone (empty: nothing).
pub fn format_price_list(price_list: &Value) -> String {
    [
        edge_lines(price_list),
        cutout_lines(price_list),
        charge_lines(price_list),
    ]
    .concat()
    .join("\n")
}

/// The STONES and PRICE LIST blocks (empty: neither).
pub fn format_catalog(stones: &str, price_list_name: &str, price_list: &str) -> String {
    let mut blocks = Vec::new();
    if !trim(stones).is_empty() {
        blocks.push(format!(
            "STONES (the company's displayed stones: material; colors: price, slab size, what is in stock now):\n{}",
            trim(stones)
        ));
    }
    if !trim(price_list).is_empty() {
        blocks.push(format!(
            "PRICE LIST \"{}\" (what a new quote charges besides the stone):\n{}",
            trim(price_list_name),
            trim(price_list)
        ));
    }
    blocks.join("\n\n")
}

/// The default list for new quotes, priced as quotes price it.
async fn load_price_list(pool: &MySqlPool, company_id: i32) -> ApiResult<Option<(String, Value)>> {
    let lists = quote_price_lists::list_rows(pool, company_id).await?;
    let Some(id) = quote_price_lists::default_id(pool, company_id, &lists).await? else {
        return Ok(None);
    };
    let Some((row, Some(info))) = quote_price_lists::find(pool, id, company_id).await? else {
        return Ok(None);
    };
    let stock = load_stock(pool, company_id).await?;
    Ok(Some((row.name, price_list::sync_with_stock(&info, &stock))))
}

/// Each live stone's color names, by stone id.
async fn load_colors(
    pool: &MySqlPool,
    company_id: i32,
) -> Result<HashMap<i32, Vec<String>>, sqlx::Error> {
    let rows: Vec<(i32, String)> = sqlx::query_as(
        "SELECT sc.stone_id, c.name FROM stone_colors sc
           JOIN colors c ON c.id = sc.color_id
           JOIN stones s ON s.id = sc.stone_id
          WHERE s.company_id = ? AND s.deleted_at IS NULL
          ORDER BY c.name",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    let mut colors: HashMap<i32, Vec<String>> = HashMap::new();
    for (stone_id, name) in rows {
        colors.entry(stone_id).or_default().push(name);
    }
    Ok(colors)
}

/// The company's catalog for the system message. Best effort: without a
/// part the AI only can't answer those price or stock questions.
pub async fn load_catalog(pool: &MySqlPool, company_id: i32) -> String {
    let filter = ListStonesQuery {
        include_sold_out: true,
        ..ListStonesQuery::default()
    };
    let stones = list_stones(pool, company_id, &filter)
        .await
        .inspect_err(|err| tracing::warn!(error = %err, "compose: stones unavailable"))
        .unwrap_or_default();
    let colors = load_colors(pool, company_id)
        .await
        .inspect_err(|err| tracing::warn!(error = %err, "compose: stone colors unavailable"))
        .unwrap_or_default();
    let price_list = load_price_list(pool, company_id)
        .await
        .inspect_err(|err| tracing::warn!(error = %err, "compose: price list unavailable"))
        .ok()
        .flatten();
    let (name, list) = price_list.unzip();
    format_catalog(
        &format_stones(&stones, &colors, list.as_ref()),
        name.as_deref().unwrap_or(""),
        &list.as_ref().map(format_price_list).unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::stones::schemas::SlabCounts;
    use serde_json::json;

    fn stone(id: i32, name: &str, price: i32, whole: i64, remnants: i64) -> Stone {
        Stone {
            id,
            name: Some(name.into()),
            kind: Some("quartz".into()),
            url: None,
            supplier_id: None,
            is_display: true,
            on_sale: false,
            regular_stock: false,
            length: Some(126.5),
            width: Some(63.0),
            retail_price: Some(price),
            cost_per_sqft: Some(30),
            level: Some(3),
            finishing: None,
            samples_amount: None,
            samples_importance: None,
            bundle_number: None,
            bundle_location: None,
            delivery_date: None,
            created_date: None,
            slabs: SlabCounts {
                total: whole + remnants,
                available: whole + remnants,
                whole_total: whole,
                whole_available: whole,
            },
        }
    }

    #[test]
    fn stones_show_colors_price_size_and_stock() {
        let regular = Stone {
            regular_stock: true,
            on_sale: true,
            length: None,
            ..stone(5, "Calacatta Laza", 0, 0, 0)
        };
        let unnamed = Stone {
            name: Some("  ".into()),
            ..stone(6, "x", 50, 1, 0)
        };
        let colors = HashMap::from([(1, vec!["Gray".to_string(), "White".to_string()])]);
        let stones = format_stones(
            &[
                stone(1, "Viraldi", 65, 3, 1),
                stone(2, "Frost White", 45, 1, 0),
                stone(3, "Taj Mahal", 90, 0, 2),
                stone(4, "Black Pearl", 40, 0, 0),
                regular,
                unnamed,
            ],
            &colors,
            None,
        );
        assert_eq!(
            stones,
            "- Viraldi (quartz; Gray, White): $65/sq ft, slabs 126.5x63 in, 3 slabs available, 1 remnant\n\
             - Frost White (quartz): $45/sq ft, slabs 126.5x63 in, 1 slab available\n\
             - Taj Mahal (quartz): $90/sq ft, slabs 126.5x63 in, 2 remnants\n\
             - Black Pearl (quartz): $40/sq ft, slabs 126.5x63 in, sold out\n\
             - Calacatta Laza (quartz): priced by the slab, regular stock (always orderable), on sale"
        );
        assert!(!stones.contains("30"), "cost and level stay out");
    }

    #[test]
    fn a_quote_price_shows_only_when_it_differs() {
        let list = json!({ "arrProducts": [{
            "name": "Quartz",
            "arrPriceGroups": [{ "name": "Lux", "levelFrom": 3, "levelTo": 3, "adjust": 10 }],
            "arrVariants": [
                { "name": "Viraldi", "stockPrice": 65, "level": 3 },
                { "name": "frost white", "stockPrice": 45 },
                { "name": "Taj Mahal", "price": 1200, "isSlabPricing": 1 },
            ],
        }]});
        let stones = format_stones(
            &[
                stone(1, "Viraldi", 65, 1, 0),
                stone(2, "Frost White", 45, 1, 0),
                stone(3, "Taj Mahal", 0, 1, 0),
            ],
            &HashMap::new(),
            Some(&list),
        );
        assert_eq!(
            stones,
            "- Viraldi (quartz): $65/sq ft ($75/sq ft on quotes), slabs 126.5x63 in, 1 slab available\n\
             - Frost White (quartz): $45/sq ft, slabs 126.5x63 in, 1 slab available\n\
             - Taj Mahal (quartz): priced by the slab ($1200 per slab on quotes), slabs 126.5x63 in, 1 slab available"
        );
    }

    #[test]
    fn the_price_list_names_edges_cutouts_and_charges() {
        let any = json!({ "allowDiscount": 1, "priceHiddenOnQuote": 0 });
        let list = json!({
            "arrEdgeProfileSets": [
                { "materialFilter": any, "arrPriceGroups": [{ "name": "Fancy", "price": 30 }], "arrEdgeProfiles": [
                    { "name": "Flat", "price": 0 },
                    { "name": "Eased", "price": 100, "isFlatPrice": 1 },
                    { "name": "Ogee", "price": 25 },
                    { "name": "Dupont", "priceGroup": "Fancy" },
                ]},
                { "materialFilter": { "product": "Quartz" }, "arrEdgeProfiles": [] },
                { "materialFilter": { "product": "Marble", "priceHiddenOnQuote": 1 }, "arrEdgeProfiles": [{ "name": "Ogee", "price": 40 }] },
            ],
            "arrSinkUndermounts": [{ "materialFilter": any, "arrRanges": [
                { "price": 125, "maxDimension": 18 }, { "price": 250, "maxDimension": null },
            ]}],
            "arrSinkFarmers": [{ "materialFilter": { "product": "Quartz" }, "arrRanges": [{ "price": 300, "maxDimension": null }] }],
            "arrCooktops": [{ "arrRanges": [{ "dropInPrice": 200, "maxDimension": null }] }],
            "arrFabricationCharges": [{ "price": 12.5, "enabled": 0, "materialFilter": any }],
            "arrInstallationCharges": [{ "price": 0, "enabled": 1 }],
        });
        assert_eq!(
            format_price_list(&list),
            "- Edges: Flat included, Eased $100 flat, Ogee $25/lin ft, Dupont $30/lin ft\n\
             - Undermount sink cutout: up to 18\" wide $125, wider $250\n\
             - Quartz: Farmhouse sink cutout: $300\n\
             - Cooktop cutout: $200\n\
             - Fabrication (stones priced by the slab only): $12.50/sq ft"
        );
        assert_eq!(format_price_list(&json!({})), "");
    }

    #[test]
    fn the_catalog_has_a_block_for_each_part() {
        assert_eq!(format_catalog(" ", "Retail", ""), "");
        assert_eq!(
            format_catalog(
                "- Viraldi (quartz): $65/sq ft, sold out",
                "Retail 2026",
                "- Cooktop cutout: $200\n"
            ),
            "STONES (the company's displayed stones: material; colors: price, slab size, what is in stock now):\n\
             - Viraldi (quartz): $65/sq ft, sold out\n\n\
             PRICE LIST \"Retail 2026\" (what a new quote charges besides the stone):\n\
             - Cooktop cutout: $200"
        );
    }
}
