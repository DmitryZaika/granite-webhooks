//! What reps earn on signed quotes, and when it is paid.
//!
//! Port of the CRM's `utils/commission/engine.ts` and `payroll.ts`: rules
//! an admin builds (Admin > Quotes > Commissions) applied to the option the
//! customer took, the customer's `QuickBooks` invoice that says whether it
//! was paid, and the paychecks (commissions, monthly goal bonuses, the
//! reps' base pay on its own schedule) per payroll date.

use crate::openapi::ToSchema;
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

/// A commission rule (`CommissionRule`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CommissionRule {
    /// Rule id.
    pub id: i64,
    /// Name.
    pub name: String,
    /// `material` (the stone of each room: a percent of everything in the
    /// room, or dollars per sq ft of stone), `quote` (the whole quote before
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
        // A stone rule's percent is of everything in the room: the room's
        // stone sets the rate for all of its items.
        let room_dollars = area.lines.iter().fold(0.0, |s, l| s + l.total) * area.quantity;
        let sq_ft = material.iter().fold(0.0, |s, l| s + line_quantity(l)) * area.quantity;
        let mut stopped = false;
        for rule in &ordered {
            if rule.applies == "material" {
                if stopped || material.is_empty() || !material_matches(rule, product, color, level)
                {
                    continue;
                }
                let base = if percent(rule) { room_dollars } else { sq_ft };
                let what: Vec<&str> = [product, color]
                    .into_iter()
                    .filter(|s| !s.is_empty())
                    .collect();
                parts.push(CommissionPart {
                    rule_id: rule.id,
                    rule: rule.name.clone(),
                    base: money(base),
                    base_label: if percent(rule) { "$ of room" } else { "sq ft" }.into(),
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

/// Which extra a price line sells (the Statistics page's Feature sales).
///
/// The quote's own waterfall, miter and edge lines by their key, items from
/// the price list or typed on the quote by their text ("Seam Phantom",
/// "Support Corbel", "Faucet", "Karran Soap Dispenser"). A custom edge is an
/// edge profile that costs money (Ogee, Dupont, …); faucet holes are not faucets.
pub fn feature_of(line: &PriceLine) -> Option<&'static str> {
    let key = line.key.to_lowercase();
    let text = line.description.to_lowercase();
    // Before the text checks: "Triple Waterfall" is an edge profile.
    if key.contains("|edge:") {
        return (line.total > 0.0).then_some("custom_edge");
    }
    let feature = if key.contains("|waterfall") || text.contains("waterfall") {
        "waterfall"
    } else if key.contains("|mitered edge material:")
        || key.contains("|miter")
        || text.contains("miter")
    {
        "miter"
    } else if text.contains("phantom") {
        "phantom_seam"
    } else if text.contains("support") || text.contains("corbel") {
        "support"
    } else if text.contains("soap") {
        "soap_dispenser"
    } else if text.contains("faucet") && !text.contains("hole") && !key.contains("faucethole") {
        "faucet"
    } else {
        return None;
    };
    Some(feature)
}

/// Dollars of each extra ([`feature_of`]) sold on one option.
///
/// Areas count times their quantity; extras not on the quote are left out.
#[allow(clippy::suboptimal_flops)]
pub fn feature_sales(price: &OptionPrice) -> BTreeMap<String, f64> {
    let mut sold: BTreeMap<String, f64> = BTreeMap::new();
    let areas = price
        .areas
        .iter()
        .map(|a| (a, a.quantity))
        .chain(std::iter::once((&price.other, 1.0)));
    for (area, quantity) in areas {
        for line in &area.lines {
            if let Some(feature) = feature_of(line) {
                *sold.entry(feature.into()).or_default() += line.total * quantity;
            }
        }
    }
    sold.into_iter()
        .map(|(feature, dollars)| (feature, money(dollars)))
        .filter(|(_, dollars)| dollars.abs() >= 0.005)
        .collect()
}

/// `monthlyBonuses`: each monthly goal reached by what was sold in the month;
/// they add up ($300 at 60K and $500 at 80K pay $800 at 80K).
pub fn monthly_bonus(rules: &[CommissionRule], sales: f64) -> f64 {
    rules
        .iter()
        .filter(|r| {
            r.active && r.applies == "monthly_goal" && r.threshold.is_some_and(|t| sales >= t)
        })
        .fold(0.0, |s, r| s + money(r.rate))
}

/// The payroll a job is paid with once an admin adds it to the salary: the
/// payroll `weekday` of the week after the one it was paid and installed in
/// (weeks start on Monday), or the next payroll from `today` when that one
/// has gone by.
pub fn next_week_payroll(done: NaiveDate, today: NaiveDate, weekday: u32) -> NaiveDate {
    let monday = done - chrono::Duration::days(i64::from(done.weekday().num_days_from_monday()));
    let payday = monday + chrono::Duration::days(7 + i64::from((weekday + 6) % 7));
    if payday >= today {
        return payday;
    }
    let ahead = (weekday + 7 - today.weekday().num_days_from_sunday()) % 7;
    today + chrono::Duration::days(i64::from(ahead))
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
    pub quote_total: f64,
    pub commission: f64,
    pub paid_date: Option<String>,
    pub payroll_date: Option<String>,
    /// The day it was sold (YYYY-MM-DD).
    pub signed_at: Option<String>,
    /// How it counts toward a monthly goal.
    pub goal: GoalRow,
}

/// How a commission row counts toward a monthly goal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalRow {
    /// A sale, by the day sold: a CRM quote or a job on the rep's sheet
    /// (cabinets too).
    Sale,
    /// A bonus the sheet already paid: the month before its payday gets no
    /// other bonus.
    PaidBonus,
    /// Neither: a sheet row paid in full that is no sale (e.g. a fee).
    Other,
}

impl GoalRow {
    /// A CRM quote is a sale; a sheet row named `Bonus …` is a paid bonus; a
    /// sheet row paying its whole total is no sale; any other sheet row is.
    pub fn of(from_quote: bool, label: Option<&str>, total: f64, commission: f64) -> Self {
        if from_quote {
            return Self::Sale;
        }
        if label.is_some_and(|l| l.trim().to_lowercase().starts_with("bonus")) {
            Self::PaidBonus
        } else if total > 0.0 && commission >= total - 0.005 {
            Self::Other
        } else {
            Self::Sale
        }
    }
}

/// `YYYY-MM` of the month before the one of `date` (`YYYY-MM-DD`).
fn month_before(date: &str) -> Option<String> {
    let day = NaiveDate::parse_from_str(date.get(..10)?, "%Y-%m-%d").ok()?;
    let last = day.with_day(1)?.pred_opt()?;
    Some(last.format("%Y-%m").to_string())
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
    /// Base pay due that day.
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

/// A rep's base pay (`commission_base_pay`): `amount` every week, two weeks
/// or month from the first payday, until an optional last day. It is one
/// person's (`user_id`) or everyone's with a position (`position_id`); a
/// person's own base pay replaces their position's.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BasePay {
    /// Base pay id.
    pub id: i64,
    /// One person's base pay.
    pub user_id: Option<i32>,
    /// Their name.
    pub rep: Option<String>,
    /// Or the base pay of everyone with this position.
    pub position_id: Option<i32>,
    /// Paid each time.
    pub amount: f64,
    /// `weekly`, `biweekly` (every two weeks) or `monthly`.
    pub frequency: String,
    /// The first payday (YYYY-MM-DD); monthly pays on its day of the month.
    pub start_date: String,
    /// The last day it is paid (YYYY-MM-DD); none: no end.
    pub end_date: Option<String>,
}

/// The days a base pay is paid: from the first payday every week, two weeks
/// or month (monthly: on that day of the month, the month's last day when
/// shorter), until the last day, and no further than the first payday after
/// `today`.
pub fn base_pay_dates(
    start: NaiveDate,
    frequency: &str,
    end: Option<NaiveDate>,
    today: NaiveDate,
) -> Vec<NaiveDate> {
    let mut dates = Vec::new();
    // Ten years of weeks at most.
    for i in 0..=520_u32 {
        let date = match frequency {
            "weekly" => start + chrono::Duration::days(7 * i64::from(i)),
            "biweekly" => start + chrono::Duration::days(14 * i64::from(i)),
            "monthly" => {
                let months = start.year() * 12 + i32::try_from(start.month0() + i).unwrap_or(0);
                let (year, month) = (months / 12, u32::try_from(months % 12).unwrap_or(0) + 1);
                let Some(date) = (0..4).find_map(|back| {
                    NaiveDate::from_ymd_opt(year, month, start.day().checked_sub(back)?)
                }) else {
                    break;
                };
                date
            }
            _ => break,
        };
        if end.is_some_and(|end| date > end) {
            break;
        }
        dates.push(date);
        if date > today {
            break;
        }
    }
    dates
}

/// Someone who holds a position in the company.
#[derive(Debug, Clone)]
pub struct PositionMember {
    pub user_id: i32,
    pub rep: String,
    pub position_id: i32,
}

/// Each person's base pay: their own, or else every base pay of each
/// position they hold, as that person's.
pub fn base_pay_by_person(base_pay: &[BasePay], members: &[PositionMember]) -> Vec<BasePay> {
    let own: Vec<BasePay> = base_pay
        .iter()
        .filter(|p| p.user_id.is_some())
        .cloned()
        .collect();
    let mut people = own.clone();
    for member in members {
        if own.iter().any(|p| p.user_id == Some(member.user_id)) {
            continue;
        }
        for pay in base_pay
            .iter()
            .filter(|p| p.user_id.is_none() && p.position_id == Some(member.position_id))
        {
            people.push(BasePay {
                user_id: Some(member.user_id),
                rep: Some(member.rep.clone()),
                ..pay.clone()
            });
        }
    }
    people
}

/// `payrollWeeks`: per payroll date (newest first), every rep's commissions
/// an admin added to the salary, monthly bonuses and their base pay on its
/// own days (through the next payday after `today`). A month's bonus goes by
/// what the rep sold that month (CRM quotes and sheet jobs, by the day sold;
/// not a month the sheet already paid a bonus for) and is
/// paid with the first payroll after the month. `base_pay` is per person
/// (see [`base_pay_by_person`]).
pub fn payroll_weeks(
    rows: &[PaidRow],
    rules: &[CommissionRule],
    base_pay: &[BasePay],
    weekday: u32,
    today: NaiveDate,
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
            base: 0.0,
            total: 0.0,
        });
        reps.len() - 1
    };
    // (user, month sold) → sales, in the order first seen.
    let mut months: Vec<(Option<i32>, String, String, f64)> = Vec::new();
    // (user, month) whose bonus the sheet already paid.
    let paid_bonus: Vec<(Option<i32>, String)> = rows
        .iter()
        .filter(|row| row.goal == GoalRow::PaidBonus)
        .filter_map(|row| {
            let day = row.payroll_date.as_deref().or(row.signed_at.as_deref())?;
            Some((row.user_id, month_before(day)?))
        })
        .collect();
    for row in rows {
        if row.goal == GoalRow::Sale
            && let Some(month) = row.signed_at.as_deref().and_then(|d| d.get(..7))
        {
            match months
                .iter_mut()
                .find(|m| m.0 == row.user_id && m.2 == month)
            {
                Some(entry) => entry.3 += row.quote_total,
                None => months.push((
                    row.user_id,
                    row.rep.clone(),
                    month.to_string(),
                    row.quote_total,
                )),
            }
        }
        // Only what an admin added to the salary is paid.
        let Some(payroll) = &row.payroll_date else {
            continue;
        };
        let i = cell(&mut weeks, payroll, row.user_id, &row.rep);
        weeks.get_mut(payroll.as_str()).expect("cell")[i].commission += row.commission;
    }
    for (user_id, rep, month, sales) in months {
        if paid_bonus.iter().any(|(u, m)| *u == user_id && *m == month) {
            continue;
        }
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
    for pay in base_pay {
        let ymd = |s: &str| NaiveDate::parse_from_str(s.get(..10)?, "%Y-%m-%d").ok();
        let (Some(user_id), Some(start)) = (pay.user_id, ymd(&pay.start_date)) else {
            continue;
        };
        let rep = pay.rep.as_deref().unwrap_or("Unassigned");
        let end = pay.end_date.as_deref().and_then(ymd);
        for date in base_pay_dates(start, &pay.frequency, end, today) {
            let date = date.format("%Y-%m-%d").to_string();
            let i = cell(&mut weeks, &date, Some(user_id), rep);
            weeks.get_mut(date.as_str()).expect("cell")[i].base += pay.amount;
        }
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
    fn added_jobs_are_paid_the_week_after_paid_and_installed() {
        let d = |s: &str| NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
        let today = d("2026-11-03");
        // Done Monday or Friday of the week of 11/2: Friday of the next week.
        assert_eq!(
            next_week_payroll(d("2026-11-02"), today, 5),
            d("2026-11-13")
        );
        assert_eq!(
            next_week_payroll(d("2026-11-06"), today, 5),
            d("2026-11-13")
        );
        // Sunday still ends that week.
        assert_eq!(
            next_week_payroll(d("2026-11-08"), today, 5),
            d("2026-11-13")
        );
        // Done the week before: this Friday.
        assert_eq!(
            next_week_payroll(d("2026-10-28"), today, 5),
            d("2026-11-06")
        );
        // Long ago: the next payroll from today, never one gone by.
        assert_eq!(
            next_week_payroll(d("2026-09-01"), today, 5),
            d("2026-11-06")
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
    fn feature_sales_sum_each_extra_by_area_quantity() {
        let line = |key: &str, description: &str, total: f64| PriceLine {
            key: key.into(),
            description: description.into(),
            unit: None,
            unit_price: None,
            total,
            flags: crate::quote_pricing::pricing::Flags {
                hidden: false,
                allow_discount: true,
                editable: true,
                taxable: false,
            },
            missing: false,
            note: None,
            flat_each: None,
            typed: false,
        };
        let area =
            |quantity: f64, lines: Vec<PriceLine>| crate::quote_pricing::pricing::AreaPrice {
                area_id: Some("a".into()),
                name: "Kitchen".into(),
                quantity,
                lines,
                subtotal: 0.0,
                missing: false,
            };
        let price = OptionPrice {
            option_id: "o".into(),
            areas: vec![area(
                2.0,
                vec![
                    line("a|Waterfall:36", "12 sq ft - 36\" Waterfall", 600.0),
                    line("a|waterfallInstall", "1 Waterfall Installation", 150.0),
                    line("a|edge:0", "20 lin ft Finished Edge - Ogee", 200.0),
                    line(
                        "a|edge:1",
                        "10 lin ft Finished Edge - Triple Waterfall",
                        0.0,
                    ),
                    line("a|edge:2", "8 lin ft Finished Edge - Eased", 0.0),
                    line("a|miterEach", "2 Mitered Edges", 100.0),
                    line("a|faucetHole", "3 Faucet Holes", 0.0),
                    line("a|material", "40 sq ft Quartz White", 2000.0),
                ],
            )],
            other: area(
                1.0,
                vec![
                    line("item:1", "2 - Support Corbel", 80.0),
                    line("item:2", "Seam Phantom", 120.0),
                    line("item:3", "Faucet - Chrome", 250.0),
                    line("item:4", "Karran Soap Dispenser", 45.0),
                ],
            ),
            discount: 0.0,
            tax: 0.0,
            total: 0.0,
            missing: false,
        };
        let sold = feature_sales(&price);
        assert_eq!(sold.get("waterfall"), Some(&1500.0));
        assert_eq!(sold.get("custom_edge"), Some(&400.0));
        assert_eq!(sold.get("miter"), Some(&200.0));
        assert_eq!(sold.get("support"), Some(&80.0));
        assert_eq!(sold.get("phantom_seam"), Some(&120.0));
        assert_eq!(sold.get("faucet"), Some(&250.0));
        assert_eq!(sold.get("soap_dispenser"), Some(&45.0));
        assert_eq!(sold.len(), 7);
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
        let row = |sold: &str, payroll: &str, total: f64| PaidRow {
            user_id: Some(100),
            rep: "Rita".into(),
            quote_total: total,
            commission: 50.0,
            paid_date: Some(payroll.into()),
            payroll_date: Some(payroll.into()),
            signed_at: Some(sold.into()),
            goal: GoalRow::Sale,
        };
        // $100 every Friday from 9/11 to 10/2: paid on weeks with no commission too.
        let base = BasePay {
            id: 1,
            user_id: Some(100),
            rep: Some("Rita".into()),
            position_id: None,
            amount: 100.0,
            frequency: "weekly".into(),
            start_date: "2026-09-11".into(),
            end_date: Some("2026-10-02".into()),
        };
        let weeks = payroll_weeks(
            &[
                row("2026-09-08", "2026-09-11", 600.0),
                row("2026-09-20", "2026-09-25", 600.0),
            ],
            std::slice::from_ref(&goal),
            &[base],
            5,
            d("2026-10-20"),
        );
        let dates: Vec<&str> = weeks.iter().map(|w| w.payroll_date.as_str()).collect();
        assert_eq!(
            dates,
            ["2026-10-02", "2026-09-25", "2026-09-18", "2026-09-11"]
        );
        assert_eq!(weeks[0].reps[0].bonus, 250.0);
        assert_eq!(weeks[0].reps[0].total, 350.0);
        assert_eq!(weeks[1].reps[0].total, 150.0);
        assert_eq!(weeks[2].reps[0].total, 100.0);

        // The goal counts everything sold that month, paid or not yet: CRM
        // quotes and the sheet's jobs (cabinets too).
        let cabinets = PaidRow {
            goal: GoalRow::of(false, Some("Smith (cabinets)"), 700.0, 24.5),
            ..row("2026-08-03", "2026-08-07", 700.0)
        };
        let unpaid = PaidRow {
            payroll_date: None,
            paid_date: None,
            ..row("2026-08-20", "", 400.0)
        };
        let weeks = payroll_weeks(
            &[cabinets.clone(), unpaid.clone()],
            std::slice::from_ref(&goal),
            &[],
            5,
            d("2026-10-20"),
        );
        // August's bonus: the first payroll Friday after the month, 9/4.
        let bonus = weeks
            .iter()
            .find(|w| w.payroll_date == "2026-09-04")
            .unwrap();
        assert_eq!(bonus.reps[0].bonus, 250.0);

        // A bonus the sheet paid on 9/4 is August's: no second one.
        let paid = PaidRow {
            goal: GoalRow::of(false, Some("Bonus August 1.1K"), 250.0, 250.0),
            ..row("2026-09-04", "2026-09-04", 250.0)
        };
        let weeks = payroll_weeks(
            &[cabinets, unpaid, paid],
            std::slice::from_ref(&goal),
            &[],
            5,
            d("2026-10-20"),
        );
        let total: f64 = weeks.iter().flat_map(|w| &w.reps).map(|r| r.bonus).sum();
        assert_eq!(total, 0.0);

        // A sheet row paying its whole total (a fee) is no sale.
        let fee = PaidRow {
            goal: GoalRow::of(false, Some("CRM"), 2000.0, 2000.0),
            ..row("2026-09-03", "2026-09-04", 2000.0)
        };
        let weeks = payroll_weeks(&[fee], &[goal], &[], 5, d("2026-10-20"));
        assert!(weeks.iter().all(|w| w.reps.iter().all(|r| r.bonus == 0.0)));
    }

    #[test]
    fn a_positions_base_pay_pays_everyone_with_it_who_has_none_of_their_own() {
        let pay = |id: i64, user_id: Option<i32>, position_id: Option<i32>, amount: f64| BasePay {
            id,
            user_id,
            rep: user_id.map(|_| "Own".into()),
            position_id,
            amount,
            frequency: "weekly".into(),
            start_date: "2026-10-02".into(),
            end_date: None,
        };
        let member = |user_id: i32, position_id: i32| PositionMember {
            user_id,
            rep: format!("User {user_id}"),
            position_id,
        };
        let people = base_pay_by_person(
            &[
                pay(1, None, Some(1), 500.0),
                pay(2, Some(11), None, 700.0),
                pay(3, None, Some(2), 900.0),
            ],
            // 10 and 11 are sales reps; 11 has their own base pay.
            &[member(10, 1), member(11, 1), member(12, 3)],
        );
        let by_user: Vec<(Option<i32>, f64)> =
            people.iter().map(|p| (p.user_id, p.amount)).collect();
        assert_eq!(by_user, [(Some(11), 700.0), (Some(10), 500.0)]);
        assert_eq!(people[1].rep.as_deref(), Some("User 10"));
        assert_eq!(people[1].position_id, Some(1));
    }

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn base_pay_runs_every_week_two_weeks_or_month_through_the_next_payday() {
        let ymd = |dates: Vec<NaiveDate>| -> Vec<String> {
            dates
                .iter()
                .map(|d| d.format("%Y-%m-%d").to_string())
                .collect()
        };
        // Weekly: through the first payday after today (Wednesday 11/4).
        assert_eq!(
            ymd(base_pay_dates(
                d("2026-10-16"),
                "weekly",
                None,
                d("2026-11-04")
            )),
            ["2026-10-16", "2026-10-23", "2026-10-30", "2026-11-06"]
        );
        assert_eq!(
            ymd(base_pay_dates(
                d("2026-10-09"),
                "biweekly",
                None,
                d("2026-11-04")
            )),
            ["2026-10-09", "2026-10-23", "2026-11-06"]
        );
        // Every 20th; a 31st falls back to the month's last day.
        assert_eq!(
            ymd(base_pay_dates(
                d("2026-09-20"),
                "monthly",
                None,
                d("2026-11-04")
            )),
            ["2026-09-20", "2026-10-20", "2026-11-20"]
        );
        assert_eq!(
            ymd(base_pay_dates(
                d("2026-12-31"),
                "monthly",
                None,
                d("2027-03-01")
            )),
            ["2026-12-31", "2027-01-31", "2027-02-28", "2027-03-31"]
        );
        // Until a last day; not started yet: only the first payday.
        assert_eq!(
            ymd(base_pay_dates(
                d("2026-10-16"),
                "weekly",
                Some(d("2026-10-29")),
                d("2026-11-04")
            )),
            ["2026-10-16", "2026-10-23"]
        );
        assert_eq!(
            ymd(base_pay_dates(
                d("2026-12-04"),
                "weekly",
                None,
                d("2026-11-04")
            )),
            ["2026-12-04"]
        );
    }
}
