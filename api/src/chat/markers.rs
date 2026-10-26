//! What the chat model writes besides its answer.
//!
//! And the price-list follow-ups built on it. Ports of the CRM's `thicknessOptions.ts`,
//! `slabSizeOptions.ts`, `priceListOptions.ts`, `specialOrderCalculator.ts`,
//! `priceListMessages.ts` and `chatAnswerHelpers.ts`.
//!
//! The model ends answers with markers (`[[SOURCE:2]]`,
//! `[[THICKNESS_OPTIONS:2 CM=36.78|3 CM=44]]`, `[[SPECIAL_ORDER:price=..]]`);
//! they are read here and stripped before the user sees the answer.

use crate::followups::js::{js_number, js_parse_float, js_parse_int, js_to_fixed, trim};
use crate::{fre, re};
use serde::Serialize;

/// A thickness the user can pick (`2 CM` at $36.78 / sq ft).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ThicknessOption {
    pub label: String,
    pub price: f64,
}

/// A slab size the user can pick.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SlabSizeOption {
    pub label: String,
    pub length_inches: f64,
    pub width_inches: f64,
}

/// One of several products the search found.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PriceListOption {
    pub label: String,
    pub price_per_sqft: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub length_inches: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width_inches: Option<f64>,
}

/// A price the user may turn into a special order.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpecialOrderOffer {
    pub price_per_sqft: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub length_inches: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width_inches: Option<f64>,
}

impl SpecialOrderOffer {
    /// `isCompleteSpecialOrderOffer`: the slab size is known.
    pub fn is_complete(&self) -> bool {
        matches!((self.length_inches, self.width_inches),
            (Some(l), Some(w)) if l.is_finite() && w.is_finite() && l > 0.0 && w > 0.0)
    }
}

fn first_capture(re: &regex::Regex, text: &str) -> Option<String> {
    re.captures(text).map(|c| {
        c.get(1)
            .map_or_else(String::new, |m| m.as_str().to_string())
    })
}

/// `/\s+/g` → `' '`, trimmed.
fn squash(text: &str) -> String {
    trim(&re!(r"\s+").replace_all(text, " ")).to_string()
}

/// `Number.parseFloat(raw.replace(/[$,]/g, ''))`, positive only.
fn parse_price(raw: &str) -> Option<f64> {
    let price = js_parse_float(&raw.replace(['$', ','], ""));
    (price.is_finite() && price > 0.0).then_some(price)
}

/// Keeps the first option per lowercase label.
fn dedupe<T>(options: Vec<T>, key: impl Fn(&T) -> String) -> Vec<T> {
    let mut seen = std::collections::HashSet::new();
    options
        .into_iter()
        .filter(|o| seen.insert(key(o)))
        .collect()
}

fn thickness_marker() -> &'static regex::Regex {
    re!(
        r"\[+\s*THICKNESS\s*_?\s*OPTIONS\s*[:#-]?\s*([^\]]*?)\s*\]+",
        "i"
    )
}

pub fn parse_thickness_options_marker(text: &str) -> Option<Vec<ThicknessOption>> {
    let raw = first_capture(thickness_marker(), text)?;
    let options = raw
        .split('|')
        .filter_map(|part| {
            let eq = part.find('=').filter(|eq| *eq > 0)?;
            let label = squash(&part[..eq]);
            let price = parse_price(&part[eq + 1..])?;
            (!label.is_empty()).then_some(ThicknessOption { label, price })
        })
        .collect();
    let unique = dedupe(options, |o: &ThicknessOption| o.label.to_lowercase());
    (unique.len() >= 2).then_some(unique)
}

pub fn strip_thickness_options_marker(text: &str) -> String {
    trim(&thickness_marker().replace(text, "")).to_string()
}

fn fancy_captures(re: &fancy_regex::Regex, text: &str) -> Vec<Vec<Option<String>>> {
    re.captures_iter(text)
        .filter_map(Result::ok)
        .map(|c| {
            (0..c.len())
                .map(|i| c.get(i).map(|m| m.as_str().to_string()))
                .collect()
        })
        .collect()
}

pub fn infer_thickness_options_from_answer(text: &str) -> Option<Vec<ThicknessOption>> {
    if let Some(found) = parse_thickness_options_marker(text) {
        return Some(found);
    }
    let cleaned = strip_thickness_options_marker(text)
        .replace("**", "")
        .replace('\u{2022}', "-");
    let mut options = Vec::new();
    let mut take = |caps: Vec<Vec<Option<String>>>| {
        for c in caps {
            let label = squash(c[1].as_deref().unwrap_or_default());
            if let Some(price) = parse_price(c[2].as_deref().unwrap_or_default())
                && !label.is_empty()
            {
                options.push(ThicknessOption { label, price });
            }
        }
    };
    take(fancy_captures(
        fre!(
            r"(?:^|\n)\s*(?:[-*•]|\d+\.)?\s*(?<![\d.])(\d+(?:\.\d+)?\s*(?:CM|cm|mm))\s*:\s*\$?\s*([\d,]+(?:\.\d{1,2})?)"
        ),
        &cleaned,
    ));
    if options.len() < 2 {
        let loose = fancy_captures(
            fre!(r"(?<![\d.])(\d+(?:\.\d+)?\s*(?:CM|cm|mm))\s*:\s*\$?\s*([\d,]+(?:\.\d{1,2})?)"),
            &cleaned,
        );
        for c in loose {
            let label = squash(c[1].as_deref().unwrap_or_default());
            if let Some(price) = parse_price(c[2].as_deref().unwrap_or_default())
                && !label.is_empty()
            {
                options.push(ThicknessOption { label, price });
            }
        }
    }
    let unique = dedupe(options, |o: &ThicknessOption| o.label.to_lowercase());
    (unique.len() >= 2).then_some(unique)
}

fn slab_size_marker() -> &'static regex::Regex {
    re!(
        r"\[+\s*SLAB\s*_?\s*SIZE\s*_?\s*OPTIONS\s*[:#-]?\s*([^\]]*?)\s*\]+",
        "i"
    )
}

fn slab_option(length: &str, width: &str) -> Option<SlabSizeOption> {
    let (l, w) = (js_parse_float(length), js_parse_float(width));
    (l.is_finite() && w.is_finite() && l > 0.0 && w > 0.0).then(|| SlabSizeOption {
        label: format!("{}\" × {}\"", js_number(l), js_number(w)),
        length_inches: l,
        width_inches: w,
    })
}

fn slab_key(o: &SlabSizeOption) -> String {
    format!(
        "{}x{}",
        js_number(o.length_inches),
        js_number(o.width_inches)
    )
}

pub fn parse_slab_size_options_marker(text: &str) -> Option<Vec<SlabSizeOption>> {
    let raw = first_capture(slab_size_marker(), text)?;
    let options = raw
        .split('|')
        .filter_map(|part| {
            let dims = re!(r"^(\d{2,3})\s*[×xX]\s*(\d{2,3})$").captures(trim(part))?;
            slab_option(&dims[1], &dims[2])
        })
        .collect();
    let unique = dedupe(options, slab_key);
    (unique.len() >= 2).then_some(unique)
}

pub fn strip_slab_size_options_marker(text: &str) -> String {
    trim(&slab_size_marker().replace(text, "")).to_string()
}

pub fn infer_slab_size_options_from_answer(text: &str) -> Option<Vec<SlabSizeOption>> {
    if let Some(found) = parse_slab_size_options_marker(text) {
        return Some(found);
    }
    let cleaned = strip_slab_size_options_marker(text).replace("**", "");
    let mut options = Vec::new();
    if let Some(range) =
        re!(r"(\d{2,3})\s*[×xX]\s*(\d{2,3})\s*[–—-]\s*(\d{2,3})\s*[×xX]\s*(\d{2,3})")
            .captures(&cleaned)
    {
        options.extend(slab_option(&range[1], &range[2]));
        options.extend(slab_option(&range[3], &range[4]));
    }
    if options.len() < 2 {
        for c in re!(r#"(\d{2,3})\s*(?:["”]|in(?:ch(?:es)?)?)?\s*[×xX]\s*(\d{2,3})\s*(?:["”]|in(?:ch(?:es)?)?)?"#)
            .captures_iter(&cleaned)
        {
            options.extend(slab_option(&c[1], &c[2]));
        }
    }
    let unique = dedupe(options, slab_key);
    (unique.len() >= 2).then_some(unique)
}

fn price_list_marker() -> &'static regex::Regex {
    re!(
        r"\[+\s*PRICE\s*_?\s*LIST\s*_?\s*OPTIONS\s*[:#-]?\s*([^\]]*?)\s*\]+",
        "i"
    )
}

fn is_thickness_label(label: &str) -> bool {
    re!(r"^\d+(?:\.\d+)?\s*(?:cm|mm)$", "i").is_match(trim(label))
}

fn price_list_option(
    label: &str,
    price: &str,
    length: Option<&str>,
    width: Option<&str>,
) -> Option<PriceListOption> {
    let label = squash(&re!(r"\*\s*$").replace(label, "*"));
    let price_per_sqft = parse_price(price)?;
    if label.is_empty() || is_thickness_label(&label) {
        return None;
    }
    let mut option = PriceListOption {
        label,
        price_per_sqft,
        length_inches: None,
        width_inches: None,
    };
    if let (Some(l), Some(w)) = (
        length.filter(|s| !s.is_empty()),
        width.filter(|s| !s.is_empty()),
    ) {
        let (l, w) = (js_parse_float(l), js_parse_float(w));
        if l.is_finite() && w.is_finite() && l > 0.0 && w > 0.0 {
            option.length_inches = Some(l);
            option.width_inches = Some(w);
        }
    }
    Some(option)
}

pub fn parse_price_list_options_marker(text: &str) -> Option<Vec<PriceListOption>> {
    let raw = first_capture(price_list_marker(), text)?;
    let options = raw
        .split('|')
        .filter_map(|part| {
            let trimmed = trim(part);
            let eq = trimmed.find('=').filter(|eq| *eq > 0)?;
            let values: Vec<&str> = trimmed[eq + 1..].split(',').map(trim).collect();
            let price = values.first().copied().filter(|p| !p.is_empty())?;
            price_list_option(
                &trimmed[..eq],
                price,
                values.get(1).copied(),
                values.get(2).copied(),
            )
        })
        .collect();
    let unique = dedupe(options, |o: &PriceListOption| o.label.to_lowercase());
    (unique.len() >= 2).then_some(unique)
}

pub fn strip_price_list_options_marker(text: &str) -> String {
    trim(&price_list_marker().replace(text, "")).to_string()
}

pub fn infer_price_list_options_from_answer(text: &str) -> Option<Vec<PriceListOption>> {
    if let Some(found) = parse_price_list_options_marker(text) {
        return Some(found);
    }
    let cleaned = strip_price_list_options_marker(text).replace("**", "");
    let options = re!(
        r"(?:^|\n)\s*(?:[-*•]|\d+\.)?\s*(.+?)\s+[—–]\s+\$\s*([\d,]+(?:\.\d{1,2})?)(?:\s*,\s*(\d{2,3})\s*[×xX]\s*(\d{2,3})(?:\s*slab)?)?",
        "gi"
    )
    .captures_iter(&cleaned)
    .filter_map(|c| {
        price_list_option(&c[1], &c[2], c.get(3).map(|m| m.as_str()), c.get(4).map(|m| m.as_str()))
    })
    .collect();
    let unique = dedupe(options, |o: &PriceListOption| o.label.to_lowercase());
    (unique.len() >= 2).then_some(unique)
}

// ---------- special orders ----------

const MIN_PLAUSIBLE_PER_SQFT: f64 = 5.0;
const MAX_PLAUSIBLE_PER_SQFT: f64 = 250.0;

pub const SPECIAL_ORDER_QUESTION: &str =
    "\n\nWould you like to adjust this price for a special order?";

fn special_order_marker() -> &'static regex::Regex {
    re!(
        r"\[+\s*SPECIAL\s*_?\s*ORDER\s*[:#-]?\s*([^\]]*?)\s*\]+",
        "i"
    )
}

pub fn slab_area_sqft(length: f64, width: f64) -> f64 {
    length * width / 144.0
}

/// A slab total turned into a price per sq ft when it cannot be one.
pub fn price_as_per_sqft(price: f64, length: Option<f64>, width: Option<f64>) -> f64 {
    let (Some(l), Some(w)) = (length, width) else {
        return price;
    };
    if !l.is_finite() || !w.is_finite() || l <= 0.0 || w <= 0.0 {
        return price;
    }
    let area = slab_area_sqft(l, w);
    if area <= 0.0 {
        return price;
    }
    let converted = price / area;
    let slab_total = price > MAX_PLAUSIBLE_PER_SQFT
        && (MIN_PLAUSIBLE_PER_SQFT..=MAX_PLAUSIBLE_PER_SQFT).contains(&converted);
    if slab_total { converted } else { price }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DeliveryCostType {
    Fixed,
    Percentage,
}

/// What a special order costs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpecialOrderInput {
    pub price_per_sqft: f64,
    pub length_inches: f64,
    pub width_inches: f64,
    pub slabs: f64,
    pub delivery_cost: f64,
    pub delivery_cost_type: Option<DeliveryCostType>,
    pub tax_rate: f64,
}

/// `calculateSpecialOrder` + `formatSpecialOrderResult`.
pub fn special_order_answer(input: &SpecialOrderInput) -> String {
    let area_per_slab = input.width_inches * input.length_inches / 144.0;
    let total_sqft = area_per_slab * input.slabs;
    let material = area_per_slab * input.price_per_sqft * input.slabs * (1.0 + input.tax_rate);
    let delivery = if input.delivery_cost_type == Some(DeliveryCostType::Percentage) {
        material * (input.delivery_cost / 100.0)
    } else {
        input.delivery_cost
    };
    let total = material + delivery;
    let per_sqft = if total_sqft == 0.0 || total_sqft.is_nan() {
        0.0
    } else {
        total / total_sqft
    };
    let tax_percent = re!(r"\.?0+$")
        .replace(&js_to_fixed(input.tax_rate * 100.0, 2), "")
        .into_owned();
    let delivery_line = if input.delivery_cost_type == Some(DeliveryCostType::Percentage) {
        format!(
            "Delivery ({}%): ${}",
            js_number(input.delivery_cost),
            js_to_fixed(delivery, 2)
        )
    } else {
        format!("Delivery: ${}", js_to_fixed(delivery, 2))
    };
    [
        format!(
            "Price per sqft (with tax & delivery): ${}",
            js_to_fixed(per_sqft, 2)
        ),
        format!("Total square feet: {} sqft", js_to_fixed(total_sqft, 2)),
        format!(
            "Material cost (incl. {tax_percent}% tax): ${}",
            js_to_fixed(material, 2)
        ),
        delivery_line,
        format!("Total cost: ${}", js_to_fixed(total, 2)),
    ]
    .join("\n")
}

pub fn parse_special_order_marker(text: &str) -> Option<SpecialOrderOffer> {
    let raw = first_capture(special_order_marker(), text)?;
    let mut params: Vec<(String, String)> = Vec::new();
    for part in raw.split(',') {
        let mut kv = part.split('=').map(trim);
        let (Some(key), Some(value)) = (kv.next(), kv.next()) else {
            continue;
        };
        if !key.is_empty() && !value.is_empty() {
            let key = key.to_lowercase();
            params.retain(|(k, _)| *k != key);
            params.push((key, value.to_string()));
        }
    }
    let get = |k: &str| {
        params
            .iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.as_str())
    };
    let price = js_parse_float(get("price").unwrap_or_default());
    if !price.is_finite() || price <= 0.0 {
        return None;
    }
    if get("length").is_none() && get("width").is_none() {
        return Some(SpecialOrderOffer {
            price_per_sqft: price,
            length_inches: None,
            width_inches: None,
        });
    }
    let l = js_parse_float(get("length").unwrap_or_default());
    let w = js_parse_float(get("width").unwrap_or_default());
    if !l.is_finite() || !w.is_finite() || l <= 0.0 || w <= 0.0 {
        return None;
    }
    Some(SpecialOrderOffer {
        price_per_sqft: price_as_per_sqft(price, Some(l), Some(w)),
        length_inches: Some(l),
        width_inches: Some(w),
    })
}

pub fn strip_special_order_marker(text: &str) -> String {
    trim(&special_order_marker().replace(text, "")).to_string()
}

fn slab_dimensions(text: &str) -> Option<(f64, f64)> {
    if let Some(r) =
        re!(r"(\d{2,3})\s*[×xX]\s*(\d{2,3})\s*[–—-]\s*(\d{2,3})\s*[×xX]\s*(\d{2,3})").captures(text)
    {
        let v: Vec<f64> = (1..=4).map(|i| js_parse_float(&r[i])).collect();
        if v.iter().all(|x| x.is_finite() && *x > 0.0) {
            return Some((v[0].min(v[2]), v[1].min(v[3])));
        }
    }
    let dims = re!(r"(\d{2,3})\s*[×xX]\s*(\d{2,3})")
        .captures(text)
        .or_else(|| {
            re!(r"(\d{2,3})\s*(?:inch|in)?(?:es)?\s*[×xX]\s*(\d{2,3})", "i").captures(text)
        })?;
    let (l, w) = (js_parse_float(&dims[1]), js_parse_float(&dims[2]));
    (l.is_finite() && w.is_finite() && l > 0.0 && w > 0.0).then_some((l, w))
}

pub fn infer_special_order_offer_from_answer(text: &str) -> Option<SpecialOrderOffer> {
    if let Some(found) = parse_special_order_marker(text) {
        return Some(found);
    }
    let cleaned = strip_special_order_marker(text);
    let cleaned = re!(r"\[+\s*SOURCE\s*[:#-]?\s*[^\]]*?\s*\]+", "gi").replace_all(&cleaned, "");
    let cleaned = cleaned.replace("**", "");
    let raw = first_capture(re!(r"\$\s*([\d,]+(?:\.\d{1,2})?)"), &cleaned).or_else(|| {
        first_capture(
            re!(
                r"\b(?:listed|priced|quoted|costs?|is)\s+(?:at\s+)?([\d,]+(?:\.\d{1,2})?)\b",
                "i"
            ),
            &cleaned,
        )
    })?;
    let price = js_parse_float(&raw.replace(',', ""));
    if !price.is_finite() || price <= 0.0 {
        return None;
    }
    let lower = cleaned.to_lowercase();
    let per_slab = re!(r"\bper\s+(?:loose\s+)?slab\b").is_match(&lower);
    let per_sqft = re!(r"\bper\s+(?:sq\.?\s*ft|square\s*foot|sf)\b").is_match(&lower);
    let Some((l, w)) = slab_dimensions(&cleaned) else {
        if price > 250.0 {
            return None;
        }
        return Some(SpecialOrderOffer {
            price_per_sqft: price,
            length_inches: None,
            width_inches: None,
        });
    };
    let area = slab_area_sqft(l, w);
    if area <= 0.0 {
        return None;
    }
    let mut per = price;
    if per_slab && !per_sqft {
        per = price_as_per_sqft(price, Some(l), Some(w));
        #[allow(clippy::float_cmp)]
        if per == price {
            let converted = price / area;
            per = if (MIN_PLAUSIBLE_PER_SQFT..=MAX_PLAUSIBLE_PER_SQFT).contains(&converted) {
                converted
            } else {
                price
            };
        }
    } else if !per_sqft {
        per = price_as_per_sqft(price, Some(l), Some(w));
    }
    if per <= 0.0 || per > 250.0 {
        return None;
    }
    Some(SpecialOrderOffer {
        price_per_sqft: per,
        length_inches: Some(l),
        width_inches: Some(w),
    })
}

/// What a follow-up says about slabs and delivery.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct SlabsAndDelivery {
    pub slabs: Option<f64>,
    pub delivery_cost: Option<f64>,
    pub delivery_cost_type: Option<DeliveryCostType>,
}

pub fn parse_slabs_and_delivery(text: &str) -> SlabsAndDelivery {
    let lower = text.to_lowercase();
    let mut out = SlabsAndDelivery::default();
    if let Some(n) = first_capture(re!(r"(\d+(?:\.\d+)?)\s*slabs?"), &lower) {
        out.slabs = Some(js_parse_float(&n));
    }
    let percent = [
        re!(
            r"(?:delivery|deliver(?:y)?(?:\s+cost)?)[:\s]*(\d+(?:\.\d+)?)\s*%",
            "i"
        ),
        re!(r"(\d+(?:\.\d+)?)\s*%\s*(?:for\s+)?delivery", "i"),
    ];
    for pattern in percent {
        if let Some(n) = first_capture(pattern, text) {
            out.delivery_cost = Some(js_parse_float(&n));
            out.delivery_cost_type = Some(DeliveryCostType::Percentage);
            break;
        }
    }
    if out.delivery_cost.is_none() {
        let fixed = [
            re!(
                r"(?:delivery|deliver(?:y)?(?:\s+cost)?)[:\s]*\$?\s*(\d+(?:\.\d+)?)",
                "i"
            ),
            re!(r"\$?\s*(\d+(?:\.\d+)?)\s*(?:for\s+)?delivery", "i"),
            re!(r"delivery\s+\$?\s*(\d+(?:\.\d+)?)", "i"),
        ];
        for pattern in fixed {
            if let Some(n) = first_capture(pattern, text) {
                out.delivery_cost = Some(js_parse_float(&n));
                out.delivery_cost_type = Some(DeliveryCostType::Fixed);
                break;
            }
        }
    }
    if out.slabs.is_none()
        && let Some(n) = first_capture(re!(r"\b(\d+)\b"), &lower)
        && out.delivery_cost.is_none_or(|d| d == 0.0 || d.is_nan())
    {
        out.slabs = Some(js_parse_float(&n));
    }
    if out
        .slabs
        .is_some_and(|s| !s.is_finite() || s <= 0.0 || s.fract() != 0.0)
    {
        out.slabs = None;
    }
    if out.delivery_cost.is_some_and(|d| !d.is_finite() || d < 0.0) {
        out.delivery_cost = None;
        out.delivery_cost_type = None;
    }
    out
}

pub fn user_declined_special_order(text: &str) -> bool {
    let lower = text.to_lowercase();
    let lower = trim(&lower);
    matches!(lower, "no" | "n" | "no thanks" | "nope")
        || lower.starts_with("no,")
        || lower.starts_with("no ")
        || lower.contains("don't")
        || lower.contains("do not")
}

fn user_accepted_special_order(text: &str) -> bool {
    let lower = text.to_lowercase();
    let lower = trim(&lower);
    matches!(lower, "yes" | "y" | "yeah" | "sure" | "ok" | "okay")
        || lower.starts_with("yes,")
        || lower.starts_with("yes ")
        || lower.contains("adjust")
        || lower.contains("special order")
}

pub fn strip_ai_special_order_prompt(text: &str) -> String {
    let text = re!(
        r"\n*\s*Would you like (?:me )?to adjust this price for a special order\??",
        "gi"
    )
    .replace_all(text, "");
    let text =
        re!(r"\n*\s*How many slabs do you need.*?delivery cost\??", "gi").replace_all(&text, "");
    let text = re!(
        r"\n*\s*If yes, please provide a delivery cost and the amount of slabs\.?",
        "gi"
    )
    .replace_all(&text, "");
    let text = re!(
        r"\*\*If yes, please provide a delivery cost and the amount of slabs\.\*\*",
        "gi"
    )
    .replace_all(&text, "");
    text.trim_end_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
        .to_string()
}

pub fn append_special_order_prompt(answer: &str) -> String {
    let without = strip_ai_special_order_prompt(answer);
    format!(
        "{}{SPECIAL_ORDER_QUESTION}",
        without.trim_end_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
    )
}

fn is_price_list_follow_up(query: &str) -> bool {
    if user_declined_special_order(query) || user_accepted_special_order(query) {
        return true;
    }
    let parsed = parse_slabs_and_delivery(query);
    if parsed.slabs.is_some() || parsed.delivery_cost.is_some() {
        return true;
    }
    let trimmed = trim(query);
    if re!(r"^\d+(?:\.\d+)?\s*(?:cm|mm)$", "i").is_match(trimmed)
        || re!(r"^use\s+\d+(?:\.\d+)?\s*(?:cm|mm)$", "i").is_match(trimmed)
        || re!(r#"^(\d{2,3})\s*(?:["”])?\s*[×xX]\s*(\d{2,3})\s*(?:["”])?$"#).is_match(trimmed)
        || re!(
            r#"^use\s+(?:slab\s+size\s+)?(\d{2,3})\s*(?:["”])?\s*[×xX]\s*(\d{2,3})\s*(?:["”])?$"#,
            "i"
        )
        .is_match(trimmed)
    {
        return true;
    }
    let lower = trimmed.to_lowercase();
    lower.contains("special order") || lower.contains("delivery") || lower.contains("slab")
}

pub fn should_rebuild_price_list_context(query: &str, offer: Option<&SpecialOrderOffer>) -> bool {
    offer.is_none() && !is_price_list_follow_up(query)
}

/// `buildPriceListNotReadyMessage`.
pub fn price_list_not_ready_message(
    supplier: &str,
    total: i64,
    processing: i64,
    failed: i64,
) -> String {
    let name = trim(supplier);
    if total == 0 {
        return if name.is_empty() {
            "I don't have any price files uploaded yet.".into()
        } else {
            format!("I don't have any price files uploaded for {name} yet.")
        };
    }
    if processing == 0 && failed > 0 {
        let subject = if name.is_empty() {
            "The price files".to_string()
        } else {
            format!("{name}'s price files")
        };
        return format!(
            "{subject} couldn't be processed ({failed} of {total} failed). An admin needs to retry them before I can answer."
        );
    }
    let subject = if name.is_empty() {
        "the price lists".to_string()
    } else {
        format!("{name}'s price lists")
    };
    format!(
        "I'm still preparing {subject} — {processing} of {total} files are still processing. Please try again in a few minutes."
    )
}

// ---------- the answer itself ----------

fn source_marker() -> &'static regex::Regex {
    re!(r"\[+\s*SOURCE\s*[:#-]?\s*([^\]]*?)\s*\]+", "i")
}

fn clarify_marker() -> &'static regex::Regex {
    re!(r"\[\[CLARIFY:([^\]]*)\]\]", "i")
}

/// A clarifying question as saved in the history: `[[CLARIFY:…]]` keeps the
/// question it asks about, for the reply that picks a source.
pub fn with_clarify_marker(text: &str, question: &str) -> String {
    let question = question.replace(['[', ']', '|'], " ");
    format!("{text}\n\n[[CLARIFY:{}]]", trim(&question))
}

/// A question back about which feature is meant, as saved:
/// `[[CLARIFY:question|label=section,section|…]]` also keeps where each
/// choice is answered from.
pub fn with_clarify_choices_marker(
    text: &str,
    question: &str,
    choices: &[(&str, &[String])],
) -> String {
    let question = question.replace(['[', ']', '|'], " ");
    let choices: Vec<String> = choices
        .iter()
        .map(|(label, sections)| {
            format!(
                "{}={}",
                trim(&label.replace(['[', ']', '|', '=', ','], " ")),
                sections.join(",")
            )
        })
        .collect();
    format!(
        "{text}\n\n[[CLARIFY:{}|{}]]",
        trim(&question),
        choices.join("|")
    )
}

/// The question a saved clarifying question asks about.
pub fn parse_clarify_marker(text: &str) -> Option<String> {
    first_capture(clarify_marker(), text)
        .map(|marker| trim(marker.split('|').next().unwrap_or_default()).to_string())
        .filter(|question| !question.is_empty())
}

/// The choices a saved question back offered (`label`, its sections); none
/// for the question between the app and the company's process.
pub fn parse_clarify_choices(text: &str) -> Vec<(String, Vec<String>)> {
    let Some(marker) = first_capture(clarify_marker(), text) else {
        return Vec::new();
    };
    marker
        .split('|')
        .skip(1)
        .filter_map(|choice| {
            let (label, sections) = choice.split_once('=')?;
            let sections: Vec<String> = sections
                .split(',')
                .map(|id| trim(id).to_string())
                .filter(|id| !id.is_empty())
                .collect();
            let label = trim(label);
            (!label.is_empty() && !sections.is_empty()).then(|| (label.to_string(), sections))
        })
        .collect()
}

pub fn strip_clarify_marker(text: &str) -> String {
    trim(&clarify_marker().replace_all(text, "")).to_string()
}

/// `stripChatResponseMarkersTrimmed`: the answer as the user reads it.
pub fn strip_markers(text: &str) -> String {
    let text = re!(r"\[+\s*SOURCE\s*[:#-]?\s*[^\]]*?\s*\]+", "gi").replace_all(text, "");
    let text = re!(r"\[\[INSTRUCTION:[^\]]*\]\]", "gi").replace_all(&text, "");
    let text = strip_clarify_marker(&text);
    let text = strip_special_order_marker(&text);
    let text = strip_thickness_options_marker(&text);
    let text = strip_slab_size_options_marker(&text);
    let text = strip_price_list_options_marker(&text);
    let text = re!(
        r"\*\*If yes, please provide a delivery cost and the amount of slabs\.\*\*",
        "gi"
    )
    .replace_all(&text, "");
    let text = collapse_before_adjust(&text);
    trim(&text).to_string()
}

/// `/\n{3,}(?=Would you like (?:me )?to adjust)/i` → `\n\n` (first only).
fn collapse_before_adjust(text: &str) -> String {
    let re = fre!(r"\n{3,}(?=Would you like (?:me )?to adjust)", "i");
    match re.find(text) {
        Ok(Some(m)) => format!("{}\n\n{}", &text[..m.start()], &text[m.end()..]),
        _ => text.to_string(),
    }
}

/// `parseInstructionIndices`: `None` without a marker, `Some(None)` for
/// `[[INSTRUCTION:none]]`, else the 1-based indices.
pub fn parse_instruction_indices(answer: &str) -> Option<Option<Vec<usize>>> {
    let raw = first_capture(re!(r"\[\[INSTRUCTION:\s*([^\]]+)\]\]", "i"), answer)?;
    let raw = trim(&raw);
    if raw.eq_ignore_ascii_case("none") {
        return Some(None);
    }
    let mut indices = Vec::new();
    for part in raw.split(',') {
        let n = js_parse_int(part);
        if n.is_finite() && n > 0.0 {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let n = n as usize;
            if !indices.contains(&n) {
                indices.push(n);
            }
        }
    }
    if indices.is_empty() {
        None
    } else {
        Some(Some(indices))
    }
}

/// `parseSourceIds`.
pub fn parse_source_ids(answer: &str) -> Vec<i64> {
    let Some(raw) = first_capture(source_marker(), answer) else {
        return Vec::new();
    };
    let raw = trim(&raw);
    if raw.eq_ignore_ascii_case("none") {
        return Vec::new();
    }
    let mut ids = Vec::new();
    for part in raw.split(',') {
        let n = js_parse_int(part);
        #[allow(clippy::cast_possible_truncation)]
        if n.is_finite() && n > 0.0 && !ids.contains(&(n as i64)) {
            ids.push(n as i64);
        }
    }
    ids
}

const PRICE_LIST_SOURCE_QUERY_STOP: [&str; 7] =
    ["price", "cost", "from", "for", "the", "and", "list"];

/// The text of `SOURCE n:` up to the next source.
fn source_block(context: &str, id: i64) -> String {
    let re =
        crate::followups::js::js_fancy(&format!(r"SOURCE {id}:[\s\S]*?(?=\nSOURCE \d+:|$)"), "i");
    re.find(context)
        .ok()
        .flatten()
        .map(|m| m.as_str().to_string())
        .unwrap_or_default()
}

/// `selectPriceListSourcesWhereColorFound`: of the cited sources, those
/// whose block names the color asked about (all of them when none does).
pub fn sources_where_color_found<'a, T>(
    cited_ids: &[i64],
    sources: &'a [T],
    id_of: impl Fn(&T) -> i64,
    supplier_of: impl Fn(&T) -> &str,
    context: &str,
    query: &str,
) -> Vec<&'a T> {
    let mut cited: Vec<&T> = Vec::new();
    for id in cited_ids {
        if let Some(source) = sources.iter().find(|s| id_of(s) == *id)
            && !cited.iter().any(|c| id_of(c) == id_of(source))
        {
            cited.push(source);
        }
    }
    if cited.len() <= 1 {
        return cited;
    }
    let split = |text: &str| -> Vec<String> {
        re!(r"\W+")
            .split(&text.to_lowercase())
            .map(str::to_string)
            .collect()
    };
    let supplier_words: std::collections::HashSet<String> = cited
        .iter()
        .flat_map(|s| split(supplier_of(s)))
        .filter(|w| js_len_ge(w, 2))
        .collect();
    let terms: Vec<String> = split(query)
        .into_iter()
        .filter(|t| {
            js_len_ge(t, 3)
                && !PRICE_LIST_SOURCE_QUERY_STOP.contains(&t.as_str())
                && !supplier_words.contains(t)
        })
        .collect();
    if terms.is_empty() {
        return cited;
    }
    let matching: Vec<&T> = cited
        .iter()
        .copied()
        .filter(|s| {
            let block = source_block(context, id_of(s)).to_lowercase();
            terms.iter().all(|t| block.contains(t.as_str()))
        })
        .collect();
    if matching.is_empty() { cited } else { matching }
}

fn js_len_ge(s: &str, n: usize) -> bool {
    s.encode_utf16().count() >= n
}

const NO_INFO_PATTERNS: [&str; 23] = [
    "do not have",
    "don't have",
    "does not have",
    "do not have specific",
    "don't have specific",
    "no specific",
    "not in the provided",
    "not in the supplier",
    "could not find",
    "couldn't find",
    "cannot find",
    "can't find",
    "no instruction for",
    "no pricing information",
    "not found in the supplier",
    "unable to find",
    "did not take any",
    "didn't take any",
    "did not use any",
    "didn't use any",
    "no information was taken",
    "i did not take",
    "i did not use",
];

/// `answerHasUsableInfo`: the answer says something (not "I could not find").
pub fn answer_has_usable_info(answer: &str) -> bool {
    let lower = answer.to_lowercase();
    let lower = trim(&lower);
    if lower.is_empty() {
        return false;
    }
    if lower.contains("price is not specified")
        || lower.contains("price is not listed")
        || lower.contains("level is")
        || lower.contains("group is")
        || (lower.contains("size is") && lower.contains("not specified"))
    {
        return true;
    }
    let dollar = re!(r"\$\s*[\d,]+(?:\.\d{2})?").is_match(answer);
    let close = lower.contains("no exact match")
        || lower.contains("not an exact match")
        || lower.contains("not a perfect match")
        || lower.contains("closest match")
        || (dollar && lower.contains(" but "));
    if close && dollar {
        return true;
    }
    !NO_INFO_PATTERNS.iter().any(|p| lower.contains(p))
}

const INCOMPLETE_INSTRUCTION_PATTERNS: [&str; 11] = [
    "no additional",
    "not provided",
    "no further",
    "cannot confirm",
    "can't confirm",
    "does not specify",
    "doesn't specify",
    "only mentioned",
    "only mentions",
    "no more detail",
    "not specified in",
];

/// `instructionAnswerNeedsAnotherLook`.
pub fn instruction_answer_needs_another_look(answer: &str) -> bool {
    let clean = strip_markers(answer);
    if !answer_has_usable_info(&clean) {
        return true;
    }
    let lower = clean.to_lowercase();
    INCOMPLETE_INSTRUCTION_PATTERNS
        .iter()
        .any(|p| lower.contains(p))
}
