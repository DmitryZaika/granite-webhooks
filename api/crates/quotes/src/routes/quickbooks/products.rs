//! The product a new invoice's priced lines get, learned per company from its
//! own invoices, with nothing about any product written in code.
//!
//! Each scanned invoice (`qbo_invoice_scans`) keeps its product and its
//! lines' words. A draft gets the product whose invoices read most like it:
//! which words they have and which they lack (naive Bayes over those words),
//! weighed by how often the product is used, reps' saved choices included.
//! The admin page shows what was learned as readable rules ("has cabinets"),
//! how often each is right on the company's own invoices (each one judged
//! from all the others), and lets an admin turn a product off
//! (`qbo_invoice_products_off`) so it is never picked.

use super::rules::{Seen, header_history, scan};
use crate::auth::AdminUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::qbo::valid_id;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use sqlx::MySqlPool;
use std::collections::{HashMap, HashSet};

/// Words on fewer of the scanned invoices than this tell nothing about a
/// product.
const MIN_WORD_INVOICES: usize = 3;
/// Words a rule shows.
const SHOWN_HAS: usize = 4;
const SHOWN_LACKS: usize = 3;

/// One product as the company's invoices use it.
struct Product<'a> {
    value: &'a str,
    name: Option<&'a str>,
    /// Invoices with it: saved here (reps' choices) and scanned.
    used: usize,
    /// Scanned invoices with it, whose words are known.
    read: usize,
    /// On how many of those each word is.
    words: HashMap<&'a str, usize>,
}

/// What a company's invoices say about its products.
pub(super) struct Model<'a> {
    products: Vec<Product<'a>>,
    /// Scanned invoices whose words are known.
    read: usize,
    /// On how many of those each word is.
    on_invoices: HashMap<&'a str, usize>,
    /// The words that tell products apart (on [`MIN_WORD_INVOICES`] or more).
    vocabulary: Vec<&'a str>,
}

impl<'a> Model<'a> {
    /// Learned from a product history, newest first.
    pub(super) fn learn(history: &'a [Seen]) -> Self {
        Self::learn_from(history.iter())
    }

    fn learn_from(history: impl Iterator<Item = &'a Seen>) -> Self {
        let mut products: Vec<Product> = Vec::new();
        let mut on_invoices: HashMap<&str, usize> = HashMap::new();
        let mut read = 0;
        for seen in history.filter(|seen| seen.outcome == "value") {
            let Some(value) = seen.value.as_deref() else {
                continue;
            };
            let index = match products.iter().position(|p| p.value == value) {
                Some(index) => index,
                None => {
                    products.push(Product {
                        value,
                        name: None,
                        used: 0,
                        read: 0,
                        words: HashMap::new(),
                    });
                    products.len() - 1
                }
            };
            let product = &mut products[index];
            product.used += 1;
            product.name = product.name.or(seen.name.as_deref());
            if !seen.words.is_empty() {
                product.read += 1;
                read += 1;
                for word in &seen.words {
                    *product.words.entry(word.as_str()).or_default() += 1;
                    *on_invoices.entry(word.as_str()).or_default() += 1;
                }
            }
        }
        let mut vocabulary: Vec<&str> = on_invoices
            .iter()
            .filter(|(_, count)| **count >= MIN_WORD_INVOICES)
            .map(|(word, _)| *word)
            .collect();
        vocabulary.sort_unstable();
        Self {
            products,
            read,
            on_invoices,
            vocabulary,
        }
    }

    /// The share of a product's invoices with the word, and of the others'.
    #[allow(clippy::cast_precision_loss)]
    fn shares(&self, product: &Product, word: &str) -> (f64, f64) {
        let with = product.words.get(word).copied().unwrap_or(0);
        let all = self.on_invoices.get(word).copied().unwrap_or(0);
        (
            (with as f64 + 1.0) / (product.read as f64 + 2.0),
            ((all - with) as f64 + 1.0) / ((self.read - product.read) as f64 + 2.0),
        )
    }

    #[allow(clippy::cast_precision_loss)]
    fn score(&self, product: &Product, draft: &HashSet<&str>) -> f64 {
        let total = self.products.iter().map(|p| p.used).sum::<usize>() + self.products.len();
        let prior = (product.used as f64 + 1.0).ln() - (total as f64).ln();
        prior
            + self
                .vocabulary
                .iter()
                .map(|word| {
                    let (share, _) = self.shares(product, word);
                    if draft.contains(word) {
                        share.ln()
                    } else {
                        (1.0 - share).ln()
                    }
                })
                .sum::<f64>()
    }

    /// The product for a draft with these words, unless turned off; the
    /// newest wins a tie.
    pub(super) fn pick(
        &self,
        draft: &[String],
        off: &HashSet<String>,
    ) -> Option<(String, Option<String>)> {
        let draft: HashSet<&str> = draft.iter().map(String::as_str).collect();
        let winner = self
            .products
            .iter()
            .filter(|product| !off.contains(product.value))
            .map(|product| (product, self.score(product, &draft)))
            .fold(None, |best: Option<(&Product, f64)>, (product, s)| match best {
                Some(best) if best.1 >= s => Some(best),
                _ => Some((product, s)),
            })?
            .0;
        Some((winner.value.to_string(), winner.name.map(str::to_string)))
    }

    /// The words that most set a product's invoices apart: ones they have,
    /// ones they lack.
    fn explain(&self, product: &Product) -> (Vec<String>, Vec<String>) {
        if product.read == 0 || product.read == self.read {
            return (Vec::new(), Vec::new());
        }
        let mut has = Vec::new();
        let mut lacks = Vec::new();
        for word in &self.vocabulary {
            let (inside, outside) = self.shares(product, word);
            if inside >= 0.6 && inside - outside >= 0.3 {
                has.push((inside - outside, *word));
            } else if inside <= 0.4 && outside - inside >= 0.3 {
                lacks.push((outside - inside, *word));
            }
        }
        let top = |mut words: Vec<(f64, &str)>, n: usize| {
            words.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(b.1)));
            words.into_iter().take(n).map(|(_, w)| w.to_string()).collect()
        };
        (top(has, SHOWN_HAS), top(lacks, SHOWN_LACKS))
    }
}

/// The company's turned-off products; none when they cannot be read.
pub(super) async fn turned_off(pool: &MySqlPool, company_id: i32) -> HashSet<String> {
    sqlx::query_scalar("SELECT item_id FROM qbo_invoice_products_off WHERE company_id = ?")
        .bind(company_id)
        .fetch_all(pool)
        .await
        .unwrap_or_else(|err| {
            tracing::warn!(error = %err, company_id, "turned-off products not read");
            Vec::new()
        })
        .into_iter()
        .collect()
}

/// `GET /v1/qbo/invoice-rules`.
#[derive(Debug, Serialize, ToSchema)]
pub struct ProductRules {
    /// Scanned invoices the rules were learned from.
    pub invoices: usize,
    /// How many of them the rules pick the right product for, each judged
    /// from all the others.
    pub right: usize,
    pub products: Vec<ProductRule>,
}

/// What the company's invoices say about one product.
#[derive(Debug, Serialize, ToSchema)]
pub struct ProductRule {
    pub item_id: String,
    pub item_name: Option<String>,
    /// Scanned invoices with it.
    pub invoices: usize,
    /// How many of those the rules pick it for.
    pub right: usize,
    /// Words its invoices usually have and the others don't.
    pub has: Vec<String>,
    /// Words its invoices usually lack and the others have.
    pub lacks: Vec<String>,
    /// Turned off: never picked.
    pub off: bool,
}

/// Invoice product rules
///
/// What the company's own invoices teach about which product a new invoice
/// gets, and how often that is right on them.
#[crate::openapi::path(
    get,
    path = "/v1/qbo/invoice-rules",
    operation_id = "get_qbo_invoice_rules",
    tag = "quickbooks",
    responses(
        (status = 200, description = "The learned rules", body = ProductRules),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 409, description = "QuickBooks is not connected or could not be read", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn product_rules(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
) -> ApiResult<Json<ProductRules>> {
    let company_id = user.company_id;
    let scan = scan(&state, company_id).await?.ok_or_else(|| {
        ApiError::Conflict(
            "QuickBooks is not connected, or its invoices could not be read just now.".into(),
        )
    })?;
    let history = header_history(&state.pool, company_id, "item", &scan.item).await?;
    let off = turned_off(&state.pool, company_id).await;
    Ok(Json(rules_of(&history, &off)))
}

fn rules_of(history: &[Seen], off: &HashSet<String>) -> ProductRules {
    let model = Model::learn(history);
    let mut right: HashMap<&str, usize> = HashMap::new();
    for (i, seen) in history.iter().enumerate() {
        let Some(actual) = seen.value.as_deref() else {
            continue;
        };
        if seen.outcome != "value" || seen.words.is_empty() {
            continue;
        }
        let others = Model::learn_from(
            history
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, seen)| seen),
        );
        if others.pick(&seen.words, off).is_some_and(|(id, _)| id == actual) {
            *right.entry(actual).or_default() += 1;
        }
    }
    let mut products: Vec<ProductRule> = model
        .products
        .iter()
        .map(|product| {
            let (has, lacks) = model.explain(product);
            ProductRule {
                item_id: product.value.to_string(),
                item_name: product.name.map(str::to_string),
                invoices: product.read,
                right: right.get(product.value).copied().unwrap_or(0),
                has,
                lacks,
                off: off.contains(product.value),
            }
        })
        .collect();
    products.sort_by(|a, b| b.invoices.cmp(&a.invoices));
    ProductRules {
        invoices: model.read,
        right: right.values().sum(),
        products,
    }
}

/// Body of `PUT /v1/qbo/invoice-rules/{item_id}`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct TurnProduct {
    /// Never pick the product.
    pub off: bool,
}

/// Turn an invoice product on or off
///
/// A product turned off is never picked for a new invoice.
#[crate::openapi::path(
    put,
    path = "/v1/qbo/invoice-rules/{item_id}",
    operation_id = "turn_qbo_invoice_product",
    tag = "quickbooks",
    params(("item_id" = String, Path, description = "QuickBooks product (item) id")),
    request_body = TurnProduct,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Invalid product id", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn turn_product(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(item_id): Path<String>,
    JsonBody(input): JsonBody<TurnProduct>,
) -> ApiResult<StatusCode> {
    if !valid_id(&item_id) {
        return Err(ApiError::BadRequest(format!(
            "\"{item_id}\" is not a QuickBooks product id."
        )));
    }
    let sql = if input.off {
        "INSERT IGNORE INTO qbo_invoice_products_off (company_id, item_id) VALUES (?, ?)"
    } else {
        "DELETE FROM qbo_invoice_products_off WHERE company_id = ? AND item_id = ?"
    };
    sqlx::query(sql)
        .bind(user.company_id)
        .bind(&item_id)
        .execute(&state.pool)
        .await?;
    Ok(StatusCode::NO_CONTENT)
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
        // Turned off, it is never picked.
        let off = HashSet::from(["30".to_string()]);
        assert_ne!(model.pick(&draft(&PACKAGE), &off).unwrap().0, "30");
        // Saved choices only (no scanned words): the most used.
        let saved: Vec<Seen> = history
            .iter()
            .map(|seen| Seen {
                words: Vec::new(),
                ..seen.clone()
            })
            .collect();
        assert_eq!(
            Model::learn(&saved).pick(&draft(&["Fabrication"]), &none).unwrap().0,
            "30"
        );
        assert_eq!(Model::learn(&[]).pick(&draft(&["Fabrication"]), &none), None);
    }

    #[test]
    fn rules_read_as_the_words_that_set_a_product_apart() {
        let rules = rules_of(&history(), &HashSet::new());
        assert_eq!(rules.invoices, 11);
        let package = &rules.products[0];
        assert_eq!(package.item_name.as_deref(), Some("Package $11999"));
        assert_eq!(package.invoices, 6);
        assert_eq!(package.right, 6);
        assert!(package.has.contains(&"cabinets".to_string()));
        assert!(package.lacks.contains(&"fabrication".to_string()));
        let sales = &rules.products[1];
        assert!(sales.has.contains(&"fabrication".to_string()));
        assert!(sales.lacks.contains(&"cabinets".to_string()));
        assert_eq!(rules.right, 6 + sales.right);
    }
}
