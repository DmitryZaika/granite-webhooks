//! `POST /v1/ai/invoice-style`: a quote's invoice draft written the way the
//! company writes its invoices.
//!
//! The model sees the company's latest `QuickBooks` invoices and the changes
//! reps made to earlier drafts (`qbo_invoice_draft_edits`), and rewrites the
//! draft's wording, products and grouping. It never writes an amount: each
//! priced line it returns names the draft's priced lines it stands for, and
//! their amounts are added up here, so the invoice keeps the quote's money.
//! Anything off, and the draft comes back as it was.

use crate::ai::{Caller, Chat, MINI_MODEL};
use crate::auth::EmployeeUser;
use crate::error::{ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::routes::quickbooks::invoice::{EditableInvoice, Line, editable};
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::collections::{HashMap, HashSet};

/// Latest invoices shown as examples.
const EXAMPLES: u32 = 10;
/// Rep edits read for patterns.
const EDITS_READ: u32 = 300;
/// One-off edits passed as hints.
const HINTS: usize = 15;

const SYSTEM_PROMPT: &str = r#"You write invoice lines for a countertop and cabinet company, in the company's own style.
You get:
- "examples": the company's latest invoices, as they were sent.
- "rules": changes reps made to earlier drafts again and again ("count" times). Always apply them where they fit.
- "hints": changes reps made once. Apply one only when the examples agree with it.
- "items": the products/services a priced line may use.
- "draft": the invoice to rewrite. Priced lines ("sale") have an "id"; text lines ("text") are the notes under them.

Return JSON {"lines": [...]} in invoice order. Each line is either
{"type": "sale", "from": ["<draft sale ids>"], "item": "<item id>", "description": "..."}
or {"type": "text", "description": "..."}.

- Every draft sale id appears in exactly one "from". Put several on one line only when the examples bill such things on one line. Never write amounts.
- Word the descriptions the way the examples do: how the main line reads, capitalization, which details get listed. Keep every number, size and price in the draft exactly. Never invent sizes, prices, products, sinks, sealers or services the draft does not have.
- Text lines may be reworded, shortened, or left out when the examples leave such details out. Add no detail the draft lacks.
- Pick "item" from "items" the way the examples do; otherwise keep the draft's item.
- A discount (negative) sale line stays its own line."#;

/// Body of `POST /v1/ai/invoice-style`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct InvoiceStyleInput {
    /// The draft the editor would open with.
    pub invoice: EditableInvoice,
}

/// `POST /v1/ai/invoice-style`.
#[derive(Debug, Serialize, ToSchema)]
pub struct InvoiceStyle {
    /// The draft in the company's style, or as it was.
    pub invoice: EditableInvoice,
    /// False when the draft came back as it was (no examples, or the model
    /// failed or answered something unusable).
    pub styled: bool,
}

/// A line of the model's answer.
#[derive(Debug, Deserialize)]
struct Proposed {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    from: Vec<String>,
    item: Option<String>,
    description: Option<String>,
}

fn is_sale(line: &Line) -> bool {
    line.detail_type == "SalesItemLineDetail"
}

fn editable_kind(line: &Line) -> bool {
    matches!(
        line.detail_type.as_str(),
        "SalesItemLineDetail" | "DescriptionOnly"
    )
}

fn cents(amount: f64) -> f64 {
    (amount * 100.0).round() / 100.0
}

/// An invoice as the model reads it.
fn compact(lines: &[Line], sale_ids: bool) -> Vec<Value> {
    let mut sale = 0;
    lines
        .iter()
        .filter(|line| editable_kind(line))
        .map(|line| {
            if is_sale(line) {
                sale += 1;
                let mut out = json!({
                    "type": "sale",
                    "item": line.item_ref_name,
                    "description": line.description,
                    "amount": line.amount,
                });
                if sale_ids {
                    out["id"] = json!(format!("s{sale}"));
                    out["item"] = json!(line.item_ref_value);
                }
                out
            } else {
                json!({ "type": "text", "description": line.description })
            }
        })
        .collect()
}

/// Digits as `#`, so "30 sqft" and "45 sqft" edits count as one pattern.
fn shape(text: Option<&str>) -> String {
    text.unwrap_or("")
        .trim()
        .to_lowercase()
        .chars()
        .map(|ch| if ch.is_ascii_digit() { '#' } else { ch })
        .collect()
}

/// Rep edits with how often the same kind of change was made, most frequent
/// (then newest) first.
async fn lessons(pool: &MySqlPool, company_id: i32) -> sqlx::Result<Vec<(Value, u32)>> {
    let rows: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT kind, draft_text, saved_text FROM qbo_invoice_draft_edits
          WHERE company_id = ? ORDER BY id DESC LIMIT ?",
    )
    .bind(company_id)
    .bind(EDITS_READ)
    .fetch_all(pool)
    .await?;
    let mut seen: HashMap<(String, String, String), usize> = HashMap::new();
    let mut out: Vec<(Value, u32)> = Vec::new();
    for (kind, before, after) in rows {
        let key = (
            kind.clone(),
            shape(before.as_deref()),
            shape(after.as_deref()),
        );
        if let Some(&at) = seen.get(&key) {
            out[at].1 += 1;
            continue;
        }
        seen.insert(key, out.len());
        out.push((json!({ "change": kind, "draft": before, "rep": after }), 1));
    }
    out.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    Ok(out)
}

/// The company's latest invoices; none when `QuickBooks` can't be read.
async fn examples(state: &AppState, company_id: i32) -> Vec<EditableInvoice> {
    let found = async {
        let access = state.qbo.access(&state.pool, company_id).await?;
        state
            .qbo
            .query(
                &access,
                &format!(
                    "select * from Invoice ORDERBY MetaData.CreateTime DESC MAXRESULTS {EXAMPLES}"
                ),
                "Invoice",
            )
            .await
    }
    .await;
    match found {
        Ok(rows) => rows.iter().filter_map(editable).collect(),
        Err(err) => {
            if !err.is_not_connected() {
                tracing::warn!(error = %err, company_id, "invoice style: no examples");
            }
            Vec::new()
        }
    }
}

/// The draft with the model's lines, or `None` when they would change the
/// money or name an unknown line.
fn apply(
    draft: &EditableInvoice,
    proposed: Vec<Proposed>,
    items: &HashMap<String, Option<String>>,
) -> Option<EditableInvoice> {
    let sales: HashMap<String, &Line> = draft
        .lines
        .iter()
        .filter(|line| is_sale(line))
        .enumerate()
        .map(|(index, line)| (format!("s{}", index + 1), line))
        .collect();
    let mut used = HashSet::new();
    let mut lines = Vec::new();
    for (index, line) in proposed.into_iter().enumerate() {
        let description = line
            .description
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty());
        let id = Some(format!("new:style-{index}"));
        if line.kind != "sale" {
            if let Some(description) = description {
                lines.push(Line {
                    id,
                    line_num: None,
                    detail_type: "DescriptionOnly".into(),
                    description: Some(description),
                    amount: None,
                    qty: None,
                    unit_price: None,
                    item_ref_value: None,
                    item_ref_name: None,
                    editable: true,
                });
            }
            continue;
        }
        let mut amount = 0.0;
        for from in &line.from {
            if !used.insert(from.clone()) {
                return None;
            }
            amount += sales.get(from)?.amount?;
        }
        let first = sales.get(line.from.first()?)?;
        let (item, item_name) = line
            .item
            .filter(|item| items.contains_key(item))
            .map_or_else(
                || (first.item_ref_value.clone(), first.item_ref_name.clone()),
                |item| {
                    let name = items.get(&item).cloned().flatten();
                    (Some(item), name)
                },
            );
        let amount = cents(amount);
        lines.push(Line {
            id,
            line_num: None,
            detail_type: "SalesItemLineDetail".into(),
            description: description.or_else(|| first.description.clone()),
            amount: Some(amount),
            qty: Some(1.0),
            unit_price: Some(amount),
            item_ref_value: item,
            item_ref_name: item_name,
            editable: true,
        });
    }
    (used.len() == sales.len()).then(|| EditableInvoice {
        lines,
        ..draft.clone()
    })
}

/// Write an invoice draft in the company's style
///
/// Rewrites a quote's invoice draft the way the company's latest
/// `QuickBooks` invoices read, following the changes reps keep making to
/// drafts. The amounts stay the draft's; when there is nothing to learn from
/// or the model fails, the draft comes back as it was (`styled` false).
/// Nothing is saved.
#[crate::openapi::path(
    post,
    path = "/v1/ai/invoice-style",
    operation_id = "style_invoice_draft",
    tag = "ai",
    request_body = InvoiceStyleInput,
    responses(
        (status = 200, description = "The draft, styled or as it was", body = InvoiceStyle),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn invoice_style(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<InvoiceStyleInput>,
) -> ApiResult<Json<InvoiceStyle>> {
    let draft = input.invoice;
    let unstyled = |draft: EditableInvoice| {
        Ok(Json(InvoiceStyle {
            invoice: draft,
            styled: false,
        }))
    };
    if !draft.lines.iter().any(is_sale) {
        return unstyled(draft);
    }
    let examples = examples(&state, user.company_id).await;
    let lessons = lessons(&state.pool, user.company_id).await?;
    if examples.is_empty() && lessons.is_empty() {
        return unstyled(draft);
    }
    let mut items: HashMap<String, Option<String>> = HashMap::new();
    for line in examples
        .iter()
        .flat_map(|invoice| &invoice.lines)
        .chain(&draft.lines)
    {
        if let Some(item) = &line.item_ref_value {
            items
                .entry(item.clone())
                .or_insert_with(|| line.item_ref_name.clone());
        }
    }
    let (rules, hints): (Vec<_>, Vec<_>) = lessons.into_iter().partition(|(_, n)| *n >= 2);
    let prompt = json!({
        "examples": examples.iter().map(|invoice| json!({ "lines": compact(&invoice.lines, false) })).collect::<Vec<_>>(),
        "rules": rules.into_iter().map(|(edit, count)| json!({ "edit": edit, "count": count })).collect::<Vec<_>>(),
        "hints": hints.into_iter().take(HINTS).map(|(edit, _)| edit).collect::<Vec<_>>(),
        "items": items.iter().map(|(id, name)| json!({ "id": id, "name": name })).collect::<Vec<_>>(),
        "draft": { "lines": compact(&draft.lines, true) },
    });
    let mut chat = Chat::new(MINI_MODEL)
        .message("system", SYSTEM_PROMPT)
        .message("user", &prompt.to_string())
        .max_tokens(Some(2000));
    chat.json = true;
    chat.reasoning_effort = Some("low");
    let caller = Caller {
        feature: "qbo_invoice_style",
        user_id: Some(user.id),
        company_id: Some(user.company_id),
    };
    let reply = match state.ai.chat(&state.pool, caller, &chat).await {
        Ok(reply) => reply,
        Err(err) => {
            tracing::warn!(error = %err, "invoice style: model failed");
            return unstyled(draft);
        }
    };
    let proposed = serde_json::from_str::<Value>(reply.trim())
        .ok()
        .and_then(|value| serde_json::from_value::<Vec<Proposed>>(value["lines"].clone()).ok());
    let Some(invoice) = proposed.and_then(|lines| apply(&draft, lines, &items)) else {
        tracing::warn!("invoice style: unusable answer");
        return unstyled(draft);
    };
    Ok(Json(InvoiceStyle {
        invoice,
        styled: true,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(kind: &str, text: &str, amount: Option<f64>, item: Option<&str>) -> Line {
        Line {
            id: Some(format!("new:{text}")),
            line_num: None,
            detail_type: kind.into(),
            description: Some(text.into()),
            amount,
            qty: amount.map(|_| 1.0),
            unit_price: amount,
            item_ref_value: item.map(Into::into),
            item_ref_name: item.map(|item| format!("Item {item}")),
            editable: true,
        }
    }

    fn draft() -> EditableInvoice {
        EditableInvoice {
            id: String::new(),
            sync_token: String::new(),
            doc_number: None,
            txn_date: Some("2026-10-26".into()),
            due_date: None,
            total_amt: Some(9184.0),
            balance: None,
            currency: None,
            customer_memo: None,
            private_note: Some("Quote: Q".into()),
            bill_addr: None,
            ship_addr: None,
            lines: vec![
                line(
                    "SalesItemLineDetail",
                    "Kitchen based @ 26.6 sqft",
                    Some(2090.0),
                    Some("3"),
                ),
                line(
                    "DescriptionOnly",
                    "33.5 lin ft Finished Edge - Flat",
                    None,
                    None,
                ),
                line("SalesItemLineDetail", "Cabinets", Some(5039.0), Some("3")),
                line(
                    "SalesItemLineDetail",
                    "Other Items",
                    Some(2055.0),
                    Some("3"),
                ),
            ],
        }
    }

    fn proposed(value: Value) -> Vec<Proposed> {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn the_model_regroups_and_rewords_but_the_amounts_are_added_here() {
        let items = HashMap::from([
            ("1".to_string(), Some("Package $11999".to_string())),
            ("3".to_string(), Some("Item 3".to_string())),
        ]);
        let styled = apply(
            &draft(),
            proposed(json!([
                { "type": "sale", "from": ["s1"], "item": "1", "description": "Kitchen based @ 26.6 sqft", "amount": 1 },
                { "type": "text", "description": "Flat Edge" },
                { "type": "sale", "from": ["s2", "s3"], "item": "9", "description": "Cabinets - Shaker" },
                { "type": "text", "description": "  " },
            ])),
            &items,
        )
        .unwrap();
        let money: Vec<_> = styled.lines.iter().map(|line| line.amount).collect();
        assert_eq!(money, [Some(2090.0), None, Some(7094.0)]);
        assert_eq!(
            styled.lines[0].item_ref_name.as_deref(),
            Some("Package $11999")
        );
        // An unknown item keeps the draft's.
        assert_eq!(styled.lines[2].item_ref_value.as_deref(), Some("3"));
        assert_eq!(styled.lines[1].description.as_deref(), Some("Flat Edge"));
        assert_eq!(styled.private_note.as_deref(), Some("Quote: Q"));
    }

    #[test]
    fn an_answer_that_loses_or_repeats_a_priced_line_is_refused() {
        let items = HashMap::new();
        let missing =
            proposed(json!([{ "type": "sale", "from": ["s1", "s2"], "description": "x" }]));
        assert!(apply(&draft(), missing, &items).is_none());
        let twice = proposed(json!([
            { "type": "sale", "from": ["s1", "s2"] },
            { "type": "sale", "from": ["s2", "s3"] },
        ]));
        assert!(apply(&draft(), twice, &items).is_none());
        let unknown = proposed(json!([{ "type": "sale", "from": ["s1", "s2", "s3", "s9"] }]));
        assert!(apply(&draft(), unknown, &items).is_none());
    }

    #[test]
    fn edits_with_other_numbers_are_one_pattern() {
        assert_eq!(
            shape(Some("Kitchen based @ 30 sqft")),
            shape(Some("kitchen based @ 45 sqft"))
        );
    }
}
