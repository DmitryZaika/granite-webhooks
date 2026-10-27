//! What a company keeps doing to its invoice drafts, learned without AI.
//!
//! Every invoice created from a quote records what became of each draft line
//! (kept, changed, removed) and the class, CC and product it went out with
//! (`qbo_invoice_draft_outcomes`); a company that just connected `QuickBooks`
//! starts from how its recent invoices set class, CC and product
//! (`qbo_invoice_scans`). A change becomes a rule only when it wins at least
//! 4 of 5 of the latest (up to [`WINDOW`]) outcomes, at least [`MIN_SEEN`]
//! times, including the 2 newest: undoing it twice turns it off. Rules
//! reword or leave out text lines and fill class and CC; they never touch an
//! amount or leave out a priced line. The product is learned from the
//! invoices' words ([`super::products`]).

use super::instructions;
use super::invoice::{EditableInvoice, Line, text};
use super::products::Model;
use crate::auth::EmployeeUser;
use crate::auth::session::SessionUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use lambda_http::tracing;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::MySqlPool;
use std::collections::HashMap;
use std::sync::LazyLock;

/// Outcomes a rule is decided from, newest first.
pub const WINDOW: usize = 10;
/// Fewest outcomes that make a rule.
pub const MIN_SEEN: usize = 3;

/// Counts and measurements: the number a line starts with ("17 - Hardware"),
/// numbers with a unit ("33.5 lin ft", "59.2 sqft", "10 ea") and prices
/// ("$99"). Other numbers (60/40, R20, 32") name the thing and stay as they are.
static VARIABLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^\s*(\d+(?:\.\d+)?)|(\d+(?:\.\d+)?)\s*(?:sq\.?\s*ft|sqft|lin\.?\s*ft|ea\b)|\$\s*(\d+(?:\.\d+)?)",
    )
    .expect("valid regex")
});
static NUMBER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\d+(?:\.\d+)?").expect("valid regex"));
static SLOT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{(\d+)\}").expect("valid regex"));

/// The variable numbers of a text, with where they are.
fn variables(text: &str) -> Vec<(usize, usize, &str)> {
    VARIABLE
        .captures_iter(text)
        .filter_map(|caps| (1..=3).find_map(|i| caps.get(i)))
        .map(|m| (m.start(), m.end(), m.as_str()))
        .collect()
}

/// A line's text with its counts and measurements as `#`, lowercased and
/// squashed: the same line on every quote.
pub fn shape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for (start, end, _) in variables(text) {
        out.push_str(&text[at..start]);
        out.push('#');
        at = end;
    }
    out.push_str(&text[at..]);
    out.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// What a draft line was changed to, with each of the draft's counts and
/// measurements it repeats as `{i}`: "Hardware - {0}" from "17 - Cabinet
/// Hardware" saved as "Hardware - 17".
pub fn template(draft: &str, saved: &str) -> String {
    let values: Vec<&str> = variables(draft).into_iter().map(|(_, _, v)| v).collect();
    let mut used = vec![false; values.len()];
    let mut out = String::with_capacity(saved.len());
    let mut at = 0;
    for m in NUMBER.find_iter(saved) {
        let slot = values
            .iter()
            .enumerate()
            .find(|(i, value)| !used[*i] && **value == m.as_str())
            .map(|(i, _)| i);
        if let Some(i) = slot {
            used[i] = true;
            out.push_str(&saved[at..m.start()]);
            out.push('{');
            out.push_str(&i.to_string());
            out.push('}');
            at = m.end();
        }
    }
    out.push_str(&saved[at..]);
    out
}

/// A template filled with a line's own counts and measurements; `None` when
/// it needs one the line lacks.
pub fn fill(template: &str, text: &str) -> Option<String> {
    let values: Vec<&str> = variables(text).into_iter().map(|(_, _, v)| v).collect();
    let mut out = String::with_capacity(template.len());
    let mut at = 0;
    for caps in SLOT.captures_iter(template) {
        let whole = caps.get(0)?;
        let index: usize = caps[1].parse().ok()?;
        out.push_str(&template[at..whole.start()]);
        out.push_str(values.get(index)?);
        at = whole.end();
    }
    out.push_str(&template[at..]);
    Some(out)
}

/// One recorded outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// `kept`, `changed`, `removed`, `none`, `sales_rep` or `value`.
    pub outcome: String,
    pub value: Option<String>,
}

impl Outcome {
    pub fn new(outcome: &str, value: Option<&str>) -> Self {
        Self {
            outcome: outcome.to_string(),
            value: value.map(str::to_string),
        }
    }
}

/// What recent outcomes (newest first) keep doing, with how many of the
/// window did it: at least [`MIN_SEEN`] times, 4 of 5 of the latest
/// [`WINDOW`], and the 2 newest.
pub fn decide(history: &[Outcome]) -> Option<(Outcome, usize)> {
    let window = &history[..history.len().min(WINDOW)];
    if window.len() < MIN_SEEN {
        return None;
    }
    let newest = &window[0];
    if window[1] != *newest {
        return None;
    }
    let seen = window.iter().filter(|o| *o == newest).count();
    (seen >= MIN_SEEN && seen * 5 >= window.len() * 4).then(|| (newest.clone(), seen))
}

/// A line rule, by the shape of the lines it is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineRule {
    /// Reworded to this template.
    Changed(String),
    /// Left out (text lines only).
    Removed,
}

/// The quotes a line rule holds on: those with a word, or those without it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct When {
    pub word: String,
    /// On quotes with the word (else: without it).
    pub has: bool,
}

/// A line rule and the quotes it holds on (all when `when` is `None`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShapeRule {
    pub rule: LineRule,
    pub when: Option<When>,
}

/// One outcome of a line, with the words of the quote it was on (unknown for
/// invoices created before those were kept).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinePast {
    pub outcome: Outcome,
    /// Sorted and distinct.
    pub words: Option<Vec<String>>,
}

/// Outcomes a rule that holds on some quotes only is looked for in.
pub const WHEN_WINDOW: usize = 40;
/// Fewest outcomes behind such a rule.
pub const WHEN_MIN_SEEN: usize = 4;

fn rule_of(outcome: &Outcome) -> Option<LineRule> {
    match outcome.outcome.as_str() {
        "changed" => Some(LineRule::Changed(outcome.value.clone()?)),
        "removed" => Some(LineRule::Removed),
        _ => None,
    }
}

/// The line rules recent outcomes make, from each shape's outcomes newest
/// first: what the latest keep doing (see [`decide`]), or else what they
/// keep doing on the quotes with (or without) one word while the line stays
/// as it is on the others: "Fabrication is left out on quotes with cabinets".
/// `common` counts the company's quotes with each word.
#[allow(clippy::implicit_hasher)]
pub fn line_rules(
    history: &HashMap<String, Vec<LinePast>>,
    common: &HashMap<String, usize>,
) -> HashMap<String, ShapeRule> {
    history
        .iter()
        .filter_map(|(shape, past)| {
            let latest: Vec<Outcome> = past.iter().take(WINDOW).map(|p| p.outcome.clone()).collect();
            let always = decide(&latest).and_then(|(winner, _)| rule_of(&winner));
            let rule = match always {
                Some(rule) => ShapeRule { rule, when: None },
                None => when_rule(past, common)?,
            };
            Some((shape.clone(), rule))
        })
        .collect()
}

/// A rule the outcomes on the quotes with (or without) one word make, while
/// the others hardly ever did it: at least [`WHEN_MIN_SEEN`] times and the
/// 2 newest of them (see [`decide`]), on 2 or more other quotes. Of such
/// words, the one with the most outcomes behind it, then rules for quotes
/// with a word before ones for quotes without it, then the word on the most
/// of the company's quotes ("cabinets" rather than "shaker").
fn when_rule(past: &[LinePast], common: &HashMap<String, usize>) -> Option<ShapeRule> {
    let known: Vec<(&Outcome, &[String])> = past
        .iter()
        .filter_map(|p| Some((&p.outcome, p.words.as_deref()?)))
        .take(WHEN_WINDOW)
        .collect();
    if known.len() < WHEN_MIN_SEEN + 2 {
        return None;
    }
    let mut candidates: Vec<&str> = known
        .iter()
        .flat_map(|(_, words)| words.iter().map(String::as_str))
        .collect();
    candidates.sort_unstable();
    candidates.dedup();
    let mut best: Option<((usize, bool, usize), When, LineRule)> = None;
    for word in candidates {
        for has in [true, false] {
            let (group, rest): (Vec<_>, Vec<_>) = known
                .iter()
                .partition(|(_, words)| has_word(words, word) == has);
            if group.len() < WHEN_MIN_SEEN || rest.len() < 2 {
                continue;
            }
            let group: Vec<Outcome> = group.iter().map(|(o, _)| (*o).clone()).collect();
            let Some((winner, seen)) = decide(&group) else {
                continue;
            };
            let Some(rule) = rule_of(&winner) else {
                continue;
            };
            let elsewhere = rest.iter().filter(|(o, _)| **o == winner).count();
            if seen < WHEN_MIN_SEEN || elsewhere * 5 > rest.len() {
                continue;
            }
            let key = (seen, has, common.get(word).copied().unwrap_or(0));
            if best.as_ref().is_none_or(|(held, _, _)| key > *held) {
                let when = When {
                    word: word.to_string(),
                    has,
                };
                best = Some((key, when, rule));
            }
        }
    }
    best.map(|(_, when, rule)| ShapeRule {
        rule,
        when: Some(when),
    })
}

fn is_text(line: &Line) -> bool {
    line.detail_type == "DescriptionOnly"
}

fn is_editable(line: &Line) -> bool {
    line.detail_type == "SalesItemLineDetail" || is_text(line)
}

/// The draft with the rules that hold on it (its quote has these `words`)
/// applied: lines reworded in place, text lines left out returned apart (so
/// the editor can bring them back). Priced lines are only ever reworded.
#[allow(clippy::implicit_hasher)]
pub fn apply(
    lines: Vec<Line>,
    rules: &HashMap<String, ShapeRule>,
    words: &[String],
) -> (Vec<Line>, Vec<Line>) {
    let mut kept = Vec::with_capacity(lines.len());
    let mut removed = Vec::new();
    for mut line in lines {
        let rule = line
            .description
            .as_deref()
            .filter(|_| is_editable(&line))
            .and_then(|text| rules.get(&shape(text)))
            .filter(|rule| {
                rule.when
                    .as_ref()
                    .is_none_or(|when| has_word(words, &when.word) == when.has)
            })
            .map(|rule| &rule.rule);
        match rule {
            Some(LineRule::Removed) if is_text(&line) => removed.push(line),
            Some(LineRule::Changed(to)) => {
                let filled = line.description.as_deref().and_then(|text| fill(to, text));
                if let Some(text) = filled.filter(|text| !text.trim().is_empty()) {
                    line.description = Some(text);
                }
                kept.push(line);
            }
            _ => kept.push(line),
        }
    }
    (kept, removed)
}

/// What became of one draft line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineOutcome {
    pub shape: String,
    pub draft_text: String,
    pub outcome: Outcome,
}

fn trimmed(text: Option<&str>) -> &str {
    text.map_or("", str::trim)
}

/// What became of each draft line (before rules) in the saved invoice,
/// matched by line id.
pub fn line_outcomes(draft: &[Line], saved: &[Line]) -> Vec<LineOutcome> {
    draft
        .iter()
        .filter(|line| is_editable(line) && line.id.is_some())
        .filter_map(|before| {
            let text = trimmed(before.description.as_deref());
            if text.is_empty() {
                return None;
            }
            let after = saved.iter().find(|line| line.id == before.id);
            let outcome = match after.map(|line| trimmed(line.description.as_deref())) {
                None | Some("") => Outcome::new("removed", None),
                Some(now) if now == text => Outcome::new("kept", None),
                Some(now) => Outcome::new("changed", Some(&template(text, now))),
            };
            Some(LineOutcome {
                shape: shape(text),
                draft_text: text.chars().take(1000).collect(),
                outcome,
            })
        })
        .collect()
}

fn same(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

/// How a class or CC was set: not at all, to the sales rep (any of `reps`:
/// the quote's rep, or any employee for an invoice `QuickBooks` already had),
/// or to a value.
pub fn header_outcome(saved: Option<&str>, reps: &[&str]) -> Outcome {
    match saved.map(str::trim).filter(|s| !s.is_empty()) {
        None => Outcome::new("none", None),
        Some(value) if reps.iter().any(|rep| same(rep, value)) => Outcome::new("sales_rep", None),
        Some(value) => Outcome::new("value", Some(value)),
    }
}

/// A class or CC for a new invoice from its rule: the quote's rep for
/// `sales_rep`, the value for `value`, nothing otherwise.
pub fn header_value(rule: Option<&Outcome>, rep: Option<&str>) -> Option<String> {
    let rule = rule?;
    match rule.outcome.as_str() {
        "sales_rep" => rep.map(str::to_string),
        "value" => rule.value.clone(),
        _ => None,
    }
    .filter(|value| !value.trim().is_empty())
}

/// Line outcomes read for rules.
const LINE_HISTORY: u32 = 5000;
/// Newest quotes whose words are read for line rules.
const QUOTE_WORDS: u32 = 1000;
/// Reps' newest product choices read.
const ITEM_HISTORY: u32 = 200;
/// Newest invoices a scan reads.
const SCANNED: u32 = 200;

/// The quote's sales rep.
#[derive(Debug, Default, sqlx::FromRow)]
struct Rep {
    name: Option<String>,
    email: Option<String>,
}

async fn quote_rep(pool: &MySqlPool, company_id: i32, quote_id: i32) -> sqlx::Result<Option<Rep>> {
    sqlx::query_as(
        "SELECT u.name, u.email FROM quotes q
           LEFT JOIN users u ON u.id = q.salesperson_id AND u.is_deleted = 0
          WHERE q.id = ? AND q.company_id = ? AND q.deleted_at IS NULL",
    )
    .bind(quote_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await
}

/// A class, CC or product as an invoice had it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct Seen {
    pub(super) outcome: String,
    pub(super) value: Option<String>,
    /// The product's name.
    pub(super) name: Option<String>,
    /// A scanned invoice's words (see [`words`]), for learning which lines go
    /// with which product.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) words: Vec<String>,
    /// A rep's choice for an invoice created here from a quote (the words
    /// are the quote's), not a scanned invoice.
    #[serde(skip)]
    pub(super) pair: bool,
}

impl Seen {
    fn outcome(&self) -> Outcome {
        Outcome {
            outcome: self.outcome.clone(),
            value: self.value.clone(),
        }
    }
}

/// How the company's newest `QuickBooks` invoices set class, CC and product
/// (newest first), and its classes.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Scan {
    class: Vec<Seen>,
    cc: Vec<Seen>,
    item: Vec<Seen>,
    classes: Vec<String>,
}

fn seen(outcome: Outcome) -> Seen {
    Seen {
        outcome: outcome.outcome,
        value: outcome.value,
        name: None,
        words: Vec::new(),
        pair: false,
    }
}

/// The distinct words (letters only, 3 or more) of some line texts.
pub(super) fn words<'a>(texts: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut words: Vec<String> = texts
        .flat_map(|text| text.split(|c: char| !c.is_alphabetic()))
        .filter(|word| word.chars().count() >= 3)
        .map(str::to_lowercase)
        .collect();
    words.sort_unstable();
    words.dedup();
    words
}

/// The first priced line's product of a `QuickBooks` invoice, with the
/// invoice's words.
fn first_item(invoice: &Value) -> Option<Seen> {
    let lines = invoice["Line"].as_array()?;
    let item = lines
        .iter()
        .find(|line| line["DetailType"] == "SalesItemLineDetail")?;
    let item = &item["SalesItemLineDetail"]["ItemRef"];
    Some(Seen {
        outcome: "value".into(),
        value: Some(text(&item["value"])?),
        name: text(&item["name"]),
        words: words(lines.iter().filter_map(|line| line["Description"].as_str())),
        pair: false,
    })
}

/// Words kept as one text, back as a sorted list.
fn split_words(text: &str) -> Vec<String> {
    let mut words: Vec<String> = text.split_whitespace().map(str::to_string).collect();
    words.sort_unstable();
    words.dedup();
    words
}

fn has_word(words: &[String], word: &str) -> bool {
    words.binary_search_by(|w| w.as_str().cmp(word)).is_ok()
}

/// Reads the newest invoices and classes again and keeps what they show.
async fn fresh_scan(state: &AppState, company_id: i32) -> Option<Scan> {
    let access = state.qbo.access(&state.pool, company_id).await.ok()?;
    let invoices = match state
        .qbo
        .query(
            &access,
            &format!("select * from Invoice ORDERBY MetaData.CreateTime DESC MAXRESULTS {SCANNED}"),
            "Invoice",
        )
        .await
    {
        Ok(invoices) => invoices,
        Err(err) => {
            tracing::warn!(error = %err, company_id, "invoice scan failed");
            return None;
        }
    };
    let classes = state
        .qbo
        .query(
            &access,
            "select * from Class where Active = true MAXRESULTS 1000",
            "Class",
        )
        .await
        .unwrap_or_default();
    let staff: Vec<(Option<String>, Option<String>)> =
        sqlx::query_as("SELECT name, email FROM users WHERE company_id = ? AND is_deleted = 0")
            .bind(company_id)
            .fetch_all(&state.pool)
            .await
            .ok()?;
    let names: Vec<&str> = staff
        .iter()
        .filter_map(|(name, _)| name.as_deref())
        .collect();
    let emails: Vec<&str> = staff
        .iter()
        .filter_map(|(_, email)| email.as_deref())
        .collect();
    let scan = Scan {
        class: invoices
            .iter()
            .map(|i| {
                seen(header_outcome(
                    text(&i["ClassRef"]["name"]).as_deref(),
                    &names,
                ))
            })
            .collect(),
        cc: invoices
            .iter()
            .map(|i| {
                seen(header_outcome(
                    text(&i["BillEmailCc"]["Address"]).as_deref(),
                    &emails,
                ))
            })
            .collect(),
        item: invoices.iter().filter_map(first_item).collect(),
        classes: classes
            .iter()
            .filter_map(|c| text(&c["FullyQualifiedName"]).or_else(|| text(&c["Name"])))
            .collect(),
    };
    let json = serde_json::to_string(&scan).ok()?;
    if let Err(err) = sqlx::query(
        "INSERT INTO qbo_invoice_scans (company_id, stats_json) VALUES (?, ?)
         ON DUPLICATE KEY UPDATE stats_json = VALUES(stats_json), scanned_at = CURRENT_TIMESTAMP",
    )
    .bind(company_id)
    .bind(json)
    .execute(&state.pool)
    .await
    {
        tracing::warn!(error = %err, company_id, "invoice scan not kept");
    }
    Some(scan)
}

/// The company's scan, read again when over a day old; none without
/// `QuickBooks`.
async fn scan(state: &AppState, company_id: i32) -> sqlx::Result<Option<Scan>> {
    let kept: Option<String> = sqlx::query_scalar(
        "SELECT CAST(stats_json AS CHAR) FROM qbo_invoice_scans
          WHERE company_id = ? AND scanned_at > DATE_SUB(NOW(), INTERVAL 1 DAY)",
    )
    .bind(company_id)
    .fetch_optional(&state.pool)
    .await?;
    // A scan kept before invoices' words were read is read again.
    if let Some(scan) = kept
        .and_then(|json| serde_json::from_str::<Scan>(&json).ok())
        .filter(|scan| scan.item.is_empty() || scan.item.iter().any(|seen| !seen.words.is_empty()))
    {
        return Ok(Some(scan));
    }
    Ok(fresh_scan(state, company_id).await)
}

/// Each line shape's outcomes, newest first (up to [`WINDOW`]).
/// The quote words of the company's newest invoices created here, by invoice.
async fn quote_words(
    pool: &MySqlPool,
    company_id: i32,
) -> sqlx::Result<HashMap<String, Vec<String>>> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT qbo_invoice_id, words FROM qbo_invoice_draft_words
          WHERE company_id = ? ORDER BY created_at DESC LIMIT ?",
    )
    .bind(company_id)
    .bind(QUOTE_WORDS)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, words)| (id, split_words(&words)))
        .collect())
}

/// Each line shape's outcomes, newest first (up to [`WHEN_WINDOW`]), with
/// their quotes' words; and on how many of the company's quotes each word is.
async fn line_history(
    pool: &MySqlPool,
    company_id: i32,
) -> sqlx::Result<(HashMap<String, Vec<LinePast>>, HashMap<String, usize>)> {
    let quotes = quote_words(pool, company_id).await?;
    let rows: Vec<(String, String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT draft_shape, outcome, saved_value, qbo_invoice_id FROM qbo_invoice_draft_outcomes
          WHERE company_id = ? AND field = 'line' AND draft_shape IS NOT NULL
          ORDER BY id DESC LIMIT ?",
    )
    .bind(company_id)
    .bind(LINE_HISTORY)
    .fetch_all(pool)
    .await?;
    let mut history: HashMap<String, Vec<LinePast>> = HashMap::new();
    for (shape, outcome, value, invoice) in rows {
        let past = history.entry(shape).or_default();
        if past.len() < WHEN_WINDOW {
            past.push(LinePast {
                outcome: Outcome { outcome, value },
                words: invoice.and_then(|id| quotes.get(&id)).cloned(),
            });
        }
    }
    let mut common: HashMap<String, usize> = HashMap::new();
    for word in quotes.values().flatten() {
        *common.entry(word.clone()).or_default() += 1;
    }
    Ok((history, common))
}

/// The products reps saved on invoices created here (with the quote's
/// words, when kept), newest first, then the scanned invoices'.
async fn item_history(
    pool: &MySqlPool,
    company_id: i32,
    scanned: &[Seen],
) -> sqlx::Result<Vec<Seen>> {
    let rows: Vec<(Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT o.saved_value, o.draft_text, w.words FROM qbo_invoice_draft_outcomes o
           LEFT JOIN qbo_invoice_draft_words w
             ON w.company_id = o.company_id AND w.qbo_invoice_id = o.qbo_invoice_id
          WHERE o.company_id = ? AND o.field = 'item' AND o.outcome = 'value'
          ORDER BY o.id DESC LIMIT ?",
    )
    .bind(company_id)
    .bind(ITEM_HISTORY)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(value, name, words)| Seen {
            outcome: "value".into(),
            value,
            name,
            words: words.as_deref().map(split_words).unwrap_or_default(),
            pair: true,
        })
        .chain(scanned.iter().cloned())
        .collect())
}

/// A header field's outcomes on invoices made here, newest first, then the
/// scanned ones.
async fn header_history(
    pool: &MySqlPool,
    company_id: i32,
    field: &str,
    scanned: &[Seen],
) -> sqlx::Result<Vec<Seen>> {
    let rows: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT outcome, saved_value, draft_text FROM qbo_invoice_draft_outcomes
          WHERE company_id = ? AND field = ? ORDER BY id DESC LIMIT ?",
    )
    .bind(company_id)
    .bind(field)
    .bind(u32::try_from(WINDOW).unwrap_or(10))
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(outcome, value, name)| Seen {
            outcome,
            value,
            name,
            words: Vec::new(),
            pair: false,
        })
        .chain(scanned.iter().cloned())
        .collect())
}

fn decided(history: &[Seen]) -> Option<Outcome> {
    let outcomes: Vec<Outcome> = history.iter().map(Seen::outcome).collect();
    decide(&outcomes).map(|(winner, _)| winner)
}

/// Body of `POST /v1/quotes/{id}/invoice-draft`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct DraftInput {
    /// The draft's lines, as the quote makes them.
    pub lines: Vec<Line>,
}

/// `POST /v1/quotes/{id}/invoice-draft`.
#[derive(Debug, Serialize, ToSchema)]
pub struct DraftRules {
    /// The lines with the company's usual changes made.
    pub lines: Vec<Line>,
    /// Text lines the company usually leaves out, for the editor to offer back.
    pub removed: Vec<Line>,
    /// The class the company's invoices usually get.
    pub class_name: Option<String>,
    /// The CC they usually get.
    pub bill_email_cc: Option<String>,
    /// The product their priced lines usually use.
    pub item_id: Option<String>,
    /// Its name.
    pub item_name: Option<String>,
}

/// Make a quote's invoice draft the company's way
///
/// Applies the changes the company's recent invoices keep making to such a
/// draft: lines reworded or left out (text lines only; amounts never change),
/// and the class, CC and product they usually get, from invoices made here
/// and, for a company that just connected, its newest `QuickBooks` invoices.
/// Nothing is saved.
#[crate::openapi::path(
    post,
    path = "/v1/quotes/{id}/invoice-draft",
    operation_id = "apply_invoice_draft_rules",
    tag = "quickbooks",
    params(("id" = i32, Path, description = "Quote id")),
    request_body = DraftInput,
    responses(
        (status = 200, description = "The draft the company's way", body = DraftRules),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such quote in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn invoice_draft(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(quote_id): Path<i32>,
    JsonBody(input): JsonBody<DraftInput>,
) -> ApiResult<Json<DraftRules>> {
    let company_id = user.company_id;
    let rep = quote_rep(&state.pool, company_id, quote_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let (history, common) = line_history(&state.pool, company_id).await?;
    let rules = line_rules(&history, &common);
    let draft_words = words(input.lines.iter().filter_map(|line| line.description.as_deref()));
    let texts: Vec<String> = input
        .lines
        .iter()
        .filter_map(|line| line.description.clone())
        .collect();
    let (mut lines, mut removed) = apply(input.lines, &rules, &draft_words);
    let told = instructions::load(&state.pool, company_id).await;
    let told = instructions::holding(&told, &texts);
    let scan = scan(&state, company_id).await?.unwrap_or_default();
    let class_name = header_value(
        decided(&header_history(&state.pool, company_id, "class", &scan.class).await?).as_ref(),
        rep.name.as_deref(),
    )
    .filter(|name| scan.classes.is_empty() || scan.classes.iter().any(|c| same(c, name)));
    let bill_email_cc = header_value(
        decided(&header_history(&state.pool, company_id, "cc", &scan.cc).await?).as_ref(),
        rep.email.as_deref(),
    );
    let class_name = instructions::class(&told).or(class_name);
    let bill_email_cc = instructions::cc(&told).or(bill_email_cc);
    let (item_id, item_name) = match instructions::product(&told) {
        Some(told) => Some(told),
        None => {
            let items = item_history(&state.pool, company_id, &scan.item).await?;
            Model::learn(&items).pick(&draft_words, &instructions::avoided(&told))
        }
    }
    .unzip();
    let item_name = item_name.flatten();
    if let Some(id) = &item_id {
        for line in lines
            .iter_mut()
            .filter(|line| line.detail_type == "SalesItemLineDetail")
        {
            line.item_ref_value = Some(id.clone());
            line.item_ref_name.clone_from(&item_name);
        }
    }
    instructions::apply_lines(&told, &mut lines, &mut removed);
    Ok(Json(DraftRules {
        lines,
        removed,
        class_name,
        bill_email_cc,
        item_id,
        item_name,
    }))
}

#[allow(clippy::too_many_arguments)]
async fn insert(
    pool: &MySqlPool,
    user: &SessionUser,
    quote_id: Option<i32>,
    invoice_id: &str,
    field: &str,
    shape: Option<&str>,
    draft_text: Option<&str>,
    outcome: &Outcome,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO qbo_invoice_draft_outcomes
           (company_id, user_id, quote_id, qbo_invoice_id, field, draft_shape, draft_text, outcome, saved_value)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(user.company_id)
    .bind(user.id)
    .bind(quote_id)
    .bind(invoice_id)
    .bind(field)
    .bind(shape.map(|s| s.chars().take(255).collect::<String>()))
    .bind(draft_text)
    .bind(&outcome.outcome)
    .bind(outcome.value.as_deref().map(|v| v.chars().take(1000).collect::<String>()))
    .execute(pool)
    .await
    .map(|_| ())
}

/// Keeps what became of a draft in the invoice created from it: each draft
/// line (`draft`, before rules), and the class, CC and product.
pub async fn record(
    pool: &MySqlPool,
    user: &SessionUser,
    quote_id: Option<i32>,
    invoice_id: &str,
    draft: &[Line],
    saved: &EditableInvoice,
) -> sqlx::Result<()> {
    let rep = match quote_id {
        Some(id) => quote_rep(pool, user.company_id, id)
            .await?
            .unwrap_or_default(),
        None => Rep::default(),
    };
    let draft_words = words(draft.iter().filter_map(|line| line.description.as_deref()));
    if !draft_words.is_empty() {
        sqlx::query(
            "INSERT INTO qbo_invoice_draft_words (company_id, qbo_invoice_id, words) VALUES (?, ?, ?)
             ON DUPLICATE KEY UPDATE words = VALUES(words)",
        )
        .bind(user.company_id)
        .bind(invoice_id)
        .bind(draft_words.join(" "))
        .execute(pool)
        .await?;
    }
    for line in line_outcomes(draft, &saved.lines) {
        insert(
            pool,
            user,
            quote_id,
            invoice_id,
            "line",
            Some(&line.shape),
            Some(&line.draft_text),
            &line.outcome,
        )
        .await?;
    }
    let class = header_outcome(
        saved.class_name.as_deref(),
        &Vec::from_iter(rep.name.as_deref()),
    );
    insert(
        pool, user, quote_id, invoice_id, "class", None, None, &class,
    )
    .await?;
    let cc = header_outcome(
        saved.bill_email_cc.as_deref(),
        &Vec::from_iter(rep.email.as_deref()),
    );
    insert(pool, user, quote_id, invoice_id, "cc", None, None, &cc).await?;
    if let Some(line) = saved
        .lines
        .iter()
        .find(|line| line.detail_type == "SalesItemLineDetail" && line.item_ref_value.is_some())
    {
        let item = Outcome::new("value", line.item_ref_value.as_deref());
        insert(
            pool,
            user,
            quote_id,
            invoice_id,
            "item",
            None,
            line.item_ref_name.as_deref(),
            &item,
        )
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(id: &str, description: &str) -> Line {
        Line {
            id: Some(id.into()),
            line_num: None,
            detail_type: "DescriptionOnly".into(),
            description: Some(description.into()),
            amount: None,
            qty: None,
            unit_price: None,
            item_ref_value: None,
            item_ref_name: None,
            editable: true,
        }
    }

    fn sale(id: &str, description: &str, amount: f64) -> Line {
        Line {
            detail_type: "SalesItemLineDetail".into(),
            amount: Some(amount),
            qty: Some(1.0),
            unit_price: Some(amount),
            item_ref_value: Some("8".into()),
            ..text(id, description)
        }
    }

    fn o(outcome: &str, value: Option<&str>) -> Outcome {
        Outcome::new(outcome, value)
    }

    #[test]
    fn shapes_hide_counts_and_measurements_but_not_model_numbers() {
        assert_eq!(
            shape("33.5 lin ft Finished Edge - Flat"),
            shape("104.6 lin ft  Finished Edge - FLAT")
        );
        assert_eq!(
            shape("17 - Cabinet Hardware"),
            shape("4 - Cabinet Hardware")
        );
        assert_eq!(
            shape("Kitchen based @ 59.2 sqft Additional sqft of Taj @ $99/sqft"),
            shape("Kitchen based @ 30 sqft Additional sqft of Taj @ $120/sqft")
        );
        // A different sink, edge or size is a different line.
        assert_ne!(shape("1 Sink - 60/40"), shape("1 Sink - 50/50"));
        assert_ne!(
            shape("33.5 lin ft Finished Edge - Flat"),
            shape("33.5 lin ft Finished Edge - Eased")
        );
        assert_ne!(
            shape("1 - 30\" Undermount Sink Cutout"),
            shape("1 - 17\" Undermount Sink Cutout")
        );
        assert_ne!(
            shape("1 Sink - NS Single-R20"),
            shape("1 Sink - NS Single-R10")
        );
    }

    #[test]
    fn templates_carry_the_counts_into_the_next_line() {
        let to = template("17 - Cabinet Hardware - Model ?", "Hardware - 17");
        assert_eq!(to, "Hardware - {0}");
        assert_eq!(
            fill(&to, "4 - Cabinet Hardware - Model ?").as_deref(),
            Some("Hardware - 4")
        );
        let edge = template("33.5 lin ft Finished Edge - Flat", "Flat Edge");
        assert_eq!(
            fill(&edge, "80 lin ft Finished Edge - Flat").as_deref(),
            Some("Flat Edge")
        );
        // "10 year" names the sealer: not a slot.
        assert_eq!(
            template("83.2 sq ft - 10 year Sealer", "10 Year Sealer"),
            "10 Year Sealer"
        );
        // A slot the line lacks: no rewording.
        assert_eq!(fill("Hardware - {1}", "17 - Cabinet Hardware"), None);
    }

    #[test]
    fn a_rule_needs_three_recent_wins_and_the_two_newest() {
        let flat = o("changed", Some("Flat Edge"));
        let kept = o("kept", None);
        assert_eq!(decide(&[flat.clone(), flat.clone()]), None);
        assert_eq!(
            decide(&[flat.clone(), flat.clone(), flat.clone()]),
            Some((flat.clone(), 3))
        );
        // Undone twice: off at once, though it won before.
        let undone = [
            kept.clone(),
            kept.clone(),
            flat.clone(),
            flat.clone(),
            flat.clone(),
            flat.clone(),
        ];
        assert_eq!(decide(&undone).map(|(w, _)| w), None);
        // Undone once: still on while 4 of 5 agree.
        let mut once = vec![flat.clone(), flat.clone(), kept.clone()];
        once.extend(std::iter::repeat_n(flat.clone(), 7));
        assert_eq!(decide(&once), Some((flat.clone(), 9)));
        // Mixed habits: no rule.
        let mixed = [
            flat.clone(),
            flat.clone(),
            kept.clone(),
            flat.clone(),
            kept.clone(),
        ];
        assert_eq!(decide(&mixed), None);
        // Only the latest 10 count.
        let mut old = vec![flat.clone(); 10];
        old.extend(vec![kept; 20]);
        assert_eq!(decide(&old), Some((flat, 10)));
    }

    #[test]
    fn rules_reword_and_leave_out_text_but_never_a_priced_line() {
        let always = |rule| ShapeRule { rule, when: None };
        let rules = HashMap::from([
            (
                shape("33.5 lin ft Finished Edge - Flat"),
                always(LineRule::Changed("Flat Edge".into())),
            ),
            (shape("Laser Template"), always(LineRule::Removed)),
            (shape("Kitchen based @ 30 sqft"), always(LineRule::Removed)),
        ]);
        let (kept, removed) = apply(
            vec![
                sale("a", "Kitchen based @ 26.6 sqft", 2090.0),
                text("b", "40.9 lin ft Finished Edge - Flat"),
                text("c", "33.5 lin ft Finished Edge - Eased"),
                text("d", "Laser Template"),
            ],
            &rules,
            &[],
        );
        let texts: Vec<_> = kept
            .iter()
            .map(|l| l.description.as_deref().unwrap())
            .collect();
        assert_eq!(
            texts,
            [
                "Kitchen based @ 26.6 sqft",
                "Flat Edge",
                "33.5 lin ft Finished Edge - Eased"
            ]
        );
        assert_eq!(kept[0].amount, Some(2090.0));
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].id.as_deref(), Some("d"));
    }

    fn past(outcome: &Outcome, lines: &[&str]) -> LinePast {
        LinePast {
            outcome: outcome.clone(),
            words: Some(words(lines.iter().copied())),
        }
    }

    const CABINETS: [&str; 2] = ["12 Shaker Cabinets - White", "73.3 sq ft Fabrication"];
    const COUNTERS: [&str; 2] = ["Granite Luna Pearl", "73.3 sq ft Fabrication"];

    #[test]
    fn a_line_left_out_on_quotes_with_a_word_is_left_out_on_such_quotes() {
        let (removed, kept) = (o("removed", None), o("kept", None));
        let fabrication = shape("73.3 sq ft Fabrication");
        // Newest first: left out with cabinets, kept on countertop-only quotes.
        let history = HashMap::from([(
            fabrication.clone(),
            vec![
                past(&removed, &CABINETS),
                past(&kept, &COUNTERS),
                past(&removed, &CABINETS),
                past(&removed, &CABINETS),
                past(&kept, &COUNTERS),
                past(&removed, &CABINETS),
                past(&kept, &COUNTERS),
            ],
        )]);
        let common = HashMap::from([
            ("cabinets".to_string(), 10),
            ("shaker".to_string(), 3),
            ("white".to_string(), 5),
            ("granite".to_string(), 40),
        ]);
        let rules = line_rules(&history, &common);
        assert_eq!(
            rules[&fabrication],
            ShapeRule {
                rule: LineRule::Removed,
                when: Some(When {
                    word: "cabinets".into(),
                    has: true
                }),
            }
        );
        let draft = |extra: &str| {
            vec![
                text("a", "40 sq ft Fabrication"),
                text("b", extra),
            ]
        };
        let with = draft("10 Cabinets");
        let with_words = words(with.iter().filter_map(|l| l.description.as_deref()));
        let (kept_lines, removed_lines) = apply(with, &rules, &with_words);
        assert_eq!(kept_lines.len(), 1);
        assert_eq!(removed_lines[0].id.as_deref(), Some("a"));
        let without = draft("Granite Zurich");
        let without_words = words(without.iter().filter_map(|l| l.description.as_deref()));
        let (kept_lines, removed_lines) = apply(without, &rules, &without_words);
        assert_eq!(kept_lines.len(), 2);
        assert!(removed_lines.is_empty());

        // Reps keep it again on cabinet quotes, twice: the rule is off.
        let mut again = vec![past(&kept, &CABINETS), past(&kept, &CABINETS)];
        again.extend(history[&fabrication].iter().cloned());
        let rules = line_rules(&HashMap::from([(fabrication.clone(), again)]), &common);
        assert_eq!(rules.get(&fabrication), None);
    }

    #[test]
    fn a_line_reps_keep_leaving_out_is_left_out_until_they_keep_it_twice() {
        let (removed, kept) = (o("removed", None), o("kept", None));
        let installation = shape("73.3 sq ft Installation");
        let mut past_outcomes = vec![past(&removed, &COUNTERS); 3];
        let common = HashMap::new();
        let rules = line_rules(
            &HashMap::from([(installation.clone(), past_outcomes.clone())]),
            &common,
        );
        assert_eq!(
            rules[&installation],
            ShapeRule {
                rule: LineRule::Removed,
                when: None
            }
        );
        past_outcomes.insert(0, past(&kept, &COUNTERS));
        past_outcomes.insert(0, past(&kept, &COUNTERS));
        let rules = line_rules(&HashMap::from([(installation.clone(), past_outcomes)]), &common);
        assert_eq!(rules.get(&installation), None);
    }

    #[test]
    fn outcomes_say_what_became_of_each_draft_line() {
        let draft = [
            sale("a", "Kitchen based @ 26.6 sqft", 2090.0),
            text("b", "33.5 lin ft Finished Edge - Flat"),
            text("c", "Laser Template"),
            text("d", "17 - Cabinet Hardware"),
        ];
        let saved = [
            sale("a", "Kitchen based @ 26.6 sqft", 2090.0),
            text("b", "Flat Edge"),
            text("d", "Hardware - 17 "),
            text("x", "Added by the rep"),
        ];
        let outcomes = line_outcomes(&draft, &saved);
        let got: Vec<_> = outcomes
            .iter()
            .map(|l| (l.outcome.outcome.as_str(), l.outcome.value.as_deref()))
            .collect();
        assert_eq!(
            got,
            [
                ("kept", None),
                ("changed", Some("Flat Edge")),
                ("removed", None),
                ("changed", Some("Hardware - {0}")),
            ]
        );
        assert_eq!(outcomes[1].shape, shape("50 lin ft Finished Edge - Flat"));
    }

    #[test]
    fn class_and_cc_follow_the_rep_or_a_value() {
        assert_eq!(header_outcome(None, &["Rita"]), o("none", None));
        assert_eq!(
            header_outcome(Some(" rita "), &["Rita"]),
            o("sales_rep", None)
        );
        assert_eq!(
            header_outcome(Some("Kitchens"), &["Rita"]),
            o("value", Some("Kitchens"))
        );
        assert_eq!(
            header_value(Some(&o("sales_rep", None)), Some("Dema")).as_deref(),
            Some("Dema")
        );
        assert_eq!(header_value(Some(&o("sales_rep", None)), None), None);
        assert_eq!(
            header_value(Some(&o("value", Some("office@x.test"))), Some("Dema")).as_deref(),
            Some("office@x.test")
        );
        assert_eq!(header_value(Some(&o("none", None)), Some("Dema")), None);
        assert_eq!(header_value(None, Some("Dema")), None);
    }
}
