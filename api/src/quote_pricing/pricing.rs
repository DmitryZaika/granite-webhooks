//! A quote's price per color option (`utils/quote/pricing.ts` `priceQuote`):
//! the same lines, keys and totals as the CRM. Line descriptions are left
//! out; nothing here shows them.

use super::doc::{AreaColor, ItemKind, QuoteDoc, QuoteItem, SinkType};
use super::geometry::{counter_outline, polygon_area, strip_pieces};
use super::js::{arr, get, num, num_str, on, str_is, string, to_number, truthy};
use super::takeoff::{AreaTakeoff, area_takeoff, js_round, round_takeoff, round1};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Flags {
    pub hidden: bool,
    pub allow_discount: bool,
    pub editable: bool,
    pub taxable: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PriceLine {
    pub key: String,
    /// What the quote prints, e.g. "42.5 sq ft Granite Black".
    pub description: String,
    /// `sq ft`, `lin ft` or none.
    pub unit: Option<&'static str>,
    pub unit_price: Option<f64>,
    pub total: f64,
    #[serde(flatten)]
    pub flags: Flags,
    pub missing: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AreaPrice {
    pub area_id: Option<String>,
    /// The area's name ("Other Items" for lines outside any area).
    pub name: String,
    pub quantity: f64,
    pub lines: Vec<PriceLine>,
    pub subtotal: f64,
    pub missing: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OptionPrice {
    pub option_id: String,
    pub areas: Vec<AreaPrice>,
    pub other: AreaPrice,
    pub discount: f64,
    pub tax: f64,
    pub total: f64,
    pub missing: bool,
}

/// The quote's discounts and tax.
#[derive(Debug, Clone, Copy, Default)]
pub struct PriceOptions {
    pub discount_pct: f64,
    pub discount_amount: f64,
    pub tax_pct: f64,
}

fn flags_of(f: Option<&Value>) -> Flags {
    let present = truthy(f);
    Flags {
        hidden: on(get(f, "priceHiddenOnQuote")),
        allow_discount: !present || on(get(f, "allowDiscount")),
        editable: !present || on(get(f, "editPriceOnQuote")),
        taxable: on(get(f, "applyTaxCode1")) || on(get(f, "applyTaxCode2")),
    }
}

/// `roundMoney`: whole dollars when the list rounds to dollars (mode 2).
fn round_money(v: f64, dollars: bool) -> f64 {
    if dollars {
        js_round(v + 1e-9)
    } else {
        js_round(v * 100.0 + 1e-9) / 100.0
    }
}

const fn js_min(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else {
        a.min(b)
    }
}

const fn js_max(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else {
        a.max(b)
    }
}

fn filter_of(rule: Option<&Value>) -> Option<&Value> {
    get(rule, "materialFilter")
}

/// Most specific rule for a product/color: color match, then product, then default.
fn pick_rule<'a>(rules: &'a [Value], product: &str, color: &str) -> Option<&'a Value> {
    let f = |r: &'a Value| filter_of(Some(r));
    rules
        .iter()
        .find(|r| str_is(f(r), "product", product) && str_is(f(r), "productVariant", color))
        .or_else(|| {
            rules
                .iter()
                .find(|r| str_is(f(r), "product", product) && !truthy(get(f(r), "productVariant")))
        })
        .or_else(|| rules.iter().find(|r| !truthy(get(f(r), "product"))))
}

fn pick_edge_set<'a>(pl: &'a Value, product: &str, color: &str) -> Option<&'a Value> {
    let sets: Vec<Value> = arr(Some(pl), "arrEdgeProfileSets")
        .iter()
        .filter(|s| !arr(Some(s), "arrEdgeProfiles").is_empty())
        .cloned()
        .collect();
    let picked = pick_rule(&sets, product, color)?;
    // The same set in the list (pick_rule ran over copies).
    arr(Some(pl), "arrEdgeProfileSets")
        .iter()
        .find(|s| *s == picked)
}

fn range_for<'a>(ranges: &'a [Value], key: &str, value: f64) -> Option<&'a Value> {
    ranges
        .iter()
        .find(|r| num(Some(r), key).is_none_or(|max| value <= max))
}

fn adjusted_price(base: f64, adjust: f64, kind: Option<&str>) -> f64 {
    let price = if kind == Some("percent") {
        base * (1.0 + adjust / 100.0)
    } else {
        base + adjust
    };
    js_round(price * 100.0) / 100.0
}

fn price_group_of<'a>(product: &'a Value, variant: Option<&'a Value>) -> Option<&'a Value> {
    let groups = arr(Some(product), "arrPriceGroups");
    if truthy(get(variant, "priceGroup")) {
        let name = get(variant, "priceGroup");
        return groups.iter().find(|g| g.get("name") == name);
    }
    let level = num(variant, "level")?;
    groups.iter().find(|g| {
        let (from, to) = (get(Some(g), "levelFrom"), get(Some(g), "levelTo"));
        from.is_some()
            && to.is_some()
            && level >= from.map_or(0.0, to_number)
            && level <= to.map_or(0.0, to_number)
    })
}

fn variant_price(variant: Option<&Value>, group: Option<&Value>) -> Option<f64> {
    if let Some(price) = num(variant, "price") {
        return Some(price);
    }
    if let Some(stock) = num(variant, "stockPrice") {
        if let Some(adjust) = num(variant, "adjust") {
            return Some(adjusted_price(stock, adjust, string(variant, "adjustType")));
        }
        if str_is(group, "adjustType", "price")
            && let Some(price) = num(group, "price")
        {
            return Some(price);
        }
        return Some(adjusted_price(
            stock,
            num(group, "adjust").unwrap_or(0.0),
            string(group, "adjustType"),
        ));
    }
    num(group, "price")
}

fn size_of(v: Option<&Value>) -> Option<(f64, f64)> {
    let a = v?.as_array()?;
    Some((
        a.first().map_or(f64::NAN, to_number),
        a.get(1).map_or(f64::NAN, to_number),
    ))
}

struct Material<'a> {
    product: Option<&'a Value>,
    price: Option<f64>,
    by_slab: bool,
    slab: Option<(f64, f64)>,
    flags: Flags,
}

fn resolve_material<'a>(pl: &'a Value, color: Option<&AreaColor>) -> Material<'a> {
    let none = Material {
        product: None,
        price: None,
        by_slab: false,
        slab: None,
        flags: flags_of(None),
    };
    let Some(color) = color.filter(|c| !c.product.is_empty()) else {
        return none;
    };
    let Some(product) = arr(Some(pl), "arrProducts")
        .iter()
        .find(|p| str_is(Some(p), "name", &color.product))
    else {
        return none;
    };
    let flags = Flags {
        hidden: false,
        allow_discount: on(get(Some(product), "allowDiscount")),
        editable: on(get(Some(product), "editPriceOnQuote")),
        taxable: on(get(Some(product), "applyTaxCode1")) || on(get(Some(product), "applyTaxCode2")),
    };
    let variant = arr(Some(product), "arrVariants")
        .iter()
        .find(|v| str_is(Some(v), "name", &color.color));
    let group = price_group_of(product, variant);
    let other_color = !color.color.is_empty() && variant.is_none();
    let price = if other_color {
        color.other_price
    } else {
        variant_price(variant, group)
    };
    let by_slab = if other_color {
        color.other_by_slab == Some(true)
    } else {
        on(get(variant, "isSlabPricing")) || on(get(group, "isSlabPricing"))
    };
    let typed = if other_color && color.other_by_slab == Some(true) {
        color.other_slab_size
    } else {
        None
    };
    let slab = typed
        .or_else(|| size_of(get(variant, "defaultSlabSize")))
        .or_else(|| size_of(get(variant, "stockSlabSize")))
        .or_else(|| size_of(get(Some(product), "defaultSlabSize")));
    Material {
        product: Some(product),
        price,
        by_slab,
        slab,
        flags,
    }
}

struct Builder<'a> {
    lines: Vec<PriceLine>,
    overrides: &'a HashMap<String, HashMap<String, f64>>,
    option_id: &'a str,
    dollars: bool,
}

struct Line {
    key: String,
    description: String,
    unit: Option<&'static str>,
    quantity: f64,
    unit_price: Option<f64>,
    flat: f64,
    flags: Flags,
}

impl Builder<'_> {
    fn add(&mut self, l: Line) {
        let over = self
            .overrides
            .get(&l.key)
            .and_then(|o| o.get(self.option_id))
            .copied();
        let unit_price = over.or(l.unit_price);
        // An override is the whole price: a flat per-item charge no longer adds on.
        let flat = if over.is_some() { 0.0 } else { l.flat };
        let total = unit_price.map_or(0.0, |p| round_money(p * l.quantity + flat, self.dollars));
        self.lines.push(PriceLine {
            key: l.key,
            description: l
                .description
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" "),
            unit: l.unit,
            unit_price,
            total,
            flags: l.flags,
            missing: unit_price.is_none(),
        });
    }
}

const fn line(
    key: String,
    description: String,
    unit: Option<&'static str>,
    quantity: f64,
    unit_price: Option<f64>,
    flags: Flags,
) -> Line {
    Line {
        key,
        description,
        unit,
        quantity,
        unit_price,
        flat: 0.0,
        flags,
    }
}

/// `fmtNum`: up to 4 decimals, as JavaScript prints numbers.
fn fmt_num(v: f64) -> String {
    num_str(js_round(v * 10000.0) / 10000.0)
}

const SQ_FT: Option<&str> = Some("sq ft");
const LIN_FT: Option<&str> = Some("lin ft");

/// Slab size of a slab-priced color that has none (the slab layout uses it too).
const DEFAULT_SLAB: (f64, f64) = (126.0, 63.0);
/// Share of a slab that ends up in pieces: the rest is offcuts between them.
const SLAB_YIELD: f64 = 0.85;

struct Laid {
    count: usize,
    sq_in: f64,
}

/// Square inches of every piece of an area (counters, splashes, aprons).
fn pieces_sq_in(doc: &QuoteDoc, area_id: &str) -> f64 {
    let mut sum = 0.0;
    for c in doc.counters.iter().filter(|c| c.area_id == area_id) {
        let outline = counter_outline(c);
        sum += polygon_area(&outline.polygon);
        for polygon in strip_pieces(c, &outline) {
            if polygon.len() >= 3 {
                sum += polygon_area(&polygon);
            }
        }
    }
    sum
}

fn estimated_slab_count(sq_in: f64, size: (f64, f64)) -> f64 {
    let slab = size.0 * size.1 * SLAB_YIELD;
    if sq_in > 0.0 && slab > 0.0 {
        (sq_in / slab).ceil()
    } else {
        0.0
    }
}

/// `edgeProfileName`: the profile of finished edge `idx` for a color.
fn edge_profile_name(pl: &Value, color: Option<&AreaColor>, idx: usize) -> String {
    let edges = color.map(|c| c.edges.as_slice()).unwrap_or_default();
    if let Some(picked) = edges.get(idx).map(|e| e.trim()).filter(|e| !e.is_empty()) {
        return picked.into();
    }
    if let Some(first) = edges.first().map(|e| e.trim()).filter(|e| !e.is_empty()) {
        return first.into();
    }
    let set = pick_edge_set(
        pl,
        color.map_or("", |c| c.product.as_str()),
        color.map_or("", |c| c.color.as_str()),
    );
    arr(set, "arrEdgeProfiles")
        .first()
        .and_then(|p| string(Some(p), "name"))
        .map(|n| n.trim().to_string())
        .unwrap_or_default()
}

#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
fn drawing_lines(
    b: &mut Builder,
    pl: &Value,
    area_id: &str,
    color: Option<&AreaColor>,
    t: &AreaTakeoff,
    laid: &Laid,
    material_override: Option<f64>,
    estimate_slabs: &dyn Fn((f64, f64)) -> f64,
) {
    let k = |s: &str| format!("{area_id}|{s}");
    let mat = resolve_material(pl, color);
    let product = color.map_or("", |c| c.product.as_str());
    let variant = color.map_or("", |c| c.color.as_str());
    let r = round_takeoff(t);
    let no_product = mat.product.is_none();
    let mat_price = if mat.by_slab {
        None
    } else {
        material_override.or(mat.price)
    };
    let rule = |key: &str| pick_rule(arr(Some(pl), key), product, variant);
    let unless_no_product = |price: Option<f64>| if no_product { None } else { price };

    if r.counter_sq_ft > 0.0 || t.counter_sq_ft > 0.0 {
        if mat.by_slab && mat.product.is_some() {
            let (w, h) = mat.slab.unwrap_or(DEFAULT_SLAB);
            let estimated = laid.count == 0;
            #[allow(clippy::cast_precision_loss)]
            let slab_count = if estimated {
                estimate_slabs((w, h))
            } else {
                laid.count as f64
            };
            let sq_in = if !estimated && laid.sq_in > 0.0 {
                laid.sq_in
            } else {
                slab_count * w * h
            };
            let slab_sq_ft = round1(sq_in / 144.0);
            #[allow(clippy::float_cmp)]
            let slabs = if slab_count == 1.0 { "Slab" } else { "Slabs" };
            b.add(line(
                k("material"),
                format!(
                    "{} sq ft - {} {slabs}{} {product} {variant}",
                    fmt_num(slab_sq_ft),
                    num_str(slab_count),
                    if estimated { " (estimated)" } else { "" }
                ),
                SQ_FT,
                slab_sq_ft,
                mat.price,
                mat.flags,
            ));
            b.add(line(
                k("material-sqft"),
                format!(
                    "{} sq ft Countertop - Priced by the slab",
                    fmt_num(r.counter_sq_ft)
                ),
                None,
                0.0,
                Some(0.0),
                mat.flags,
            ));
        } else {
            let description = if mat.product.is_some() {
                format!("{} sq ft {product} {variant}", fmt_num(r.counter_sq_ft))
            } else {
                format!("{} sq ft Countertop", fmt_num(r.counter_sq_ft))
            };
            b.add(line(
                k("material"),
                description,
                SQ_FT,
                r.counter_sq_ft,
                mat.price,
                mat.flags,
            ));
        }
    }

    for s in &r.splash {
        let sr = rule("arrSplashes");
        b.add(line(
            k(&format!("splash:{}", num_str(s.height))),
            format!(
                "{} sq ft - {}\" Splash{}",
                fmt_num(s.sq_ft),
                fmt_num(s.height),
                if mat.by_slab {
                    " Priced by the slab"
                } else {
                    ""
                }
            ),
            if mat.by_slab { None } else { SQ_FT },
            if mat.by_slab { 0.0 } else { s.sq_ft },
            if mat.by_slab { Some(0.0) } else { mat_price },
            flags_of(filter_of(sr)),
        ));
    }
    for (list, label) in [
        (&r.waterfall, "Waterfall"),
        (&r.mitered, "Mitered Edge Material"),
    ] {
        for s in list {
            let wr = rule("arrWaterfallMiteredEdgeMaterialCharges");
            b.add(line(
                k(&format!("{label}:{}", num_str(s.height))),
                format!(
                    "{} sq ft - {}\" {label}{}",
                    fmt_num(s.sq_ft),
                    fmt_num(s.height),
                    if mat.by_slab {
                        " Priced by the slab"
                    } else {
                        ""
                    }
                ),
                if mat.by_slab { None } else { SQ_FT },
                if mat.by_slab { 0.0 } else { s.sq_ft },
                if mat.by_slab { Some(0.0) } else { mat_price },
                flags_of(filter_of(wr)),
            ));
        }
    }

    let fab = rule("arrFabricationCharges");
    let fab_by_slab = mat.by_slab && mat.product.is_some();
    let fab_unset = fab_by_slab && !(num(fab, "price").unwrap_or(0.0) > 0.0);
    let enabled = |rule: Option<&Value>| {
        rule.is_none() || on(get(rule, "enabled")) || rule.and_then(|r| r.get("enabled")).is_none()
    };
    if r.fabrication_sq_ft > 0.0 && (fab_by_slab || enabled(fab)) {
        let price = if no_product || fab_unset {
            None
        } else {
            Some(num(fab, "price").unwrap_or(0.0))
        };
        b.add(line(
            k("fabrication"),
            format!("{} sq ft Fabrication", fmt_num(r.fabrication_sq_ft)),
            SQ_FT,
            r.fabrication_sq_ft,
            price,
            flags_of(filter_of(fab)),
        ));
    }
    let inst = rule("arrInstallationCharges");
    if r.fabrication_sq_ft > 0.0 && enabled(inst) {
        b.add(line(
            k("installation"),
            format!("{} sq ft Installation", fmt_num(r.fabrication_sq_ft)),
            SQ_FT,
            r.fabrication_sq_ft,
            unless_no_product(Some(num(inst, "price").unwrap_or(0.0))),
            flags_of(filter_of(inst)),
        ));
    }
    if t.waterfall_count > 0.0 {
        let wi = rule("arrWaterfallInstallationCharges");
        b.add(line(
            k("waterfallInstall"),
            format!("{} Waterfall Installation", num_str(t.waterfall_count)),
            None,
            t.waterfall_count,
            unless_no_product(Some(num(wi, "price").unwrap_or(0.0))),
            flags_of(filter_of(wi)),
        ));
    }
    if t.miter_count > 0.0 {
        let mc = rule("arrWaterfallMiteredEdgeCutCharges");
        let by_joint = str_is(mc, "measure", "joint");
        let lin_ft = if by_joint {
            r.miter_joint_lin_ft
        } else {
            r.mitered_cuts_lin_ft
        };
        let each = num(mc, "eachPrice");
        let linear = num(mc, "linearPrice");
        if !(mc.is_some() && linear.is_none() && each.is_some()) {
            let price = if no_product || (mc.is_some() && linear.is_none()) {
                None
            } else {
                Some(linear.unwrap_or(0.0))
            };
            b.add(line(
                k("miterCuts"),
                format!("{} lin ft - Mitered Cuts", fmt_num(lin_ft)),
                LIN_FT,
                lin_ft,
                price,
                flags_of(filter_of(mc)),
            ));
        }
        if let Some(each) = each {
            #[allow(clippy::float_cmp)]
            let edges = if t.miter_count == 1.0 {
                "Edge"
            } else {
                "Edges"
            };
            b.add(line(
                k("miterEach"),
                format!("{} Mitered {edges}", num_str(t.miter_count)),
                None,
                t.miter_count,
                unless_no_product(Some(each)),
                flags_of(filter_of(mc)),
            ));
        }
    }

    if let Some(delivery) = color.and_then(|c| c.delivery).filter(|d| *d > 0.0) {
        b.add(line(
            k("delivery"),
            format!("Delivery - {product} {variant}"),
            None,
            1.0,
            Some(delivery),
            mat.flags,
        ));
    }

    let edge_set = pick_edge_set(pl, product, variant);
    for (idx, lin) in r.finished_lin_ft.iter().copied().enumerate() {
        if lin <= 0.0 {
            continue;
        }
        let name = edge_profile_name(pl, color, idx);
        let profile = if name.is_empty() {
            None
        } else {
            arr(edge_set, "arrEdgeProfiles")
                .iter()
                .find(|p| str_is(Some(p), "name", &name))
        };
        let cost = profile.map(|p| {
            let group = if truthy(get(Some(p), "priceGroup")) {
                arr(edge_set, "arrPriceGroups")
                    .iter()
                    .find(|g| g.get("name") == p.get("priceGroup"))
            } else {
                None
            };
            group.map_or_else(
                || (num(Some(p), "price"), on(get(Some(p), "isFlatPrice"))),
                |g| (num(Some(g), "price"), false),
            )
        });
        let flat = cost.is_some_and(|c| c.1);
        let suffix = if !name.is_empty() {
            format!(" - {name}")
        } else if r.finished_lin_ft.len() > 1 {
            format!(" - Edge {}", idx + 1)
        } else {
            String::new()
        };
        b.add(line(
            k(&format!("edge:{idx}")),
            if flat {
                format!("Finished Edge{suffix}")
            } else {
                format!("{} lin ft Finished Edge{suffix}", fmt_num(lin))
            },
            if flat { None } else { LIN_FT },
            if flat { 1.0 } else { lin },
            cost.and_then(|c| c.0),
            flags_of(filter_of(edge_set)),
        ));
    }
    if r.appliance_lin_ft > 0.0 {
        let ae = rule("arrApplianceEdges");
        b.add(line(
            k("appliance"),
            format!("{} lin ft Appliance Edge", fmt_num(r.appliance_lin_ft)),
            LIN_FT,
            r.appliance_lin_ft,
            unless_no_product(Some(num(ae, "linearPrice").unwrap_or(0.0))),
            flags_of(filter_of(ae)),
        ));
    }
    if t.full_radius_count > 0.0 {
        let fr = rule("arrRadiusEdges");
        let lin = round1(t.full_radius_lin_ft);
        let flat_each = num(fr, "flatPrice").unwrap_or(0.0);
        b.add(Line {
            key: k("fullRadius"),
            description: format!(
                "{} - {} lin ft - Full Radius Edge",
                num_str(t.full_radius_count),
                fmt_num(lin)
            ),
            unit: LIN_FT,
            quantity: lin,
            unit_price: unless_no_product(Some(num(fr, "linearPrice").unwrap_or(0.0))),
            flat: flat_each * t.full_radius_count,
            flags: flags_of(filter_of(fr)),
        });
    }

    for s in &t.sink_cutouts {
        let rules = match s.kind {
            SinkType::Farmer => "arrSinkFarmers",
            SinkType::DropIn => "arrSinkDropIns",
            SinkType::Undermount => "arrSinkUndermounts",
        };
        let sr = rule(rules);
        let range = range_for(arr(sr, "arrRanges"), "maxDimension", s.width);
        let label = match s.kind {
            SinkType::Farmer => "Farmer",
            SinkType::DropIn => "Drop-In",
            SinkType::Undermount => "Undermount",
        };
        b.add(line(
            k(&format!(
                "sinkCutout:{}:{}",
                s.kind.as_str(),
                num_str(s.width)
            )),
            format!(
                "{} - {}\" {label} Sink Cutout",
                num_str(s.count),
                fmt_num(s.width)
            ),
            None,
            s.count,
            unless_no_product(num(range, "price")),
            flags_of(filter_of(sr)),
        ));
    }
    for s in &t.sink_models {
        let model = arr(Some(pl), "arrSinkModels")
            .iter()
            .find(|m| str_is(Some(m), "name", &s.name));
        b.add(line(
            format!("sinkModel:{}", s.name),
            format!("{} Sink - {}", num_str(s.count), s.name),
            None,
            s.count,
            num(model, "flatPrice"),
            Flags {
                hidden: false,
                allow_discount: model.is_none() || on(get(model, "allowDiscount")),
                editable: model.is_none() || on(get(model, "editPriceOnQuote")),
                taxable: true,
            },
        ));
    }
    if t.faucet_holes > 0.0 {
        let fh = rule("arrFaucetHoles");
        b.add(line(
            k("faucetHole"),
            format!("{} Faucet Holes", num_str(t.faucet_holes)),
            None,
            t.faucet_holes,
            unless_no_product(Some(num(fh, "faucetHolePrice").unwrap_or(0.0))),
            flags_of(filter_of(fh)),
        ));
    }
    if t.outlets > 0.0 {
        let oc = rule("arrOutletCutouts");
        b.add(line(
            k("outlet"),
            format!("{} Outlet Cutouts", num_str(t.outlets)),
            None,
            t.outlets,
            unless_no_product(Some(num(oc, "price").unwrap_or(0.0))),
            flags_of(filter_of(oc)),
        ));
    }
    for c in &t.cooktops {
        let cr = rule("arrCooktops");
        let range = range_for(arr(cr, "arrRanges"), "maxDimension", c.width);
        b.add(line(
            k(&format!("cooktop:{}", num_str(c.width))),
            format!("{} Cooktop Cutout", num_str(c.count)),
            None,
            c.count,
            unless_no_product(num(range, "dropInPrice").or_else(|| num(range, "price"))),
            flags_of(filter_of(cr)),
        ));
    }
    for o in &t.other_cutouts {
        b.add(line(
            k(&format!("otherCutout:{}", o.name)),
            format!(
                "{} {}",
                num_str(o.count),
                if o.name.is_empty() {
                    "Other Cutout"
                } else {
                    o.name.as_str()
                }
            ),
            None,
            o.count,
            o.price,
            Flags {
                hidden: false,
                allow_discount: o.allow_discount,
                editable: true,
                taxable: true,
            },
        ));
    }
    for c in &t.corners {
        let (price, f) = match c.kind.as_str() {
            kind @ ("radius" | "insideRadius" | "roundNotch") => {
                let rr = rule(if kind == "radius" {
                    "arrOutsideRadiusCorners"
                } else {
                    "arrInsideRadiusCorners"
                });
                let range = range_for(arr(rr, "arrRanges"), "maxRadius", c.size);
                (num(range, "price").unwrap_or(0.0), filter_of(rr))
            }
            kind => {
                let rules = match kind {
                    "clip" => Some("arrClippedCorners"),
                    "insideDiagonal" => Some("arrInsideDiagonalCorners"),
                    "recessedDiagonal" => Some("arrRecessedDiagonalCorners"),
                    "notch" => Some("arrNotchedCorners"),
                    "bumpOut" => Some("arrBumpedOutCorners"),
                    _ => None,
                };
                let cr = rules.and_then(rule);
                (num(cr, "price").unwrap_or(0.0), filter_of(cr))
            }
        };
        let size = match c.kind.as_str() {
            "notch" => format!("{}\" x {}\"", fmt_num(c.length), fmt_num(c.depth)),
            "bumpOut" => format!("{}\"", fmt_num(c.length)),
            _ => format!("{}\"", fmt_num(c.size)),
        };
        let label = match c.kind.as_str() {
            "radius" => "Outside Radius Corner",
            "insideRadius" => "Inside Radius Corner",
            "clip" => "Clipped Corner",
            "insideDiagonal" => "Inside Diagonal Corner",
            "recessedDiagonal" => "Recessed Diagonal Corner",
            "notch" => "Notched Corner",
            "bumpOut" => "Bumped Out Corner",
            "roundNotch" => "Round Notch Corner",
            _ => "undefined",
        };
        b.add(line(
            k(&format!(
                "corner:{}:{}:{}:{}",
                c.kind,
                num_str(c.size),
                num_str(c.length),
                num_str(c.depth)
            )),
            format!("{} - {size} {label}", num_str(c.count)),
            None,
            c.count,
            unless_no_product(Some(price)),
            flags_of(f),
        ));
    }
    for e in &t.edge_mods {
        let rules = match e.kind.as_str() {
            "bumpOut" => Some("arrBumpOutPolygons"),
            "bumpIn" => Some("arrBumpInPolygons"),
            "bumpOutArc" => Some("arrBumpOutArcs"),
            "bumpInArc" => Some("arrBumpInArcs"),
            _ => None,
        };
        let er = rules.and_then(rule);
        let label = match e.kind.as_str() {
            "bumpOut" => "Bump-Out",
            "bumpIn" => "Bump-In",
            "bumpOutArc" => "Bump-Out Arc",
            "bumpInArc" => "Bump-In Arc",
            _ => "undefined",
        };
        b.add(line(
            k(&format!("edgeMod:{}:{}", e.kind, num_str(e.width))),
            format!("{} - {}\" {label}", num_str(e.count), fmt_num(e.width)),
            None,
            e.count,
            unless_no_product(Some(num(er, "flatPrice").unwrap_or(0.0))),
            flags_of(filter_of(er)),
        ));
    }
}

/// The extra a quote line uses: by its catalog id (follows a rename), else by name.
fn find_other_item<'a>(pl: &'a Value, name: &str, catalog_id: Option<f64>) -> Option<&'a Value> {
    let items = arr(Some(pl), "arrOtherItems");
    #[allow(clippy::float_cmp)]
    let by_id = catalog_id.and_then(|id| {
        items
            .iter()
            .find(|o| o.get("catalogId").and_then(Value::as_f64) == Some(id))
    });
    by_id.or_else(|| items.iter().find(|o| str_is(Some(o), "name", name)))
}

#[allow(clippy::float_cmp)]
fn follows_area(item: &QuoteItem, def: Option<&Value>, area_sq_ft: Option<f64>) -> bool {
    let Some(area) = area_sq_ft.filter(|_| on(get(def, "fillFromAreaSqFt"))) else {
        return false;
    };
    item.amount.is_none() || item.fill_base.is_none_or(|base| base != area)
}

#[allow(clippy::too_many_lines)]
fn item_lines(b: &mut Builder, pl: &Value, items: &[&QuoteItem], area_sq_ft: Option<f64>) {
    for item in items {
        let key = format!("item:{}", item.id);
        #[allow(clippy::float_cmp)]
        let qty_prefix = if item.quantity == 1.0 {
            String::new()
        } else {
            format!("{} - ", fmt_num(item.quantity))
        };
        match item.kind {
            ItemKind::Text => b.add(line(
                key,
                item.name.clone(),
                None,
                0.0,
                Some(0.0),
                Flags {
                    hidden: true,
                    allow_discount: false,
                    editable: false,
                    taxable: false,
                },
            )),
            ItemKind::Misc => b.add(line(
                key,
                format!("{qty_prefix}{}", item.name),
                None,
                item.quantity,
                item.price,
                Flags {
                    hidden: item.hide_on_quote,
                    allow_discount: item.allow_discount,
                    editable: true,
                    taxable: item.taxable,
                },
            )),
            ItemKind::PriceList => {
                let def = find_other_item(pl, &item.name, item.catalog_id);
                let name = string(def, "name")
                    .unwrap_or(item.name.as_str())
                    .to_string();
                let flags = Flags {
                    hidden: on(get(def, "priceHiddenOnQuote")),
                    allow_discount: def.is_none() || on(get(def, "allowDiscount")),
                    editable: def.is_none() || on(get(def, "editPriceOnQuote")),
                    taxable: on(get(def, "applyTaxCode1")) || on(get(def, "applyTaxCode2")),
                };
                let ranges = arr(def, "arrRanges");
                if !ranges.is_empty() {
                    let amount = if follows_area(item, def, area_sq_ft) {
                        area_sq_ft.unwrap_or(0.0)
                    } else {
                        item.amount.unwrap_or(0.0)
                    };
                    let range = range_for(ranges, "maxDimension", amount);
                    let unit_name = string(def, "unitName").unwrap_or_default();
                    b.add(Line {
                        key,
                        // `.replace('  ', ' ')`: the first double space only.
                        description: format!("{} {unit_name} - {name}", fmt_num(amount))
                            .replacen("  ", " ", 1),
                        unit: if unit_name == "sq ft" { SQ_FT } else { None },
                        quantity: amount,
                        unit_price: Some(num(range, "unitPrice").unwrap_or(0.0)),
                        flat: num(range, "flatPrice").unwrap_or(0.0),
                        flags,
                    });
                    continue;
                }
                let options = arr(def, "arrOptions");
                let by_id = item
                    .option_id
                    .as_deref()
                    .filter(|id| !id.is_empty())
                    .and_then(|id| options.iter().find(|o| str_is(Some(o), "id", id)));
                let non_empty = |s: Option<&str>| s.filter(|s| !s.is_empty()).map(str::to_string);
                let option_name = non_empty(string(by_id, "name"))
                    .or_else(|| non_empty(item.option.as_deref()))
                    .or_else(|| {
                        non_empty(
                            options
                                .iter()
                                .find(|o| on(get(Some(o), "defaultOption")))
                                .and_then(|o| string(Some(o), "name")),
                        )
                    })
                    .or_else(|| non_empty(options.first().and_then(|o| string(Some(o), "name"))));
                let opt = by_id.or_else(|| {
                    option_name
                        .as_deref()
                        .and_then(|n| options.iter().find(|o| str_is(Some(o), "name", n)))
                });
                let price = item
                    .price
                    .or_else(|| num(opt, "flatPrice"))
                    .or_else(|| def.map(|_| 0.0));
                let single_default = options.len() == 1
                    && options[0].get("name").and_then(Value::as_str) == Some("Option 1");
                let option_label = match option_name.as_deref() {
                    Some(option) if !single_default => format!(" - {option}"),
                    _ => String::new(),
                };
                b.add(line(
                    key,
                    format!("{qty_prefix}{name}{option_label}"),
                    None,
                    item.quantity,
                    price,
                    flags,
                ));
            }
        }
    }
}

fn slab_count_for(doc: &QuoteDoc, area_id: &str, option_id: &str) -> Laid {
    let slabs = doc
        .layouts
        .get(area_id)
        .and_then(|l| l.get(option_id))
        .map(|l| l.slabs.as_slice())
        .unwrap_or_default();
    let sized = slabs.iter().all(|s| s.width > 0.0 && s.height > 0.0);
    Laid {
        count: slabs.len(),
        sq_in: if sized {
            slabs.iter().fold(0.0, |sum, s| sum + s.width * s.height)
        } else {
            0.0
        },
    }
}

fn sum_totals(lines: &[PriceLine]) -> f64 {
    lines.iter().fold(0.0, |s, l| s + l.total)
}

/// `priceQuote`: every color option of the quote priced with `pl`.
#[allow(clippy::too_many_lines)]
pub fn price_quote(doc: &QuoteDoc, pl: &Value, opts: PriceOptions) -> Vec<OptionPrice> {
    // `lineItemPriceRounding ?? 2`: 2 rounds lines to whole dollars.
    let dollars = get(Some(pl), "lineItemPriceRounding").is_none_or(|v| v.as_f64() == Some(2.0));
    let takeoffs: Vec<AreaTakeoff> = doc.areas.iter().map(|a| area_takeoff(doc, &a.id)).collect();
    let mut estimates: HashMap<String, f64> = HashMap::new();
    let mut pieces: HashMap<String, f64> = HashMap::new();
    doc.color_options
        .iter()
        .map(|option| {
            let areas: Vec<AreaPrice> = doc
                .areas
                .iter()
                .zip(&takeoffs)
                .map(|(area, t)| {
                    let mut b = Builder {
                        lines: Vec::new(),
                        overrides: &doc.price_overrides,
                        option_id: &option.id,
                        dollars,
                    };
                    let estimate = |size: (f64, f64)| {
                        let key = format!("{}|{}x{}", area.id, num_str(size.0), num_str(size.1));
                        if let Some(hit) = estimates.get(&key) {
                            return *hit;
                        }
                        let sq_in = *pieces
                            .entry(area.id.clone())
                            .or_insert_with(|| pieces_sq_in(doc, &area.id));
                        let count = estimated_slab_count(sq_in, size);
                        estimates.insert(key, count);
                        count
                    };
                    let estimate = std::cell::RefCell::new(estimate);
                    drawing_lines(
                        &mut b,
                        pl,
                        &area.id,
                        option.by_area.get(&area.id),
                        t,
                        &slab_count_for(doc, &area.id, &option.id),
                        doc.price_overrides
                            .get(&format!("{}|material", area.id))
                            .and_then(|o| o.get(&option.id))
                            .copied(),
                        &|size| (estimate.borrow_mut())(size),
                    );
                    let items: Vec<&QuoteItem> = doc
                        .items
                        .iter()
                        .filter(|i| i.area_id.as_deref() == Some(area.id.as_str()))
                        .collect();
                    item_lines(&mut b, pl, &items, Some(round_takeoff(t).fabrication_sq_ft));
                    #[allow(clippy::cast_precision_loss)]
                    let quantity = area.quantity as f64;
                    AreaPrice {
                        area_id: Some(area.id.clone()),
                        name: area.name.clone(),
                        quantity,
                        subtotal: sum_totals(&b.lines) * quantity,
                        missing: b.lines.iter().any(|l| l.missing),
                        lines: b.lines,
                    }
                })
                .collect();
            let mut ob = Builder {
                lines: Vec::new(),
                overrides: &doc.price_overrides,
                option_id: &option.id,
                dollars,
            };
            let others: Vec<&QuoteItem> = doc
                .items
                .iter()
                .filter(|i| {
                    i.area_id
                        .as_deref()
                        .is_none_or(|id| id.is_empty() || !doc.areas.iter().any(|a| a.id == id))
                })
                .collect();
            item_lines(&mut ob, pl, &others, None);
            let other = AreaPrice {
                area_id: None,
                name: "Other Items".into(),
                quantity: 1.0,
                subtotal: sum_totals(&ob.lines),
                missing: ob.lines.iter().any(|l| l.missing),
                lines: ob.lines,
            };
            let all: Vec<&AreaPrice> = areas.iter().chain(std::iter::once(&other)).collect();
            let all_lines = || {
                all.iter()
                    .flat_map(|a| a.lines.iter().map(move |l| (l, a.quantity)))
            };
            let discountable = all_lines()
                .filter(|(l, _)| l.flags.allow_discount)
                .fold(0.0, |s, (l, q)| s + l.total * q);
            let discount = js_min(
                js_max(discountable, 0.0),
                round_money(discountable * opts.discount_pct / 100.0, dollars)
                    + js_max(opts.discount_amount, 0.0),
            );
            let taxable = all_lines()
                .filter(|(l, _)| l.flags.taxable)
                .fold(0.0, |s, (l, q)| s + l.total * q);
            let tax = round_money(taxable * opts.tax_pct / 100.0, false);
            let subtotal = all.iter().fold(0.0, |s, a| s + a.subtotal);
            let missing = all.iter().any(|a| a.missing);
            OptionPrice {
                option_id: option.id.clone(),
                areas,
                other,
                discount,
                tax,
                total: subtotal - discount + tax,
                missing,
            }
        })
        .collect()
}
