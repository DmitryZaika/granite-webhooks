//! The product a new invoice's priced lines get, learned per company from its
//! own invoices, with nothing about any product written in code.
//!
//! Two kinds of examples, each a product with the words of its lines:
//! invoices reps created here (the quote's words and the product they saved,
//! [`PAIR_WEIGHT`] times as telling, since a rep chose it for that quote), and
//! the company's newest `QuickBooks` invoices (`qbo_invoice_scans`), the newer
//! the more telling ([`HALF_LIFE`]). A draft gets the product whose examples
//! read most like it: which words they have and which they lack (naive Bayes
//! over those words), weighed by how much the product is used. So a company
//! keeps getting what it keeps doing, and a few corrections turn it.

use super::rules::Seen;
use std::collections::{HashMap, HashSet};

/// Words on fewer examples than this tell nothing about a product.
const MIN_WORD_INVOICES: usize = 3;
/// How much more an invoice a rep created from a quote counts than a scanned one.
const PAIR_WEIGHT: f64 = 3.0;
/// Scanned invoices (newest first) count half as much every this many.
const HALF_LIFE: f64 = 100.0;

/// One product as the company's invoices use it.
struct Product<'a> {
    value: &'a str,
    name: Option<&'a str>,
    /// Its examples, weighted.
    used: f64,
    /// Those whose words are known.
    read: f64,
    /// Their weight on which each word is.
    words: HashMap<&'a str, f64>,
}

/// What a company's invoices say about its products.
pub(super) struct Model<'a> {
    products: Vec<Product<'a>>,
    /// The words that tell products apart (on [`MIN_WORD_INVOICES`] or more).
    vocabulary: Vec<&'a str>,
}

impl<'a> Model<'a> {
    /// Learned from a product history: invoices created here, then scanned
    /// ones, each newest first.
    pub(super) fn learn(history: &'a [Seen]) -> Self {
        let mut products: Vec<Product> = Vec::new();
        let mut examples_with: HashMap<&str, usize> = HashMap::new();
        let mut scanned = 0.0;
        for seen in history.iter().filter(|seen| seen.outcome == "value") {
            let Some(value) = seen.value.as_deref() else {
                continue;
            };
            let weight = if seen.pair {
                PAIR_WEIGHT
            } else {
                scanned += 1.0;
                0.5_f64.powf((scanned - 1.0) / HALF_LIFE)
            };
            let index = match products.iter().position(|p| p.value == value) {
                Some(index) => index,
                None => {
                    products.push(Product {
                        value,
                        name: None,
                        used: 0.0,
                        read: 0.0,
                        words: HashMap::new(),
                    });
                    products.len() - 1
                }
            };
            let product = &mut products[index];
            product.used += weight;
            product.name = product.name.or(seen.name.as_deref());
            if !seen.words.is_empty() {
                product.read += weight;
                for word in &seen.words {
                    *product.words.entry(word.as_str()).or_default() += weight;
                    *examples_with.entry(word.as_str()).or_default() += 1;
                }
            }
        }
        let mut vocabulary: Vec<&str> = examples_with
            .into_iter()
            .filter(|(_, count)| *count >= MIN_WORD_INVOICES)
            .map(|(word, _)| word)
            .collect();
        vocabulary.sort_unstable();
        Self {
            products,
            vocabulary,
        }
    }

    /// The share of a product's examples with the word.
    fn share(product: &Product, word: &str) -> f64 {
        let with = product.words.get(word).copied().unwrap_or(0.0);
        (with + 1.0) / (product.read + 2.0)
    }

    fn score(&self, product: &Product, draft: &HashSet<&str>) -> f64 {
        let total = self.products.iter().map(|p| p.used + 1.0).sum::<f64>();
        let prior = (product.used + 1.0).ln() - total.ln();
        prior
            + self
                .vocabulary
                .iter()
                .map(|word| {
                    let share = Self::share(product, word);
                    if draft.contains(word) {
                        share.ln()
                    } else {
                        (1.0 - share).ln()
                    }
                })
                .sum::<f64>()
    }

    /// The product for a draft with these words, but none of `avoid`; the
    /// newest wins a tie.
    pub(super) fn pick(
        &self,
        draft: &[String],
        avoid: &HashSet<String>,
    ) -> Option<(String, Option<String>)> {
        let draft: HashSet<&str> = draft.iter().map(String::as_str).collect();
        let winner = self
            .products
            .iter()
            .filter(|product| !avoid.contains(product.value))
            .map(|product| (product, self.score(product, &draft)))
            .fold(
                None,
                |best: Option<(&Product, f64)>, (product, s)| match best {
                    Some(best) if best.1 >= s => Some(best),
                    _ => Some((product, s)),
                },
            )?
            .0;
        Some((winner.value.to_string(), winner.name.map(str::to_string)))
    }
}

#[cfg(test)]
mod tests {
    use super::super::rules::words;
    use super::*;

    fn item(value: &str, name: &str, lines: &[&str]) -> Seen {
        Seen {
            outcome: "value".into(),
            value: Some(value.into()),
            name: Some(name.into()),
            words: words(lines.iter().copied()),
            pair: false,
        }
    }

    const PACKAGE: [&str; 4] = [
        "Kitchen based @ 70 sqft",
        "Granite Black Pearl",
        "12 Shaker Cabinets - White",
        "Installation",
    ];
    const COUNTERS: [&str; 3] = ["Granite Black Pearl", "Fabrication", "Installation"];

    /// The package is the most used, but only ever with cabinets.
    fn history() -> Vec<Seen> {
        let mut history: Vec<Seen> = (0..6)
            .map(|_| item("30", "Package $11999", &PACKAGE))
            .collect();
        history.extend((0..4).map(|_| item("5", "Sales", &COUNTERS)));
        history.push(item("9", "Square Footage", &COUNTERS));
        history
    }

    fn draft(lines: &[&str]) -> Vec<String> {
        words(lines.iter().copied())
    }

    #[test]
    fn the_product_follows_what_the_companys_invoices_with_such_lines_use() {
        let history = history();
        let none = HashSet::new();
        let model = Model::learn(&history);
        assert_eq!(
            model.pick(&draft(&COUNTERS), &none),
            Some(("5".into(), Some("Sales".into())))
        );
        assert_eq!(
            model.pick(&draft(&PACKAGE), &none),
            Some(("30".into(), Some("Package $11999".into())))
        );
        // Avoided, it is never picked.
        let avoid = HashSet::from(["30".to_string()]);
        assert_ne!(model.pick(&draft(&PACKAGE), &avoid).unwrap().0, "30");
        // Choices without words: the most used.
        let saved: Vec<Seen> = history
            .iter()
            .map(|seen| Seen {
                words: Vec::new(),
                ..seen.clone()
            })
            .collect();
        assert_eq!(
            Model::learn(&saved)
                .pick(&draft(&["Fabrication"]), &none)
                .unwrap()
                .0,
            "30"
        );
        assert_eq!(
            Model::learn(&[]).pick(&draft(&["Fabrication"]), &none),
            None
        );
    }

    #[test]
    fn a_company_keeps_its_habit_until_reps_correct_it() {
        // Every recent QuickBooks invoice got the package, cabinets or not.
        let mut scanned: Vec<Seen> = (0..60)
            .map(|_| item("30", "Package $11999", &COUNTERS))
            .collect();
        scanned.extend((0..10).map(|_| item("30", "Package $11999", &PACKAGE)));
        scanned.extend((0..30).map(|_| item("5", "Sales", &COUNTERS)));
        let none = HashSet::new();
        assert_eq!(
            Model::learn(&scanned)
                .pick(&draft(&COUNTERS), &none)
                .unwrap()
                .0,
            "30"
        );
        // Reps pick Sales for countertop-only quotes: a few such choices turn it.
        let mut history: Vec<Seen> = (0..8)
            .map(|_| Seen {
                pair: true,
                ..item("5", "Sales", &COUNTERS)
            })
            .collect();
        history.extend(scanned.iter().cloned());
        let model = Model::learn(&history);
        assert_eq!(model.pick(&draft(&COUNTERS), &none).unwrap().0, "5");
        assert_eq!(model.pick(&draft(&PACKAGE), &none).unwrap().0, "30");
    }

    #[test]
    fn newer_invoices_count_more() {
        // The company moved from Sales to the package: the newest habit wins.
        let mut history: Vec<Seen> = (0..50)
            .map(|_| item("30", "Package $11999", &COUNTERS))
            .collect();
        history.extend((0..70).map(|_| item("5", "Sales", &COUNTERS)));
        assert_eq!(
            Model::learn(&history)
                .pick(&draft(&COUNTERS), &HashSet::new())
                .unwrap()
                .0,
            "30"
        );
    }
}
