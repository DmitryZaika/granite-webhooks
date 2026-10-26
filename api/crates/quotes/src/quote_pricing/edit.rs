//! Editing price lists and the extras catalog, as the CRM does it.
//!
//! `normalizePriceList` and `validatePriceList` (`priceListEdit.ts`), and the
//! catalog helpers of `extrasCatalog.ts`: ids for options and tiers, a list's
//! own extras, and the one-time move of the lists' own extras into the
//! catalog.

use super::js::{arr, get, truthy};
use serde_json::{Map, Value, json};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

/// Every price list array whose entries are scoped by a `materialFilter`,
/// with its label (`RULE_ARRAY_KEYS`, `RULE_LABELS`).
pub const RULES: [(&str, &str); 26] = [
    ("arrFabricationCharges", "Fabrication"),
    ("arrInstallationCharges", "Installation"),
    ("arrSplashes", "Splashes"),
    (
        "arrWaterfallMiteredEdgeMaterialCharges",
        "Waterfall / mitered material",
    ),
    ("arrWaterfallMiteredEdgeCutCharges", "Mitered cuts"),
    ("arrWaterfallInstallationCharges", "Waterfall installation"),
    ("arrEdgeProfileSets", "Edge profiles"),
    ("arrApplianceEdges", "Appliance edge"),
    ("arrRadiusEdges", "Full radius edge"),
    ("arrOutsideRadiusCorners", "Outside radius corners"),
    ("arrInsideRadiusCorners", "Inside radius corners"),
    ("arrClippedCorners", "Clipped corners"),
    ("arrBumpedOutCorners", "Bumped out corners"),
    ("arrNotchedCorners", "Notched corners"),
    ("arrInsideDiagonalCorners", "Inside diagonal corners"),
    ("arrRecessedDiagonalCorners", "Recessed diagonal corners"),
    ("arrBumpOutArcs", "Bump-out arcs"),
    ("arrBumpInArcs", "Bump-in arcs"),
    ("arrBumpOutPolygons", "Bump-outs"),
    ("arrBumpInPolygons", "Bump-ins"),
    ("arrSinkUndermounts", "Undermount sink cutouts"),
    ("arrSinkDropIns", "Drop-in sink cutouts"),
    ("arrSinkFarmers", "Farmer sink cutouts"),
    ("arrFaucetHoles", "Faucet holes"),
    ("arrOutletCutouts", "Outlet cutouts"),
    ("arrCooktops", "Cooktop cutouts"),
];

/// Rules whose size ranges are sorted by this key.
const RANGE_KEYS: [(&str, &str); 7] = [
    ("arrSplashes", "maxHeight"),
    ("arrOutsideRadiusCorners", "maxRadius"),
    ("arrInsideRadiusCorners", "maxRadius"),
    ("arrSinkUndermounts", "maxDimension"),
    ("arrSinkDropIns", "maxDimension"),
    ("arrSinkFarmers", "maxDimension"),
    ("arrCooktops", "maxDimension"),
];

fn name_of(row: &Value) -> &str {
    row.get("name").and_then(Value::as_str).unwrap_or_default()
}

/// Rows with their names trimmed, nameless ones dropped.
fn named(rows: Option<&Value>) -> Value {
    Value::Array(
        rows.and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|row| {
                let name = name_of(row).trim().to_string();
                if name.is_empty() {
                    return None;
                }
                let mut row = row.clone();
                if let Some(object) = row.as_object_mut() {
                    object.insert("name".into(), Value::String(name));
                }
                Some(row)
            })
            .collect(),
    )
}

/// `byMax(key)`: ascending, ranges without a maximum last.
fn by_max(key: &str) -> impl Fn(&Value, &Value) -> Ordering + '_ {
    move |a, b| match (get(Some(a), key), get(Some(b), key)) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(x), Some(y)) => {
            let (x, y) = (super::js::to_number(x), super::js::to_number(y));
            x.partial_cmp(&y).unwrap_or(Ordering::Equal)
        }
    }
}

fn sorted(ranges: &Value, key: &str) -> Value {
    let mut ranges = ranges.as_array().cloned().unwrap_or_default();
    ranges.sort_by(by_max(key));
    Value::Array(ranges)
}

/// `normalizePriceList`: names trimmed, blank rows dropped, size ranges
/// sorted ascending (pricing picks the first range that fits).
pub fn normalize(input: &Value) -> Value {
    let mut info = input.clone();
    let Some(list) = info.as_object_mut() else {
        return info;
    };
    let products: Vec<Value> = named(list.get("arrProducts"))
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|mut product| {
            let groups = named(product.get("arrPriceGroups"));
            let variants = named(product.get("arrVariants"));
            product["arrPriceGroups"] = groups;
            product["arrVariants"] = variants;
            product
        })
        .collect();
    list.insert("arrProducts".into(), Value::Array(products));
    for (key, max) in RANGE_KEYS {
        if let Some(rules) = list.get_mut(key).and_then(Value::as_array_mut) {
            for rule in rules {
                if let Some(ranges) = rule.get("arrRanges") {
                    let ranges = sorted(ranges, max);
                    rule["arrRanges"] = ranges;
                }
            }
        }
    }
    if let Some(sets) = list
        .get_mut("arrEdgeProfileSets")
        .and_then(Value::as_array_mut)
    {
        for set in sets {
            let profiles = named(set.get("arrEdgeProfiles"));
            set["arrEdgeProfiles"] = profiles;
            if truthy(set.get("arrPriceGroups")) {
                let groups = named(set.get("arrPriceGroups"));
                set["arrPriceGroups"] = groups;
            }
        }
    }
    let sinks = named(list.get("arrSinkModels"));
    list.insert("arrSinkModels".into(), sinks);
    let cutouts = named(list.get("arrSinkCutoutOptions"));
    list.insert("arrSinkCutoutOptions".into(), cutouts);
    let others: Vec<Value> = list
        .get("arrOtherCutouts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|c| !name_of(c).trim().is_empty())
        .cloned()
        .collect();
    list.insert("arrOtherCutouts".into(), Value::Array(others));
    let items: Vec<Value> = named(list.get("arrOtherItems"))
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|mut item| {
            if let Some(object) = item.as_object_mut() {
                let category = object
                    .get("category")
                    .and_then(Value::as_str)
                    .map(|c| c.trim().to_string())
                    .filter(|c| !c.is_empty());
                match category {
                    Some(category) => {
                        object.insert("category".into(), Value::String(category));
                    }
                    None => {
                        object.remove("category");
                    }
                }
                if truthy(object.get("arrOptions")) {
                    let options = named(object.get("arrOptions"));
                    object.insert("arrOptions".into(), options);
                }
                if truthy(object.get("arrRanges")) {
                    let ranges = sorted(&object["arrRanges"], "maxDimension");
                    object.insert("arrRanges".into(), ranges);
                }
            }
            item
        })
        .collect();
    list.insert("arrOtherItems".into(), Value::Array(items));
    info
}

/// Names listed more than once, each once, in order (`dupes`).
fn dupes(names: &[String]) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for (i, name) in names.iter().enumerate() {
        if !name.is_empty() && names[..i].contains(name) && !found.contains(name) {
            found.push(name.clone());
        }
    }
    found
}

fn names(rows: &[Value]) -> Vec<String> {
    rows.iter().map(|r| name_of(r).to_string()).collect()
}

/// `${filter.product ?? ''}\0${filter.productVariant ?? ''}`.
fn scope_of(rule: &Value) -> String {
    let filter = rule.get("materialFilter");
    let text = |key: &str| {
        get(filter, key).map_or_else(String::new, |v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
    };
    format!("{}\u{0}{}", text("product"), text("productVariant"))
}

/// `validatePriceList`: problems that would make quotes price ambiguously;
/// empty when valid.
pub fn validate(info: &Value) -> Vec<String> {
    let mut errors = Vec::new();
    let report = |errors: &mut Vec<String>, what: &str, list: Vec<String>| {
        for d in dupes(&list) {
            errors.push(format!("{what} \"{d}\" is listed more than once."));
        }
    };
    let products = arr(Some(info), "arrProducts");
    report(&mut errors, "Material", names(products));
    for product in products {
        let product_name = name_of(product);
        report(
            &mut errors,
            &format!("{product_name} color"),
            names(arr(Some(product), "arrVariants")),
        );
        let groups = arr(Some(product), "arrPriceGroups");
        report(
            &mut errors,
            &format!("{product_name} price group"),
            names(groups),
        );
        let mut ranged: Vec<(String, f64, f64)> = Vec::new();
        for group in groups {
            let label = format!("{product_name} price group \"{}\"", name_of(group));
            let level = |key: &str| get(Some(group), key).map(super::js::to_number);
            match (level("levelFrom"), level("levelTo")) {
                (None, None) => {}
                (Some(_), None) | (None, Some(_)) => {
                    errors.push(format!("{label}: set both levels of the range."));
                }
                (Some(from), Some(to)) if from > to => {
                    errors.push(format!("{label}: the first level is higher than the last."));
                }
                (Some(from), Some(to)) => {
                    if let Some(clash) = ranged.iter().find(|r| from <= r.2 && to >= r.1) {
                        errors.push(format!(
                            "{label}: its levels overlap with price group \"{}\".",
                            clash.0
                        ));
                    }
                    ranged.push((name_of(group).to_string(), from, to));
                }
            }
        }
    }
    for set in arr(Some(info), "arrEdgeProfileSets") {
        let scope = get(set.get("materialFilter"), "product")
            .and_then(Value::as_str)
            .unwrap_or("Default")
            .to_string();
        report(
            &mut errors,
            &format!("{scope} finished edge"),
            names(arr(Some(set), "arrEdgeProfiles")),
        );
        report(
            &mut errors,
            &format!("{scope} edge price group"),
            names(arr(Some(set), "arrPriceGroups")),
        );
    }
    report(
        &mut errors,
        "Extra",
        names(arr(Some(info), "arrOtherItems")),
    );
    report(
        &mut errors,
        "Sink model",
        names(arr(Some(info), "arrSinkModels")),
    );
    for (key, label) in RULES {
        let scopes: Vec<String> = arr(Some(info), key).iter().map(scope_of).collect();
        if !dupes(&scopes).is_empty() {
            errors.push(format!("{label} has two prices for the same material."));
        }
    }
    if let Some(cooktop) = get(Some(info), "defaultCooktopCutout").filter(|c| truthy(Some(c))) {
        let positive =
            |key: &str| get(Some(cooktop), key).is_some_and(|v| super::js::to_number(v) > 0.0);
        if !(positive("cutoutWidth") && positive("cutoutDepth")) {
            errors.push("Default cooktop cutout: set a length and width above 0.".into());
        }
    }
    for item in arr(Some(info), "arrOtherItems") {
        if !arr(Some(item), "arrOptions").is_empty() && !arr(Some(item), "arrRanges").is_empty() {
            errors.push(format!(
                "Extra \"{}\" has both options and a per-unit price; keep one.",
                name_of(item)
            ));
        }
    }
    errors
}

/// `newId(prefix)`: `prefix-` and 8 random base-36 characters.
pub fn new_id(prefix: &str) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut bytes = [0u8; 8];
    let _ = getrandom::fill(&mut bytes);
    let tail: String = bytes
        .iter()
        .map(|b| DIGITS[usize::from(*b) % DIGITS.len()] as char)
        .collect();
    format!("{prefix}-{tail}")
}

/// `withExtraIds`: every option and tier gets an id, so links and quotes
/// survive renames.
pub fn with_extra_ids(item: &Value) -> Value {
    let mut item = item.clone();
    for (key, prefix) in [("arrOptions", "opt"), ("arrRanges", "tier")] {
        if let Some(rows) = item.get_mut(key).and_then(Value::as_array_mut) {
            for row in rows {
                if !truthy(row.get("id"))
                    && let Some(object) = row.as_object_mut()
                {
                    object.insert("id".into(), Value::String(new_id(prefix)));
                }
            }
        }
    }
    item
}

/// The item without its `catalogId`.
pub fn without_catalog_id(item: &Value) -> Value {
    let mut item = item.clone();
    if let Some(object) = item.as_object_mut() {
        object.remove("catalogId");
    }
    item
}

/// `ownExtras`: the list's own extras, not catalog extras merged in.
pub fn own_extras(info: &Value) -> Vec<Value> {
    arr(Some(info), "arrOtherItems")
        .iter()
        .filter(|o| get(Some(o), "catalogId").is_none())
        .cloned()
        .collect()
}

fn lower(text: &str) -> String {
    text.trim().to_lowercase()
}

/// `catalogItemsToCreate`: catalog extras to create so every list's own
/// extras have one (the first list's version wins).
pub fn catalog_items_to_create(lists: &[Value], catalog: &[(i64, Value)]) -> Vec<Value> {
    let mut known: HashSet<String> = catalog
        .iter()
        .map(|(_, item)| lower(name_of(item)))
        .collect();
    let mut create = Vec::new();
    for info in lists {
        for item in own_extras(info) {
            if !known.insert(lower(name_of(&item))) {
                continue;
            }
            create.push(with_extra_ids(&without_catalog_id(&item)));
        }
    }
    create
}

/// A number, `undefined` (missing) kept apart.
fn amount(row: &Value, key: &str) -> Option<f64> {
    row.get(key).and_then(Value::as_f64)
}

/// `listAfterMove`: the list after its own extras moved to the catalog.
///
/// They leave the list, and each becomes a link that keeps the list's
/// prices where they differ; catalog extras just created that the list
/// never had are switched off.
pub fn list_after_move<S: std::hash::BuildHasher>(
    info: &Value,
    catalog: &[(i64, Value)],
    created: &HashSet<i64, S>,
) -> Value {
    // `new Map(...)`: a later extra of the same name wins.
    let by_name: HashMap<String, &(i64, Value)> = catalog
        .iter()
        .map(|extra| (lower(name_of(&extra.1)), extra))
        .collect();
    let mut links: Map<String, Value> = info
        .get("extraLinks")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut own = Vec::new();
    let mut had = HashSet::new();
    for item in own_extras(info) {
        let Some((id, extra)) = by_name.get(&lower(name_of(&item))).copied() else {
            own.push(item);
            continue;
        };
        had.insert(*id);
        let mut prices = Map::new();
        for option in arr(Some(extra), "arrOptions") {
            let mine = arr(Some(&item), "arrOptions")
                .iter()
                .find(|x| lower(name_of(x)) == lower(name_of(option)));
            if let (Some(option_id), Some(mine)) = (
                option
                    .get("id")
                    .filter(|v| truthy(Some(v)))
                    .and_then(Value::as_str),
                mine,
            ) && amount(mine, "flatPrice") != amount(option, "flatPrice")
            {
                let mut set = json!({ "adjustType": "price" });
                if let Some(flat) = mine.get("flatPrice") {
                    set["flatPrice"] = flat.clone();
                }
                prices.insert(option_id.to_string(), set);
            }
        }
        for (i, range) in arr(Some(extra), "arrRanges").iter().enumerate() {
            let Some(range_id) = range
                .get("id")
                .filter(|v| truthy(Some(v)))
                .and_then(Value::as_str)
            else {
                continue;
            };
            let Some(mine) = arr(Some(&item), "arrRanges").get(i) else {
                continue;
            };
            let mut diff = Map::new();
            for key in ["flatPrice", "unitPrice"] {
                let ours = amount(mine, key).unwrap_or(0.0);
                if (ours - amount(range, key).unwrap_or(0.0)).abs() > 0.0 {
                    diff.insert(
                        key.into(),
                        mine.get(key).cloned().unwrap_or_else(|| json!(0)),
                    );
                }
            }
            if !diff.is_empty() {
                let mut set = Map::new();
                set.insert("adjustType".into(), json!("price"));
                set.extend(diff);
                prices.insert(range_id.to_string(), Value::Object(set));
            }
        }
        if prices.is_empty() {
            links.remove(&id.to_string());
        } else {
            links.insert(id.to_string(), json!({ "prices": prices }));
        }
    }
    let mut created: Vec<i64> = created.iter().copied().collect();
    created.sort_unstable();
    for id in created {
        if !had.contains(&id) {
            links.insert(id.to_string(), json!({ "off": true }));
        }
    }
    let mut info = info.clone();
    info["arrOtherItems"] = Value::Array(own);
    info["extraLinks"] = Value::Object(links);
    info
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizing_trims_drops_and_sorts() {
        let info = json!({
            "arrProducts": [{ "name": " Granite ", "arrVariants": [{ "name": "" }, { "name": " Black " }] }, { "name": "  " }],
            "arrSplashes": [{ "arrRanges": [{ "maxHeight": null }, { "maxHeight": 6 }, { "maxHeight": 4 }] }],
            "arrOtherItems": [{ "name": "Tear out", "category": "  " }],
            "arrOtherCutouts": [{ "name": " " }, { "name": "Vent " }],
        });
        let out = normalize(&info);
        assert_eq!(
            out["arrProducts"],
            json!([{ "name": "Granite", "arrPriceGroups": [], "arrVariants": [{ "name": "Black" }] }])
        );
        assert_eq!(
            out["arrSplashes"][0]["arrRanges"],
            json!([{ "maxHeight": 4 }, { "maxHeight": 6 }, { "maxHeight": null }])
        );
        assert_eq!(out["arrOtherItems"], json!([{ "name": "Tear out" }]));
        assert_eq!(out["arrOtherCutouts"], json!([{ "name": "Vent " }]));
        assert_eq!(out["arrSinkModels"], json!([]));
    }

    #[test]
    fn validation_finds_duplicates_and_overlaps() {
        let info = json!({
            "arrProducts": [
                { "name": "Granite", "arrVariants": [{ "name": "Black" }, { "name": "Black" }],
                  "arrPriceGroups": [{ "name": "A", "levelFrom": 1, "levelTo": 3 }, { "name": "B", "levelFrom": 2, "levelTo": 4 }, { "name": "C", "levelFrom": 1 }] },
            ],
            "arrFabricationCharges": [{ "materialFilter": {} }, { "materialFilter": { "product": null } }],
            "arrOtherItems": [{ "name": "X", "arrOptions": [{ "name": "a" }], "arrRanges": [{}] }],
            "defaultCooktopCutout": { "cutoutWidth": 0, "cutoutDepth": 20 },
        });
        assert_eq!(
            validate(&info),
            vec![
                "Granite color \"Black\" is listed more than once.".to_string(),
                "Granite price group \"B\": its levels overlap with price group \"A\".".into(),
                "Granite price group \"C\": set both levels of the range.".into(),
                "Fabrication has two prices for the same material.".into(),
                "Default cooktop cutout: set a length and width above 0.".into(),
                "Extra \"X\" has both options and a per-unit price; keep one.".into(),
            ]
        );
    }

    #[test]
    fn moving_extras_links_the_list_to_the_catalog() {
        let info = json!({ "arrOtherItems": [
            { "name": "Haul away", "arrRanges": [{ "flatPrice": 90 }] },
            { "name": "Kept", "catalogId": 9 },
        ] });
        let catalog = vec![
            (
                1,
                json!({ "name": "Haul Away", "arrRanges": [{ "id": "tier-1", "flatPrice": 75 }] }),
            ),
            (2, json!({ "name": "Tear out" })),
        ];
        let out = list_after_move(&info, &catalog, &HashSet::from([1, 2]));
        assert_eq!(out["arrOtherItems"], json!([]));
        assert_eq!(
            out["extraLinks"],
            json!({ "1": { "prices": { "tier-1": { "adjustType": "price", "flatPrice": 90 } } }, "2": { "off": true } })
        );
        let made = catalog_items_to_create(&[info], &catalog[1..]);
        assert_eq!(made.len(), 1);
        assert!(
            made[0]["arrRanges"][0]["id"]
                .as_str()
                .unwrap()
                .starts_with("tier-")
        );
    }
}
