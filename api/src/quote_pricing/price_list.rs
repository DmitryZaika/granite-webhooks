//! The price list a quote prices with, built as the CRM builds it.
//!
//! `syncPriceListWithStock`, `applyExtrasCatalog`, `withLiveChoices`: the
//! list follows the company's stones and sinks, takes the extras catalog,
//! and a quote's snapshot gets today's new choices added.

use super::js::{arr, get, num, string, to_number, truthy};
use super::takeoff::js_round;
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};

/// `parsePriceListInfo`: the list, or `None` when it is not one.
pub fn parse(raw: &Value) -> Option<Value> {
    let value = match raw {
        Value::String(text) => serde_json::from_str(text).ok()?,
        other => other.clone(),
    };
    let ok = value.get("units").is_some_and(Value::is_string)
        && value.get("currency").is_some_and(Value::is_string)
        && value.get("expirationDays").is_some_and(Value::is_number)
        && value
            .get("lineItemPriceRounding")
            .is_some_and(Value::is_number)
        && value.get("arrProducts").is_some_and(Value::is_array)
        && value.get("arrOtherItems").is_some_and(Value::is_array);
    ok.then_some(value)
}

/// `emptyPriceListInfo`.
pub fn empty() -> Value {
    let mut info = json!({
        "units": "in",
        "currency": "$",
        "expirationDays": 30,
        "lineItemPriceRounding": 2,
        "otherSinks": { "enabled": 1 },
        "miscellaneousOptions": { "enabled": 1 },
        "otherCutoutOptions": { "customOtherCutoutsIsEnabled": true },
    });
    for key in [
        "arrProducts",
        "arrFabricationCharges",
        "arrInstallationCharges",
        "arrSplashes",
        "arrWaterfallMiteredEdgeMaterialCharges",
        "arrWaterfallMiteredEdgeCutCharges",
        "arrWaterfallInstallationCharges",
        "arrEdgeProfileSets",
        "arrApplianceEdges",
        "arrRadiusEdges",
        "arrOutsideRadiusCorners",
        "arrInsideRadiusCorners",
        "arrClippedCorners",
        "arrBumpedOutCorners",
        "arrNotchedCorners",
        "arrInsideDiagonalCorners",
        "arrRecessedDiagonalCorners",
        "arrBumpOutArcs",
        "arrBumpInArcs",
        "arrBumpOutPolygons",
        "arrBumpInPolygons",
        "arrSinkUndermounts",
        "arrSinkDropIns",
        "arrSinkFarmers",
        "arrFaucetHoles",
        "arrOutletCutouts",
        "arrCooktops",
        "arrOtherCutouts",
        "arrSinkModels",
        "arrSinkCutoutOptions",
        "arrOtherItems",
    ] {
        info[key] = json!([]);
    }
    info
}

/// A company stone (Inventory > Stones), as pricing reads it.
#[derive(Debug, Clone, Default)]
pub struct StockStone {
    pub name: String,
    pub kind: String,
    pub retail_price: Option<f64>,
    pub level: Option<f64>,
    pub length: Option<f64>,
    pub width: Option<f64>,
    pub is_display: Option<bool>,
    pub has_image: bool,
    pub slabs_left: Option<f64>,
    pub regular_stock: bool,
}

/// A company sink type in stock (Inventory > Sinks).
#[derive(Debug, Clone, Default)]
pub struct StockSink {
    pub name: String,
    pub kind: String,
    pub length: Option<f64>,
    pub width: Option<f64>,
    pub retail_price: Option<f64>,
    pub cutout_type: Option<f64>,
    pub cutout_shape: Option<f64>,
    pub faucet_hole_count: Option<f64>,
    pub is_display: Option<bool>,
}

/// What the list is synced with (`loadPricingStock` + the extras catalog).
#[derive(Debug, Clone, Default)]
pub struct Stock {
    /// Every stone, in-stock ones first per name (`inStockFirst`).
    pub stones: Vec<StockStone>,
    /// `(material, color)` keys of stones with slabs left or regular stock.
    pub available: HashSet<String>,
    pub sinks: Vec<StockSink>,
    pub types: Vec<String>,
    /// The extras catalog: `(id, item)`.
    pub extras: Vec<(i64, Value)>,
}

pub fn key(s: &str) -> String {
    s.trim().to_lowercase()
}

/// "quartz" → "Quartz": the material a stone is priced under.
pub fn material_name(kind: &str) -> String {
    let t = kind.trim();
    let mut chars = t.chars();
    chars.next().map_or_else(
        || "Other".to_string(),
        |first| first.to_uppercase().chain(chars).collect(),
    )
}

pub fn available_key(stone: &StockStone) -> String {
    format!("{}|{}", key(&material_name(&stone.kind)), key(&stone.name))
}

type Material = (String, String, Vec<(String, Color)>);

#[derive(Debug, Clone)]
struct Color {
    name: String,
    level: Option<f64>,
    slab_size: Option<(f64, f64)>,
    price: Option<f64>,
    is_display: Option<bool>,
    has_image: bool,
    slabs_left: Option<f64>,
}

fn name_of(v: &Value) -> &str {
    v.get("name").and_then(Value::as_str).unwrap_or_default()
}

fn obj(v: &Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap_or_default()
}

fn number(v: f64) -> Value {
    serde_json::Number::from_f64(v).map_or(Value::Null, |n| {
        // Whole numbers as integers, as JSON.stringify writes them.
        #[allow(clippy::cast_possible_truncation, clippy::float_cmp)]
        if v.fract() == 0.0 && v.abs() < 9e15 {
            Value::from(v as i64)
        } else {
            Value::Number(n)
        }
    })
}

/// `x || null` for a number.
fn truthy_num(v: Option<f64>) -> Option<f64> {
    v.filter(|x| *x != 0.0 && !x.is_nan())
}

fn from_stock(
    material: &str,
    v: Map<String, Value>,
    color: &Color,
    available: Option<&HashSet<String>>,
) -> Value {
    let mut next = v;
    next.insert("name".into(), Value::String(color.name.clone()));
    match color.level {
        None => next.remove("level"),
        Some(level) => next.insert("level".into(), number(level)),
    };
    match color.slab_size {
        None => next.remove("stockSlabSize"),
        Some((w, h)) => next.insert("stockSlabSize".into(), json!([number(w), number(h)])),
    };
    match color.price {
        None => next.remove("stockPrice"),
        Some(price) => next.insert("stockPrice".into(), number(price)),
    };
    let display = color.is_display.map_or_else(
        || {
            next.get("isDisplay")
                .filter(|v| !v.is_null())
                .cloned()
                .unwrap_or_else(|| json!(1))
        },
        |shown| json!(u8::from(shown)),
    );
    next.insert("isDisplay".into(), display);
    if available.is_some_and(|a| !a.contains(&format!("{material}|{}", key(&color.name)))) {
        next.insert("soldOut".into(), Value::Bool(true));
    } else {
        next.remove("soldOut");
    }
    if color.has_image {
        next.remove("noImage");
    } else {
        next.insert("noImage".into(), Value::Bool(true));
    }
    match color.slabs_left {
        None => next.remove("slabsLeft"),
        Some(left) => next.insert("slabsLeft".into(), number(left)),
    };
    Value::Object(next)
}

fn to_product(
    product: &Map<String, Value>,
    colors: &[(String, Color)],
    available: Option<&HashSet<String>>,
) -> Value {
    let product_name = product
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let material = key(product_name);
    let find = |k: &str| colors.iter().find(|(ck, _)| ck == k).map(|(_, c)| c);
    let mut variants: Vec<Value> = product
        .get("arrVariants")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|v| {
            if let Some(color) = find(&key(name_of(v))) {
                let mut stocked = obj(v);
                stocked.remove("custom");
                return Some(from_stock(&material, stocked, color, available));
            }
            truthy(v.get("custom")).then(|| v.clone())
        })
        .collect();
    let listed: HashSet<String> = variants.iter().map(|v| key(name_of(v))).collect();
    for (_, color) in colors {
        if !listed.contains(&key(&color.name)) {
            let mut fresh = Map::new();
            fresh.insert("name".into(), Value::String(color.name.clone()));
            variants.push(from_stock(&material, fresh, color, available));
        }
    }
    let mut next = product.clone();
    next.insert("name".into(), Value::String(material_name(product_name)));
    next.insert("arrVariants".into(), Value::Array(variants));
    Value::Object(next)
}

fn sink_model_from_stock(sink: &StockSink, existing: Option<&Value>) -> Value {
    let width = truthy_num(sink.length)
        .or_else(|| truthy_num(num(existing, "cutoutWidth")))
        .unwrap_or(0.0);
    let depth = truthy_num(sink.width)
        .or_else(|| truthy_num(num(existing, "cutoutDepth")))
        .unwrap_or(0.0);
    let farm = sink.kind.to_lowercase().contains("farm");
    let mut model = Map::new();
    model.insert("allowDiscount".into(), json!(1));
    model.insert("editPriceOnQuote".into(), json!(1));
    if let Some(e) = existing.and_then(Value::as_object) {
        model.extend(e.clone());
    }
    model.remove("custom");
    model.insert("name".into(), Value::String(sink.name.trim().to_string()));
    model.insert("cutoutWidth".into(), number(width));
    model.insert("cutoutDepth".into(), number(depth));
    let pick = |own: Option<f64>, key: &str, fallback: f64| {
        own.map_or_else(
            || {
                get(existing, key)
                    .cloned()
                    .unwrap_or_else(|| number(fallback))
            },
            number,
        )
    };
    model.insert(
        "sinkType".into(),
        pick(sink.cutout_type, "sinkType", if farm { 3.0 } else { 2.0 }),
    );
    model.insert(
        "sinkShape".into(),
        pick(sink.cutout_shape, "sinkShape", 1.0),
    );
    model.insert(
        "faucetHoleCount".into(),
        pick(sink.faucet_hole_count, "faucetHoleCount", 0.0),
    );
    let display = sink.is_display.map_or_else(
        || {
            get(existing, "isDisplay")
                .cloned()
                .unwrap_or_else(|| json!(1))
        },
        |shown| json!(u8::from(shown)),
    );
    model.insert("isDisplay".into(), display);
    let price = get(existing, "flatPrice")
        .cloned()
        .unwrap_or_else(|| number(sink.retail_price.unwrap_or(0.0)));
    model.insert("flatPrice".into(), price);
    Value::Object(model)
}

fn sink_models_from_stock(models: &[Value], sinks: &[StockSink]) -> Vec<Value> {
    let by_name: HashMap<String, &Value> = models.iter().map(|m| (key(name_of(m)), m)).collect();
    let order: HashMap<String, usize> = models
        .iter()
        .enumerate()
        .map(|(i, m)| (key(name_of(m)), i))
        .collect();
    let at = |name: &str| order.get(&key(name)).copied().unwrap_or(usize::MAX);
    let stocked: HashSet<String> = sinks.iter().map(|s| key(&s.name)).collect();
    let mut all: Vec<(usize, usize, Value)> = sinks
        .iter()
        .enumerate()
        .map(|(i, s)| {
            (
                at(&s.name),
                i,
                sink_model_from_stock(s, by_name.get(&key(&s.name)).copied()),
            )
        })
        .collect();
    let custom = models
        .iter()
        .filter(|m| truthy(m.get("custom")) && !stocked.contains(&key(name_of(m))));
    for (i, m) in custom.enumerate() {
        all.push((at(name_of(m)), sinks.len() + i, m.clone()));
    }
    all.sort_by_key(|(at, i, _)| (*at, *i));
    all.into_iter().map(|(_, _, m)| m).collect()
}

fn set_shared_price_groups(products: &mut [Value], groups: &[Value]) {
    for p in products.iter_mut() {
        let name = name_of(p).to_string();
        let mine: Vec<&Value> = groups
            .iter()
            .filter(|g| {
                g.get("products")
                    .and_then(Value::as_array)
                    .is_some_and(|ps| ps.iter().any(|x| x.as_str() == Some(name.as_str())))
            })
            .collect();
        let names: Vec<Option<&Value>> = mine.iter().map(|g| g.get("name")).collect();
        let copies: Vec<Value> = mine
            .iter()
            .map(|g| {
                let mut g = obj(g);
                g.remove("products");
                Value::Object(g)
            })
            .collect();
        p["arrPriceGroups"] = Value::Array(copies);
        if let Some(variants) = p.get_mut("arrVariants").and_then(Value::as_array_mut) {
            for v in variants {
                if truthy(v.get("priceGroup"))
                    && !names.contains(&v.get("priceGroup"))
                    && let Some(v) = v.as_object_mut()
                {
                    v.remove("priceGroup");
                }
            }
        }
    }
}

fn adjusted_price(base: f64, adjust: f64, kind: Option<&str>) -> f64 {
    let price = if kind == Some("percent") {
        base * (1.0 + adjust / 100.0)
    } else {
        base + adjust
    };
    js_round(price * 100.0) / 100.0
}

/// `optionLink`: how the list prices one option or tier.
fn option_link(link: &Value, id: Option<&str>) -> Option<Value> {
    let own = id
        .filter(|id| !id.is_empty())
        .and_then(|id| get(get(Some(link), "prices"), id));
    if truthy(get(own, "adjustType")) {
        return own.cloned();
    }
    if let Some(own) = own
        && (get(Some(own), "flatPrice").is_some() || get(Some(own), "unitPrice").is_some())
    {
        let mut set = obj(own);
        set.insert("adjustType".into(), json!("price"));
        return Some(Value::Object(set));
    }
    let kind = string(Some(link), "adjustType").filter(|k| !k.is_empty());
    if let Some(kind) = kind.filter(|k| *k != "price") {
        return Some(
            json!({ "adjustType": kind, "adjust": link.get("adjust").cloned().unwrap_or(Value::Null) }),
        );
    }
    None
}

/// `linkedPrice`: one catalog price as the list sets it.
fn linked_price(price: Option<&Value>, set: Option<&Value>, key: &str) -> Option<Value> {
    let price = price.filter(|p| !p.is_null()).cloned();
    let Some(kind) = string(set, "adjustType").filter(|k| !k.is_empty()) else {
        return price;
    };
    if kind == "price" {
        return get(set, key).cloned().or(price);
    }
    match price {
        Some(p) if truthy(Some(&p)) => Some(number(adjusted_price(
            to_number(&p),
            num(set, "adjust").unwrap_or(0.0),
            Some(kind),
        ))),
        other => other,
    }
}

fn linked_extra(id: i64, item: &Value, link: Option<&Value>) -> Value {
    let mut item = obj(item);
    item.insert("catalogId".into(), json!(id));
    let Some(link) = link else {
        return Value::Object(item);
    };
    let options = item
        .get("arrOptions")
        .and_then(Value::as_array)
        .map(|options| {
            options
                .iter()
                .map(|o| {
                    let set = option_link(link, o.get("id").and_then(Value::as_str));
                    let mut o = obj(o);
                    let price = linked_price(o.get("flatPrice"), set.as_ref(), "flatPrice")
                        .unwrap_or_else(|| json!(0));
                    o.insert("flatPrice".into(), price);
                    Value::Object(o)
                })
                .collect::<Vec<_>>()
        });
    match options {
        Some(options) => item.insert("arrOptions".into(), Value::Array(options)),
        None => item.remove("arrOptions"),
    };
    let ranges = item
        .get("arrRanges")
        .and_then(Value::as_array)
        .map(|ranges| {
            ranges
                .iter()
                .map(|r| {
                    let set = option_link(link, r.get("id").and_then(Value::as_str));
                    let mut r = obj(r);
                    for key in ["flatPrice", "unitPrice"] {
                        match linked_price(r.get(key), set.as_ref(), key) {
                            Some(price) => r.insert(key.into(), price),
                            None => r.remove(key),
                        };
                    }
                    Value::Object(r)
                })
                .collect::<Vec<_>>()
        });
    match ranges {
        Some(ranges) => item.insert("arrRanges".into(), Value::Array(ranges)),
        None => item.remove("arrRanges"),
    };
    Value::Object(item)
}

fn apply_extras_catalog(info: &mut Value, catalog: &[(i64, Value)]) {
    let links = get(Some(info), "extraLinks");
    let mut items: Vec<Value> = catalog
        .iter()
        .filter_map(|(id, item)| {
            let link = get(links, &id.to_string());
            (!truthy(get(link, "off"))).then(|| linked_extra(*id, item, link))
        })
        .collect();
    items.extend(
        arr(Some(info), "arrOtherItems")
            .iter()
            .filter(|o| get(Some(o), "catalogId").is_none())
            .cloned(),
    );
    info["arrOtherItems"] = Value::Array(items);
}

/// `syncPriceListWithStock(info, { ...stock, extras })`.
pub fn sync_with_stock(info: &Value, stock: &Stock) -> Value {
    // (key, name, colors by key), in the order first seen.
    let mut materials: Vec<Material> = Vec::new();
    for stone in &stock.stones {
        let name = material_name(&stone.kind);
        let mk = key(&name);
        let idx = materials.iter().position(|m| m.0 == mk).unwrap_or_else(|| {
            materials.push((mk.clone(), name.clone(), Vec::new()));
            materials.len() - 1
        });
        let colors = &mut materials[idx].2;
        let ck = key(&stone.name);
        let seen = colors.iter().position(|c| c.0 == ck);
        let prev = seen.map(|i| colors[i].1.clone());
        let slab_size = match (truthy_num(stone.length), truthy_num(stone.width)) {
            (Some(l), Some(w)) => Some((l, w)),
            _ => None,
        };
        let slabs_left = if stone.regular_stock
            || prev.as_ref().is_some_and(|p| p.slabs_left.is_none())
            || stone.slabs_left.is_none()
        {
            None
        } else {
            Some(
                prev.as_ref().and_then(|p| p.slabs_left).unwrap_or(0.0)
                    + stone.slabs_left.unwrap_or(0.0),
            )
        };
        let color = Color {
            name: prev
                .as_ref()
                .map_or_else(|| stone.name.trim().to_string(), |p| p.name.clone()),
            level: prev.as_ref().and_then(|p| p.level).or(stone.level),
            slab_size: prev.as_ref().and_then(|p| p.slab_size).or(slab_size),
            price: prev
                .as_ref()
                .and_then(|p| p.price)
                .or_else(|| truthy_num(stone.retail_price)),
            is_display: prev
                .as_ref()
                .and_then(|p| p.is_display)
                .or(stone.is_display),
            has_image: prev.as_ref().is_some_and(|p| p.has_image) || stone.has_image,
            slabs_left,
        };
        match seen {
            Some(i) => colors[i].1 = color,
            None => colors.push((ck, color)),
        }
    }
    for kind in &stock.types {
        let name = material_name(kind);
        let mk = key(&name);
        if !materials.iter().any(|m| m.0 == mk) {
            materials.push((mk, name, Vec::new()));
        }
    }
    let available = Some(&stock.available);
    let mut products: Vec<Value> = arr(Some(info), "arrProducts")
        .iter()
        .filter_map(|product| {
            let mk = key(name_of(product));
            let material = materials.iter().find(|m| m.0 == mk)?;
            Some(to_product(&obj(product), &material.2, available))
        })
        .collect();
    let listed: HashSet<String> = products.iter().map(|p| key(name_of(p))).collect();
    for (mk, name, colors) in &materials {
        if listed.contains(mk) {
            continue;
        }
        let fresh = json!({
            "name": name, "editPriceOnQuote": 1, "allowDiscount": 1, "applyTaxCode1": 1, "arrVariants": [],
        });
        products.push(to_product(&obj(&fresh), colors, available));
    }
    let shared = info.get("arrSharedPriceGroups").filter(|g| truthy(Some(g)));
    if let Some(groups) = shared.and_then(Value::as_array) {
        set_shared_price_groups(&mut products, groups);
    }
    let mut synced = info.clone();
    synced["arrProducts"] = Value::Array(products);
    synced["arrSinkModels"] = Value::Array(sink_models_from_stock(
        arr(Some(info), "arrSinkModels"),
        &stock.sinks,
    ));
    apply_extras_catalog(&mut synced, &stock.extras);
    synced
}

/// `withLiveChoices`: the snapshot's prices with today's new choices added.
pub fn with_live_choices(snapshot: &Value, latest: &Value) -> Value {
    let latest_products = arr(Some(latest), "arrProducts");
    let products: Vec<Value> = arr(Some(snapshot), "arrProducts")
        .iter()
        .map(|p| {
            let now = latest_products
                .iter()
                .find(|x| key(name_of(x)) == key(name_of(p)));
            let now_variants = arr(now, "arrVariants");
            let mut variants: Vec<Value> = arr(Some(p), "arrVariants")
                .iter()
                .map(|v| {
                    let live = now_variants
                        .iter()
                        .find(|x| key(name_of(x)) == key(name_of(v)));
                    let sold_out =
                        live.map_or(Some(Value::Bool(true)), |l| l.get("soldOut").cloned());
                    let mut rest = obj(v);
                    for k in ["soldOut", "noImage", "slabsLeft"] {
                        rest.remove(k);
                    }
                    if let Some(s) = sold_out.filter(|s| truthy(Some(s))) {
                        rest.insert("soldOut".into(), s);
                    }
                    if let Some(n) = live
                        .and_then(|l| l.get("noImage"))
                        .filter(|n| truthy(Some(n)))
                    {
                        rest.insert("noImage".into(), n.clone());
                    }
                    if let Some(s) = get(live, "slabsLeft") {
                        rest.insert("slabsLeft".into(), s.clone());
                    }
                    Value::Object(rest)
                })
                .collect();
            let known: HashSet<String> = arr(Some(p), "arrVariants")
                .iter()
                .map(|v| key(name_of(v)))
                .collect();
            variants.extend(
                now_variants
                    .iter()
                    .filter(|v| !known.contains(&key(name_of(v))))
                    .cloned(),
            );
            let mut p = obj(p);
            p.insert("arrVariants".into(), Value::Array(variants));
            Value::Object(p)
        })
        .collect();
    let names =
        |list: &[Value]| -> HashSet<String> { list.iter().map(|x| key(name_of(x))).collect() };
    let known_products = names(arr(Some(snapshot), "arrProducts"));
    let known_sinks = names(arr(Some(snapshot), "arrSinkModels"));
    let known_items = names(arr(Some(snapshot), "arrOtherItems"));
    let mut out = snapshot.clone();
    let mut all_products = products;
    all_products.extend(
        latest_products
            .iter()
            .filter(|p| !known_products.contains(&key(name_of(p))))
            .cloned(),
    );
    out["arrProducts"] = Value::Array(all_products);
    let mut sinks = arr(Some(snapshot), "arrSinkModels").to_vec();
    sinks.extend(
        arr(Some(latest), "arrSinkModels")
            .iter()
            .filter(|s| !known_sinks.contains(&key(name_of(s))))
            .cloned(),
    );
    out["arrSinkModels"] = Value::Array(sinks);
    let latest_items = arr(Some(latest), "arrOtherItems");
    let mut items: Vec<Value> = arr(Some(snapshot), "arrOtherItems")
        .iter()
        .map(|o| {
            let now = latest_items
                .iter()
                .find(|x| key(name_of(x)) == key(name_of(o)));
            let mut rest = obj(o);
            rest.remove("fillFromAreaSqFt");
            if let Some(fill) = now
                .and_then(|n| n.get("fillFromAreaSqFt"))
                .filter(|f| truthy(Some(f)))
            {
                rest.insert("fillFromAreaSqFt".into(), fill.clone());
            }
            Value::Object(rest)
        })
        .collect();
    items.extend(
        latest_items
            .iter()
            .filter(|o| !known_items.contains(&key(name_of(o))))
            .cloned(),
    );
    out["arrOtherItems"] = Value::Array(items);
    out
}
