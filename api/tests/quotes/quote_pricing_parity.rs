//! Prices quotes and their commission the way the CRM's TypeScript does,
//! line by line. The cases come from the CRM (`PARITY_FILE`: JSON lines of
//! a doc, price list, options, rules and what TypeScript computed); run with
//! `PARITY_FILE=... cargo test -p api --test quote_pricing_parity -- --ignored`.

use api::commissions::{CommissionPart, CommissionRule, quote_commission};
use api::quote_pricing::doc::QuoteDoc;
use api::quote_pricing::pricing::{PriceOptions, price_quote};
use serde::Deserialize;
use serde_json::Value;
use std::io::BufRead;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Opts {
    discount_pct: f64,
    discount_amount: f64,
    tax_pct: f64,
}

#[derive(Deserialize)]
struct Line {
    key: String,
    description: String,
    unit: Option<String>,
    total: f64,
    note: Option<String>,
    #[serde(rename = "flatEach")]
    flat_each: Option<f64>,
}

#[derive(Deserialize)]
struct Area {
    name: String,
    lines: Vec<Line>,
}

#[derive(Deserialize)]
struct Option_ {
    total: f64,
    areas: Vec<Area>,
}

#[derive(Deserialize)]
struct Commission {
    total: f64,
    parts: Vec<CommissionPart>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Case {
    file: String,
    doc: Value,
    pl: Value,
    opts: Opts,
    rules: Vec<CommissionRule>,
    option_index: usize,
    prices: Vec<Option_>,
    commission: Commission,
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-6
}

#[test]
#[ignore = "needs PARITY_FILE from the CRM"]
fn pricing_and_commissions_match_the_crm() {
    let path = std::env::var("PARITY_FILE").expect("PARITY_FILE");
    let file = std::io::BufReader::new(std::fs::File::open(path).unwrap());
    let (mut cases, mut lines, mut problems) = (0, 0, Vec::new());
    for (n, row) in file.lines().enumerate() {
        let case: Case = serde_json::from_str(&row.unwrap()).unwrap();
        cases += 1;
        if QuoteDoc::parse(&case.doc).is_none() {
            let why = serde_json::from_value::<QuoteDoc>(case.doc.clone())
                .err()
                .map_or_else(|| "fails its checks".to_string(), |e| e.to_string());
            problems.push(format!("case {n} ({}): doc not read: {why}", case.file));
            continue;
        }
        let doc = QuoteDoc::from_stored(&case.doc);
        let opts = PriceOptions {
            discount_pct: case.opts.discount_pct,
            discount_amount: case.opts.discount_amount,
            tax_pct: case.opts.tax_pct,
        };
        let prices = price_quote(&doc, &case.pl, opts);
        let at = |what: &str| format!("case {n} ({}): {what}", case.file);
        if prices.len() != case.prices.len() {
            problems.push(at("option count"));
            continue;
        }
        for (ours, theirs) in prices.iter().zip(&case.prices) {
            if !close(ours.total, theirs.total) {
                problems.push(at(&format!("total {} vs {}", ours.total, theirs.total)));
            }
            let areas: Vec<_> = ours
                .areas
                .iter()
                .chain(std::iter::once(&ours.other))
                .collect();
            for (area, expected) in areas.iter().zip(&theirs.areas) {
                if area.name != expected.name {
                    problems.push(at(&format!("area {:?} vs {:?}", area.name, expected.name)));
                }
                if area.lines.len() != expected.lines.len() {
                    problems.push(at(&format!("{}: line count", expected.name)));
                    continue;
                }
                for (line, want) in area.lines.iter().zip(&expected.lines) {
                    lines += 1;
                    if line.key != want.key
                        || line.description != want.description
                        || line.unit != want.unit.as_deref()
                        || !close(line.total, want.total)
                        || line.note != want.note.as_deref()
                        || line.flat_each != want.flat_each
                    {
                        problems.push(at(&format!(
                            "{} {:?} {:?} {} vs {} {:?} {:?} {}",
                            line.key,
                            line.description,
                            line.unit,
                            line.total,
                            want.key,
                            want.description,
                            want.unit,
                            want.total
                        )));
                    }
                }
            }
        }
        let commission = quote_commission(&doc, &case.pl, &case.rules, case.option_index, &prices);
        if !close(commission.total, case.commission.total)
            || commission.parts.len() != case.commission.parts.len()
            || commission
                .parts
                .iter()
                .zip(&case.commission.parts)
                .any(|(a, b)| {
                    a.rule_id != b.rule_id
                        || a.note != b.note
                        || a.base_label != b.base_label
                        || !close(a.base, b.base)
                        || !close(a.amount, b.amount)
                })
        {
            problems.push(at(&format!(
                "commission {:?} vs {:?}",
                commission.parts, case.commission.parts
            )));
        }
    }
    eprintln!("{cases} quotes, {lines} lines, {} problems", problems.len());
    assert!(
        problems.is_empty(),
        "{}",
        problems
            .iter()
            .take(20)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}
