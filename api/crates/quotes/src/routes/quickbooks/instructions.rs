//! Invoice instructions: what an admin tells the AI about the company's
//! invoices ("Package $11999 only when the quote has cabinets, otherwise
//! Sales"), turned once, when saved, into exact rules that every new invoice
//! draft then follows, with no AI per invoice. The rules come back to the
//! admin as what the AI understood, with what it could not turn into a rule.
//!
//! A rule holds on a quote that has all its `when_has` phrases and none of
//! its `when_lacks` (a phrase matches at the start of a word, any case:
//! "cabinet" matches "Cabinets"). Instructions win over what is learned from
//! the company's invoices; like learned rules, they never change an amount
//! or leave out a priced line.

use super::QboItem;
use super::invoice::Line;
use crate::ai::{Caller, Chat, MINI_MODEL};
use crate::auth::AdminUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use lambda_http::tracing;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::collections::HashSet;

/// Longest instructions, in characters.
const MAX_CHARS: usize = 4000;
const MAX_RULES: usize = 40;
const MAX_PHRASES: usize = 6;
const MAX_PHRASE_CHARS: usize = 80;
/// Recent draft lines shown to the AI, so its phrases match the company's.
const LINE_EXAMPLES: u32 = 60;

/// What a rule does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    /// The priced lines get this product.
    Product { item_id: String, item_name: String },
    /// The priced lines never get this product (another one is learned).
    AvoidProduct { item_id: String, item_name: String },
    /// Text lines with the phrase are left out (the editor offers them back).
    Hide { line: String },
    /// The phrase in a line becomes `to`.
    Rename { line: String, to: String },
    /// A text line added at the end.
    AddLine { text: String },
    /// The invoice's class.
    Class { name: String },
    /// The invoice's CC.
    Cc { email: String },
}

/// One rule the AI made of the instructions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct InstructionRule {
    /// Phrases the quote must all have.
    #[serde(default)]
    pub when_has: Vec<String>,
    /// Phrases the quote must not have.
    #[serde(default)]
    pub when_lacks: Vec<String>,
    pub action: Action,
}

/// `GET`/`PUT /v1/qbo/invoice-instructions`.
#[derive(Debug, Default, Clone, Serialize, Deserialize, ToSchema)]
pub struct InvoiceInstructions {
    /// As the admin wrote them.
    #[serde(default)]
    pub instructions: String,
    /// The rules made of them, in order.
    #[serde(default)]
    pub rules: Vec<InstructionRule>,
    /// What could not be made into a rule, and why.
    #[serde(default)]
    pub not_understood: Vec<String>,
}

/// Body of `PUT /v1/qbo/invoice-instructions`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct InstructionsInput {
    /// Plain words; empty removes them.
    pub instructions: String,
}

const SYSTEM_PROMPT: &str = r#"You turn a countertop company's instructions about its QuickBooks invoices into exact rules.
Each invoice is made from a quote; its lines read like "Kitchen based @ 73.3 sqft Additional sqft of Black Pearl @ $59/sqft", "73.3 sq ft Fabrication", "12 Shaker Cabinets - White", "48.3 lin ft Finished Edge - Eased", "1 Sink - Kohler K-8199 (50/50)".
A rule holds on a quote whose lines have ALL its "when_has" phrases and NONE of its "when_lacks" phrases (both empty: every quote). A phrase matches at the start of a word in any case, so "cabinet" also matches "Cabinets"; prefer short singular phrases ("cabinet", "black pearl", "eased").

Return ONLY JSON: {"rules": [...], "not_understood": [...]}
Each rule: {"when_has": [...], "when_lacks": [...], "action": ACTION}
ACTION is one of:
- {"type": "product", "product": "<exact name from products>"}: the priced lines get this product.
- {"type": "avoid_product", "product": "<exact name from products>"}: the priced lines must not get this product.
- {"type": "hide", "line": "<phrase>"}: text lines containing the phrase are left out of the invoice.
- {"type": "rename", "line": "<phrase>", "to": "<new text>"}: the phrase is replaced in the lines that have it.
- {"type": "add_line", "text": "<text>"}: a text line is added at the end of the invoice.
- {"type": "class", "class": "<exact name from classes>"}
- {"type": "cc", "email": "<email address>"}

Rules are checked in order and, for product, class and cc, the first that holds wins: put specific rules before general ones.
"X only when Y" means two rules: product X when_has Y, and avoid_product X when_lacks Y. When the instructions say what to use otherwise, add that rule with when_lacks Y.
Never change prices, quantities, amounts or discounts, never leave out a priced line, and never invent a product or class that is not listed. Anything you cannot express with these actions goes into "not_understood" as one short sentence quoting the instruction and saying why.
Use the phrases of "line_examples" when they fit."#;

/// The rules a company's invoices follow; none when they cannot be read.
pub(super) async fn load(pool: &MySqlPool, company_id: i32) -> Vec<InstructionRule> {
    match stored(pool, company_id).await {
        Ok(stored) => stored.rules,
        Err(err) => {
            tracing::warn!(error = %err, company_id, "invoice instructions not read");
            Vec::new()
        }
    }
}

async fn stored(pool: &MySqlPool, company_id: i32) -> sqlx::Result<InvoiceInstructions> {
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT instructions, CAST(rules_json AS CHAR) FROM qbo_invoice_instructions
          WHERE company_id = ?",
    )
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    Ok(row
        .map(|(instructions, json)| InvoiceInstructions {
            instructions,
            ..serde_json::from_str(&json).unwrap_or_default()
        })
        .unwrap_or_default())
}

/// A phrase at the start of a word, any case; group 1 is what precedes it.
fn phrase_regex(phrase: &str) -> Option<Regex> {
    let phrase = phrase.trim();
    if phrase.is_empty() {
        return None;
    }
    Regex::new(&format!(r"(?i)(^|[^\pL\pN])({})", regex::escape(phrase))).ok()
}

fn has_phrase(texts: &[String], phrase: &str) -> bool {
    phrase_regex(phrase).is_some_and(|re| texts.iter().any(|text| re.is_match(text)))
}

/// The rules that hold on a quote with these line texts, in order.
pub(super) fn holding<'a>(rules: &'a [InstructionRule], texts: &[String]) -> Vec<&'a InstructionRule> {
    rules
        .iter()
        .filter(|rule| {
            rule.when_has.iter().all(|phrase| has_phrase(texts, phrase))
                && !rule.when_lacks.iter().any(|phrase| has_phrase(texts, phrase))
        })
        .collect()
}

/// The product the holding rules give, with its name.
pub(super) fn product(holding: &[&InstructionRule]) -> Option<(String, Option<String>)> {
    let avoid = avoided(holding);
    holding.iter().find_map(|rule| match &rule.action {
        Action::Product { item_id, item_name } if !avoid.contains(item_id) => {
            Some((item_id.clone(), Some(item_name.clone())))
        }
        _ => None,
    })
}

/// The products the holding rules rule out.
pub(super) fn avoided(holding: &[&InstructionRule]) -> HashSet<String> {
    holding
        .iter()
        .filter_map(|rule| match &rule.action {
            Action::AvoidProduct { item_id, .. } => Some(item_id.clone()),
            _ => None,
        })
        .collect()
}

pub(super) fn class(holding: &[&InstructionRule]) -> Option<String> {
    holding.iter().find_map(|rule| match &rule.action {
        Action::Class { name } => Some(name.clone()),
        _ => None,
    })
}

pub(super) fn cc(holding: &[&InstructionRule]) -> Option<String> {
    holding.iter().find_map(|rule| match &rule.action {
        Action::Cc { email } => Some(email.clone()),
        _ => None,
    })
}

/// The holding rules' line changes: phrases renamed, text lines left out
/// (into `removed`), text lines added.
pub(super) fn apply_lines(holding: &[&InstructionRule], lines: &mut Vec<Line>, removed: &mut Vec<Line>) {
    for rule in holding {
        match &rule.action {
            Action::Rename { line: phrase, to } => {
                let Some(re) = phrase_regex(phrase) else {
                    continue;
                };
                for line in lines.iter_mut() {
                    let Some(text) = line.description.as_deref() else {
                        continue;
                    };
                    let renamed = re
                        .replace_all(text, |caps: &regex::Captures| format!("{}{to}", &caps[1]))
                        .into_owned();
                    if renamed != text && !renamed.trim().is_empty() {
                        line.description = Some(renamed);
                    }
                }
            }
            Action::Hide { line: phrase } => {
                let Some(re) = phrase_regex(phrase) else {
                    continue;
                };
                let (hidden, kept): (Vec<Line>, Vec<Line>) =
                    std::mem::take(lines).into_iter().partition(|line| {
                        line.detail_type == "DescriptionOnly"
                            && line.description.as_deref().is_some_and(|text| re.is_match(text))
                    });
                *lines = kept;
                removed.extend(hidden);
            }
            Action::AddLine { text } => {
                let id = format!("new:told:{}", lines.len() + removed.len());
                lines.push(Line {
                    id: Some(id),
                    line_num: None,
                    detail_type: "DescriptionOnly".into(),
                    description: Some(text.clone()),
                    amount: None,
                    qty: None,
                    unit_price: None,
                    item_ref_value: None,
                    item_ref_name: None,
                    editable: true,
                });
            }
            _ => {}
        }
    }
}

fn clean_phrase(value: &Value) -> Option<String> {
    let phrase = value.as_str()?.split_whitespace().collect::<Vec<_>>().join(" ");
    (!phrase.is_empty() && phrase.chars().count() <= MAX_PHRASE_CHARS).then(|| phrase.to_lowercase())
}

fn phrases(value: &Value) -> Vec<String> {
    let mut list: Vec<String> = value
        .as_array()
        .map(|list| list.iter().filter_map(clean_phrase).collect())
        .unwrap_or_default();
    list.dedup();
    list.truncate(MAX_PHRASES);
    list
}

fn text_of(value: &Value, max: usize) -> Option<String> {
    let text = value.as_str()?.trim();
    (!text.is_empty() && text.chars().count() <= max).then(|| text.to_string())
}

/// The rules in the AI's reply, checked against the company's products and
/// classes; what fails a check is said in `not_understood`.
pub(super) fn compile(reply: &Value, items: &[QboItem], classes: &[String]) -> (Vec<InstructionRule>, Vec<String>) {
    let mut not_understood: Vec<String> = reply["not_understood"]
        .as_array()
        .map(|list| list.iter().filter_map(|v| text_of(v, 300)).take(10).collect())
        .unwrap_or_default();
    let item = |name: &str| {
        items
            .iter()
            .find(|item| item.name.trim().eq_ignore_ascii_case(name.trim()))
    };
    let mut rules = Vec::new();
    for raw in reply["rules"].as_array().into_iter().flatten().take(MAX_RULES) {
        let action = &raw["action"];
        let named = |key: &str| action[key].as_str().unwrap_or_default().trim().to_string();
        let action = match action["type"].as_str().unwrap_or_default() {
            kind @ ("product" | "avoid_product") => {
                let name = named("product");
                let Some(found) = item(&name) else {
                    not_understood.push(format!("QuickBooks has no product named “{name}”."));
                    continue;
                };
                let (item_id, item_name) = (found.id.clone(), found.name.clone());
                if kind == "product" {
                    Action::Product { item_id, item_name }
                } else {
                    Action::AvoidProduct { item_id, item_name }
                }
            }
            "hide" => match clean_phrase(&action["line"]) {
                Some(line) => Action::Hide { line },
                None => continue,
            },
            "rename" => match (clean_phrase(&action["line"]), text_of(&action["to"], 300)) {
                (Some(line), Some(to)) => Action::Rename { line, to },
                _ => continue,
            },
            "add_line" => match text_of(&action["text"], 1000) {
                Some(text) => Action::AddLine { text },
                None => continue,
            },
            "class" => {
                let name = named("class");
                match classes.iter().find(|class| class.eq_ignore_ascii_case(&name)) {
                    Some(class) => Action::Class { name: class.clone() },
                    None => {
                        not_understood.push(format!("QuickBooks has no class named “{name}”."));
                        continue;
                    }
                }
            }
            "cc" => {
                let email = named("email");
                if email.contains('@') && !email.contains(char::is_whitespace) {
                    Action::Cc { email }
                } else {
                    not_understood.push(format!("“{email}” is not an email address."));
                    continue;
                }
            }
            other => {
                not_understood.push(format!("“{other}” can't be done on invoices yet."));
                continue;
            }
        };
        rules.push(InstructionRule {
            when_has: phrases(&raw["when_has"]),
            when_lacks: phrases(&raw["when_lacks"]),
            action,
        });
    }
    (rules, not_understood)
}

/// Recent draft lines of the company's invoices, newest first.
async fn line_examples(pool: &MySqlPool, company_id: i32) -> sqlx::Result<Vec<String>> {
    sqlx::query_scalar(
        "SELECT draft_text FROM qbo_invoice_draft_outcomes
          WHERE company_id = ? AND field = 'line' AND draft_text IS NOT NULL
          GROUP BY draft_text ORDER BY MAX(id) DESC LIMIT ?",
    )
    .bind(company_id)
    .bind(LINE_EXAMPLES)
    .fetch_all(pool)
    .await
}

/// Read the invoice instructions
///
/// What the admin told the AI about the company's invoices and the rules it
/// made of them.
#[crate::openapi::path(
    get,
    path = "/v1/qbo/invoice-instructions",
    operation_id = "get_qbo_invoice_instructions",
    tag = "quickbooks",
    responses(
        (status = 200, description = "The instructions and their rules", body = InvoiceInstructions),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get_instructions(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
) -> ApiResult<Json<InvoiceInstructions>> {
    Ok(Json(stored(&state.pool, user.company_id).await?))
}

/// Save the invoice instructions
///
/// The AI turns them into exact rules (checked against the company's
/// `QuickBooks` products and classes) that every new invoice draft follows.
/// Empty instructions remove them.
#[crate::openapi::path(
    put,
    path = "/v1/qbo/invoice-instructions",
    operation_id = "save_qbo_invoice_instructions",
    tag = "quickbooks",
    request_body = InstructionsInput,
    responses(
        (status = 200, description = "Saved, with the rules made of them", body = InvoiceInstructions),
        (status = 400, description = "Too long", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 409, description = "QuickBooks is not connected", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
        (status = 502, description = "QuickBooks or the AI failed; nothing was saved", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn save_instructions(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<InstructionsInput>,
) -> ApiResult<Json<InvoiceInstructions>> {
    let company_id = user.company_id;
    let text = input.instructions.trim();
    if text.chars().count() > MAX_CHARS {
        return Err(ApiError::BadRequest(format!(
            "Instructions can be at most {MAX_CHARS} characters long."
        )));
    }
    if text.is_empty() {
        sqlx::query("DELETE FROM qbo_invoice_instructions WHERE company_id = ?")
            .bind(company_id)
            .execute(&state.pool)
            .await?;
        return Ok(Json(InvoiceInstructions::default()));
    }
    let catalog = super::company_items(&state, company_id).await?;
    let prompt = json!({
        "instructions": text,
        "products": catalog.items.iter().map(|item| &item.name).collect::<Vec<_>>(),
        "classes": catalog.classes,
        "line_examples": line_examples(&state.pool, company_id).await?,
    });
    let mut chat = Chat::new(MINI_MODEL)
        .message("system", SYSTEM_PROMPT)
        .message("user", &prompt.to_string());
    chat.json = true;
    let caller = Caller {
        feature: "qbo_invoice_instructions",
        user_id: Some(user.id),
        company_id: Some(company_id),
    };
    let reply = state
        .ai
        .chat(&state.pool, caller, &chat)
        .await
        .map_err(|err| {
            ApiError::Upstream(format!(
                "The AI could not read the instructions just now ({err}). Nothing was saved; try again."
            ))
        })?;
    let reply: Value = serde_json::from_str(reply.trim()).map_err(|_| {
        ApiError::Upstream(
            "The AI answered with something other than rules. Nothing was saved; try again.".into(),
        )
    })?;
    let (rules, not_understood) = compile(&reply, &catalog.items, &catalog.classes);
    let saved = InvoiceInstructions {
        instructions: text.to_string(),
        rules,
        not_understood,
    };
    let rules_json = json!({ "rules": saved.rules, "not_understood": saved.not_understood });
    sqlx::query(
        "INSERT INTO qbo_invoice_instructions (company_id, instructions, rules_json, updated_by)
         VALUES (?, ?, ?, ?)
         ON DUPLICATE KEY UPDATE instructions = VALUES(instructions),
                                 rules_json = VALUES(rules_json), updated_by = VALUES(updated_by)",
    )
    .bind(company_id)
    .bind(&saved.instructions)
    .bind(rules_json.to_string())
    .bind(user.id)
    .execute(&state.pool)
    .await?;
    Ok(Json(saved))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(id: &str, kind: &str, text: &str) -> Line {
        Line {
            id: Some(id.into()),
            line_num: None,
            detail_type: kind.into(),
            description: Some(text.into()),
            amount: (kind == "SalesItemLineDetail").then_some(100.0),
            qty: None,
            unit_price: None,
            item_ref_value: None,
            item_ref_name: None,
            editable: true,
        }
    }

    fn items() -> Vec<QboItem> {
        [("30", "Package $11999"), ("5", "Sales")]
            .into_iter()
            .map(|(id, name)| QboItem {
                id: id.into(),
                name: name.into(),
            })
            .collect()
    }

    /// "Package $11999 only with cabinets, otherwise Sales; no fabrication
    /// line with cabinets; call the template a laser template; CC the office."
    fn reply() -> Value {
        json!({ "rules": [
            { "when_has": ["Cabinet"], "action": { "type": "product", "product": "package $11999" } },
            { "when_lacks": ["cabinet"], "action": { "type": "avoid_product", "product": "Package $11999" } },
            { "when_lacks": ["cabinet"], "action": { "type": "product", "product": "Sales" } },
            { "when_has": ["cabinet"], "action": { "type": "hide", "line": "fabrication" } },
            { "action": { "type": "rename", "line": "template", "to": "Laser template" } },
            { "action": { "type": "add_line", "text": "Balance due at installation" } },
            { "action": { "type": "cc", "email": "office@example.com" } },
            { "action": { "type": "product", "product": "Granite Package" } },
            { "action": { "type": "class", "class": "Nobody" } },
            { "action": { "type": "discount", "percent": 5 } },
        ], "not_understood": ["“Give builders 5% off”: prices are never changed."] })
    }

    #[test]
    fn the_ai_reply_becomes_checked_rules() {
        let (rules, not_understood) = compile(&reply(), &items(), &["Dema".into()]);
        assert_eq!(rules.len(), 7);
        assert_eq!(rules[0].when_has, ["cabinet"]);
        assert_eq!(
            rules[0].action,
            Action::Product {
                item_id: "30".into(),
                item_name: "Package $11999".into()
            }
        );
        assert_eq!(
            not_understood,
            [
                "“Give builders 5% off”: prices are never changed.",
                "QuickBooks has no product named “Granite Package”.",
                "QuickBooks has no class named “Nobody”.",
                "“discount” can't be done on invoices yet.",
            ]
        );
    }

    fn draft(with_cabinets: bool) -> Vec<Line> {
        let mut lines = vec![
            line("a", "SalesItemLineDetail", "Kitchen based @ 73.3 sqft"),
            line("b", "DescriptionOnly", "73.3 sq ft Fabrication"),
            line("c", "DescriptionOnly", "Template"),
        ];
        if with_cabinets {
            lines.push(line("d", "DescriptionOnly", "12 Shaker Cabinets - White"));
        }
        lines
    }

    fn texts(lines: &[Line]) -> Vec<String> {
        lines.iter().filter_map(|l| l.description.clone()).collect()
    }

    #[test]
    fn rules_hold_on_the_quotes_they_name() {
        let (rules, _) = compile(&reply(), &items(), &[]);

        let cabinets = draft(true);
        let holding_rules = holding(&rules, &texts(&cabinets));
        assert_eq!(product(&holding_rules).unwrap().0, "30");
        assert!(avoided(&holding_rules).is_empty());
        assert_eq!(cc(&holding_rules).as_deref(), Some("office@example.com"));
        let (mut lines, mut removed) = (cabinets, Vec::new());
        apply_lines(&holding_rules, &mut lines, &mut removed);
        assert_eq!(
            texts(&lines),
            [
                "Kitchen based @ 73.3 sqft",
                "Laser template",
                "12 Shaker Cabinets - White",
                "Balance due at installation",
            ]
        );
        assert_eq!(texts(&removed), ["73.3 sq ft Fabrication"]);

        let counters = draft(false);
        let holding_rules = holding(&rules, &texts(&counters));
        assert_eq!(product(&holding_rules).unwrap().0, "5");
        assert_eq!(avoided(&holding_rules), HashSet::from(["30".to_string()]));
        let (mut lines, mut removed) = (counters, Vec::new());
        apply_lines(&holding_rules, &mut lines, &mut removed);
        assert!(removed.is_empty());
        assert_eq!(lines[1].description.as_deref(), Some("73.3 sq ft Fabrication"));
    }

    #[test]
    fn a_priced_line_is_never_left_out_and_phrases_start_words() {
        let rules = vec![
            InstructionRule {
                when_has: vec![],
                when_lacks: vec![],
                action: Action::Hide { line: "kitchen".into() },
            },
            InstructionRule {
                when_has: vec![],
                when_lacks: vec![],
                action: Action::Rename {
                    line: "edge".into(),
                    to: "Edge profile".into(),
                },
            },
        ];
        let mut lines = vec![
            line("a", "SalesItemLineDetail", "Kitchen based @ 30 sqft"),
            line("b", "DescriptionOnly", "Ledge support"),
            line("c", "DescriptionOnly", "48 lin ft Finished Edge - Flat"),
        ];
        let mut removed = Vec::new();
        let holding_rules = holding(&rules, &texts(&lines));
        apply_lines(&holding_rules, &mut lines, &mut removed);
        assert!(removed.is_empty());
        assert_eq!(
            texts(&lines),
            [
                "Kitchen based @ 30 sqft",
                "Ledge support",
                "48 lin ft Finished Edge profile - Flat",
            ]
        );
    }
}
