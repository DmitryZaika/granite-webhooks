//! What reps earn on signed quotes, and when it is paid.
//!
//! Port of the CRM's `utils/commission/engine.ts` and `payroll.ts`: rules
//! an admin builds (Admin > Quotes > Commissions) applied to the option the
//! customer took, the customer's `QuickBooks` invoice that says whether it
//! was paid, and the paychecks (commissions, monthly goal bonuses, the
//! weekly base) per payroll date.

use crate::quote_pricing::doc::QuoteDoc;
use crate::quote_pricing::js::{arr, num};
use crate::quote_pricing::pricing::{OptionPrice, PriceLine};
use crate::quote_pricing::takeoff::js_round;
use crate::routes::quickbooks::invoice::InvoiceSummary;
use chrono::{Datelike, NaiveDate};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::LazyLock;
use utoipa::ToSchema;

/// A commission rule (`CommissionRule`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CommissionRule {
    /// Rule id.
    pub id: i64,
    /// Name.
    pub name: String,
    /// `material` (the stone per area), `quote` (the whole quote before
    /// tax), `lines` (price lines matching `lineMatch`) or `monthly_goal`.
    pub applies: String,
    /// `percent` or `flat` (dollars per sq ft, unit, quote or goal).
    pub rate_type: String,
    /// Percent or dollars.
    pub rate: f64,
    /// Higher goes first.
    pub priority: i64,
    /// Material rules: a match keeps lower rules from paying on the same area.
    pub stop: bool,
    /// Material rules: only these materials (empty: any).
    pub materials: Vec<String>,
    /// Material rules: only these colors (empty: any).
    pub colors: Vec<String>,
    /// Material rules: lowest stone level.
    pub level_min: Option<f64>,
    /// Material rules: highest stone level.
    pub level_max: Option<f64>,
    /// Line rules: text the line's key or description contains.
    pub line_match: String,
    /// Monthly goals: the month's paid sales that earn the bonus.
    pub threshold: Option<f64>,
    /// Off rules pay nothing.
    pub active: bool,
}

/// One rule's share of a quote's commission.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CommissionPart {
    /// The rule.
    pub rule_id: i64,
    /// Its name.
    pub rule: String,
    /// What it was applied to: dollars, sq ft or units.
    pub base: f64,
    /// What `base` counts.
    pub base_label: String,
    /// Earned.
    pub amount: f64,
    /// Where it applied, e.g. the area or the line.
    pub note: String,
}

/// What a quote earns.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QuoteCommission {
    pub total: f64,
    pub parts: Vec<CommissionPart>,
}

fn lower(s: &str) -> String {
    s.trim().to_lowercase()
}

/// `money`: cents (as JavaScript rounds them, so no fused multiply-add).
#[allow(clippy::suboptimal_flops)]
pub fn money(v: f64) -> f64 {
    js_round(v * 100.0 + 1e-9) / 100.0
}

/// Stone sold by sq ft: the slab, splash, waterfall and mitered-edge material lines.
static MATERIAL_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\|(?:material|splash:|waterfall:|mitered edge material:)").expect("regex")
});
static LEADING_NUMBER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*([\d,]*\.?\d+)").expect("regex"));

fn is_material_line(line: &PriceLine) -> bool {
    MATERIAL_KEY.is_match(&line.key) && line.unit.is_some()
}

/// Material lines are priced by sq ft, so their quantity is the number the
/// line starts with.
fn line_quantity(line: &PriceLine) -> f64 {
    let parsed = LEADING_NUMBER
        .captures(&line.description)
        .and_then(|c| c[1].replace(',', "").parse::<f64>().ok());
    if let Some(quantity) = parsed.filter(|q| q.is_finite() && *q > 0.0) {
        return quantity;
    }
    match line.unit_price {
        Some(price) if price > 0.0 => line.total / price,
        _ => 1.0,
    }
}

fn level_of(price_list: &Value, product: &str, color: &str) -> Option<f64> {
    let product = arr(Some(price_list), "arrProducts").iter().find(|p| {
        lower(p.get("name").and_then(Value::as_str).unwrap_or_default()) == lower(product)
    })?;
    let variant = arr(Some(product), "arrVariants")
        .iter()
        .find(|v| lower(v.get("name").and_then(Value::as_str).unwrap_or_default()) == lower(color));
    num(variant, "level")
}

fn material_matches(rule: &CommissionRule, product: &str, color: &str, level: Option<f64>) -> bool {
    if !rule.materials.is_empty() && !rule.materials.iter().any(|m| lower(m) == lower(product)) {
        return false;
    }
    if !rule.colors.is_empty() && !rule.colors.iter().any(|c| lower(c) == lower(color)) {
        return false;
    }
    if rule
        .level_min
        .is_some_and(|min| level.is_none_or(|l| l < min))
    {
        return false;
    }
    if rule
        .level_max
        .is_some_and(|max| level.is_none_or(|l| l > max))
    {
        return false;
    }
    true
}

fn percent(rule: &CommissionRule) -> bool {
    rule.rate_type == "percent"
}

fn pay(rule: &CommissionRule, base: f64) -> f64 {
    money(if percent(rule) {
        base * rule.rate / 100.0
    } else {
        base * rule.rate
    })
}

/// `quoteCommission`: what a rep earns on one option (the one the customer
/// took): every active rule applied to the quote's areas and lines. Monthly
/// goals are paid by [`payroll_weeks`].
#[allow(clippy::too_many_lines)]
pub fn quote_commission(
    doc: &QuoteDoc,
    price_list: &Value,
    rules: &[CommissionRule],
    option_index: usize,
    prices: &[OptionPrice],
) -> QuoteCommission {
    let (Some(option), Some(price)) = (
        doc.color_options.get(option_index),
        prices.get(option_index),
    ) else {
        return QuoteCommission {
            total: 0.0,
            parts: Vec::new(),
        };
    };
    let mut ordered: Vec<&CommissionRule> = rules
        .iter()
        .filter(|r| r.active && r.applies != "monthly_goal")
        .collect();
    ordered.sort_by(|a, b| b.priority.cmp(&a.priority).then(a.id.cmp(&b.id)));
    let mut parts = Vec::new();

    for area in &price.areas {
        let picked = area.area_id.as_ref().and_then(|id| option.by_area.get(id));
        let product = picked.map_or("", |p| p.product.trim());
        let color = picked.map_or("", |p| p.color.trim());
        let level = level_of(price_list, product, color);
        let material: Vec<&PriceLine> = area.lines.iter().filter(|l| is_material_line(l)).collect();
        let material_dollars = material.iter().fold(0.0, |s, l| s + l.total) * area.quantity;
        let sq_ft = material.iter().fold(0.0, |s, l| s + line_quantity(l)) * area.quantity;
        let mut stopped = false;
        for rule in &ordered {
            if rule.applies == "material" {
                if stopped || material.is_empty() || !material_matches(rule, product, color, level)
                {
                    continue;
                }
                let base = if percent(rule) {
                    material_dollars
                } else {
                    sq_ft
                };
                let what: Vec<&str> = [product, color]
                    .into_iter()
                    .filter(|s| !s.is_empty())
                    .collect();
                parts.push(CommissionPart {
                    rule_id: rule.id,
                    rule: rule.name.clone(),
                    base: money(base),
                    base_label: if percent(rule) {
                        "$ of material"
                    } else {
                        "sq ft"
                    }
                    .into(),
                    amount: pay(rule, base),
                    note: format!("{}: {}", area.name, what.join(" "))
                        .trim()
                        .to_string(),
                });
                if rule.stop {
                    stopped = true;
                }
            } else if rule.applies == "lines" && !rule.line_match.is_empty() {
                let needle = lower(&rule.line_match);
                for line in &area.lines {
                    if !lower(&format!("{} {}", line.key, line.description)).contains(&needle) {
                        continue;
                    }
                    let base = if percent(rule) {
                        line.total * area.quantity
                    } else {
                        line_quantity(line) * area.quantity
                    };
                    parts.push(CommissionPart {
                        rule_id: rule.id,
                        rule: rule.name.clone(),
                        base: money(base),
                        base_label: if percent(rule) { "$" } else { "units" }.into(),
                        amount: pay(rule, base),
                        note: format!("{}: {}", area.name, line.description),
                    });
                }
            }
        }
    }

    // Lines on the quote itself, outside any area ("Other Items").
    for rule in &ordered {
        if rule.applies == "lines" && !rule.line_match.is_empty() {
            let needle = lower(&rule.line_match);
            for line in &price.other.lines {
                if !lower(&format!("{} {}", line.key, line.description)).contains(&needle) {
                    continue;
                }
                let base = if percent(rule) {
                    line.total
                } else {
                    line_quantity(line)
                };
                parts.push(CommissionPart {
                    rule_id: rule.id,
                    rule: rule.name.clone(),
                    base: money(base),
                    base_label: if percent(rule) { "$" } else { "units" }.into(),
                    amount: pay(rule, base),
                    note: line.description.clone(),
                });
            }
        }
        if rule.applies == "quote" {
            let base = price.total - price.tax;
            parts.push(CommissionPart {
                rule_id: rule.id,
                rule: rule.name.clone(),
                base: money(base),
                base_label: if percent(rule) { "$ of quote" } else { "quote" }.into(),
                amount: money(if percent(rule) {
                    base * rule.rate / 100.0
                } else {
                    rule.rate
                }),
                note: "Whole quote".into(),
            });
        }
    }
    QuoteCommission {
        total: money(parts.iter().fold(0.0, |s, p| s + p.amount)),
        parts,
    }
}

/// `monthlyBonuses`: each monthly goal reached by the month's paid sales.
pub fn monthly_bonus(rules: &[CommissionRule], paid_sales: f64) -> f64 {
    rules
        .iter()
        .filter(|r| {
            r.active && r.applies == "monthly_goal" && r.threshold.is_some_and(|t| paid_sales >= t)
        })
        .fold(0.0, |s, r| s + money(r.rate))
}

/// `payrollDateFor`: the payroll `weekday` (0 = Sunday) on or after a date.
pub fn payroll_date_for(paid_date: &str, weekday: u32) -> Option<String> {
    let date = NaiveDate::parse_from_str(paid_date.get(..10)?, "%Y-%m-%d").ok()?;
    let today = date.weekday().num_days_from_sunday();
    let ahead = (weekday + 7 - today) % 7;
    Some(
        (date + chrono::Duration::days(i64::from(ahead)))
            .format("%Y-%m-%d")
            .to_string(),
    )
}

/// `pickInvoice`: the invoice whose total matches the quote (within $1),
/// else the earliest one issued on or after signing.
pub fn pick_invoice<'a>(
    invoices: &'a [InvoiceSummary],
    quote_totals: &[f64],
    signed_at: Option<&str>,
) -> Option<&'a InvoiceSummary> {
    if let Some(exact) = invoices.iter().find(|inv| {
        quote_totals
            .iter()
            .any(|total| (inv.total_amt.unwrap_or(0.0) - total).abs() <= 1.0)
    }) {
        return Some(exact);
    }
    let signed = signed_at.and_then(|s| s.get(..10));
    let mut after: Vec<&InvoiceSummary> = invoices
        .iter()
        .filter(|inv| {
            inv.txn_date
                .as_deref()
                .is_some_and(|d| !d.is_empty() && signed.is_none_or(|s| d >= s))
        })
        .collect();
    after.sort_by(|a, b| a.txn_date.cmp(&b.txn_date));
    after.first().copied()
}

/// `statusOf`: `paid`, `partial`, `unpaid` or `no_invoice`.
pub fn status_of(invoice: Option<&InvoiceSummary>) -> &'static str {
    let Some(total) = invoice.and_then(|i| i.total_amt) else {
        return "no_invoice";
    };
    let balance = invoice.and_then(|i| i.balance).unwrap_or(total);
    if balance <= 0.005 {
        "paid"
    } else if balance < total - 0.005 {
        "partial"
    } else {
        "unpaid"
    }
}

/// A commission row as payroll reads it.
#[derive(Debug, Clone)]
pub struct PaidRow {
    pub user_id: Option<i32>,
    pub rep: String,
    pub status: String,
    pub quote_total: f64,
    pub commission: f64,
    pub paid_date: Option<String>,
    pub payroll_date: Option<String>,
}

/// One rep's paycheck on a payroll date.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PayrollRep {
    /// The rep.
    pub user_id: Option<i32>,
    /// Their name.
    pub rep: String,
    /// Paid commissions.
    pub commission: f64,
    /// Monthly goal bonuses.
    pub bonus: f64,
    /// The weekly base.
    pub base: f64,
    /// All of it.
    pub total: f64,
}

/// A payroll date's paychecks.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PayrollWeek {
    /// The payroll date (YYYY-MM-DD).
    pub payroll_date: String,
    /// Per rep.
    pub reps: Vec<PayrollRep>,
}

/// `payrollWeeks`: per payroll date (newest first), every rep's paid
/// commissions, monthly bonuses (paid with the first payroll after the
/// month) and the weekly base.
pub fn payroll_weeks(
    rows: &[PaidRow],
    rules: &[CommissionRule],
    base_weekly: f64,
    weekday: u32,
) -> Vec<PayrollWeek> {
    // date → reps in the order first seen.
    let mut weeks: BTreeMap<String, Vec<PayrollRep>> = BTreeMap::new();
    let cell = |weeks: &mut BTreeMap<String, Vec<PayrollRep>>,
                date: &str,
                user_id: Option<i32>,
                rep: &str|
     -> usize {
        let reps = weeks.entry(date.to_string()).or_default();
        if let Some(i) = reps.iter().position(|r| r.user_id == user_id) {
            return i;
        }
        reps.push(PayrollRep {
            user_id,
            rep: rep.to_string(),
            commission: 0.0,
            bonus: 0.0,
            base: base_weekly,
            total: 0.0,
        });
        reps.len() - 1
    };
    // (user, month) → sales, in the order first seen.
    let mut months: Vec<(Option<i32>, String, String, f64)> = Vec::new();
    for row in rows {
        let (Some(payroll), Some(paid)) = (&row.payroll_date, &row.paid_date) else {
            continue;
        };
        if row.status != "paid" {
            continue;
        }
        let i = cell(&mut weeks, payroll, row.user_id, &row.rep);
        weeks.get_mut(payroll.as_str()).expect("cell")[i].commission += row.commission;
        let month = paid.get(..7).unwrap_or_default().to_string();
        match months
            .iter_mut()
            .find(|m| m.0 == row.user_id && m.2 == month)
        {
            Some(entry) => entry.3 += row.quote_total,
            None => months.push((row.user_id, row.rep.clone(), month, row.quote_total)),
        }
    }
    for (user_id, rep, month, sales) in months {
        let bonus = monthly_bonus(rules, sales);
        if bonus == 0.0 {
            continue;
        }
        let next = month
            .split_once('-')
            .and_then(|(y, m)| Some((y.parse::<i32>().ok()?, m.parse::<u32>().ok()?)))
            .and_then(|(y, m)| {
                if m == 12 {
                    NaiveDate::from_ymd_opt(y + 1, 1, 1)
                } else {
                    NaiveDate::from_ymd_opt(y, m + 1, 1)
                }
            });
        let Some(date) =
            next.and_then(|d| payroll_date_for(&d.format("%Y-%m-%d").to_string(), weekday))
        else {
            continue;
        };
        let i = cell(&mut weeks, &date, user_id, &rep);
        weeks.get_mut(date.as_str()).expect("cell")[i].bonus += bonus;
    }
    weeks
        .into_iter()
        .rev()
        .map(|(payroll_date, reps)| PayrollWeek {
            payroll_date,
            reps: reps
                .into_iter()
                .map(|r| PayrollRep {
                    total: js_round((r.commission + r.bonus + r.base) * 100.0) / 100.0,
                    ..r
                })
                .collect(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invoice(id: &str, txn: &str, total: f64, balance: f64) -> InvoiceSummary {
        serde_json::from_value(serde_json::json!({
            "id": id, "doc_number": null, "txn_date": txn, "due_date": null, "total_amt": total,
            "balance": balance, "currency": null, "project_address": null, "last_updated": null,
        }))
        .unwrap()
    }

    #[test]
    fn payroll_lands_on_the_next_payroll_weekday() {
        // 2026-10-13 is a Tuesday; Friday is 5.
        assert_eq!(payroll_date_for("2026-10-13", 5).unwrap(), "2026-10-16");
        assert_eq!(
            payroll_date_for("2026-10-16T09:00:00Z", 5).unwrap(),
            "2026-10-16"
        );
    }

    #[test]
    fn invoices_match_by_total_then_date() {
        let invoices = vec![
            invoice("1", "2026-01-01", 100.0, 0.0),
            invoice("2", "2026-03-01", 900.0, 900.0),
            invoice("3", "2026-02-01", 400.0, 100.0),
        ];
        assert_eq!(pick_invoice(&invoices, &[899.5], None).unwrap().id, "2");
        assert_eq!(
            pick_invoice(&invoices, &[5.0], Some("2026-01-15"))
                .unwrap()
                .id,
            "3"
        );
        assert_eq!(status_of(Some(&invoices[0])), "paid");
        assert_eq!(status_of(Some(&invoices[1])), "unpaid");
        assert_eq!(status_of(Some(&invoices[2])), "partial");
        assert_eq!(status_of(None), "no_invoice");
    }

    #[test]
    fn paychecks_add_commissions_bonuses_and_the_base() {
        let goal = CommissionRule {
            id: 1,
            name: "Goal".into(),
            applies: "monthly_goal".into(),
            rate_type: "flat".into(),
            rate: 250.0,
            priority: 0,
            stop: false,
            materials: vec![],
            colors: vec![],
            level_min: None,
            level_max: None,
            line_match: String::new(),
            threshold: Some(1000.0),
            active: true,
        };
        let row = |paid: &str, payroll: &str, total: f64| PaidRow {
            user_id: Some(100),
            rep: "Rita".into(),
            status: "paid".into(),
            quote_total: total,
            commission: 50.0,
            paid_date: Some(paid.into()),
            payroll_date: Some(payroll.into()),
        };
        let weeks = payroll_weeks(
            &[
                row("2026-09-10", "2026-09-11", 600.0),
                row("2026-09-20", "2026-09-25", 600.0),
            ],
            &[goal],
            100.0,
            5,
        );
        let dates: Vec<&str> = weeks.iter().map(|w| w.payroll_date.as_str()).collect();
        assert_eq!(dates, ["2026-10-02", "2026-09-25", "2026-09-11"]);
        assert_eq!(weeks[0].reps[0].bonus, 250.0);
        assert_eq!(weeks[0].reps[0].total, 350.0);
        assert_eq!(weeks[1].reps[0].total, 150.0);
    }
}
