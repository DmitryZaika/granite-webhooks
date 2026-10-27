//! `POST /v1/ai/invoice-edit`: an invoice edit proposed from an instruction.
//!
//! "Remove the flat edge", "make it 2 sinks". Remix: `api.ai.invoice-edit`
//! (prompt unchanged; the model sees the invoice with the CRM's camelCase
//! keys, as before).

use crate::ai::{Caller, Chat, MINI_MODEL};
use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::routes::quickbooks::invoice::{EditableInvoice, Line};
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::HashSet;

const SYSTEM_PROMPT: &str = r#"You edit QuickBooks Online invoices for a granite/countertop business.
Given the current invoice JSON and a user instruction, return ONLY valid JSON for the full updated invoice in the same shape.

Rules:
- Keep id and syncToken unchanged.
- Only change fields the user asked about.
- For lines: you may edit description, qty, unitPrice, amount on editable sales lines (detailType SalesItemLineDetail) and description on DescriptionOnly lines.
- You may ADD new sales lines only when the user asks, by copying an existing itemRefValue/itemRefName from this invoice. New line ids must start with "new:" (example "new:1").
- Do NOT invent new itemRefValue values that are not already on the invoice.
- REMOVING a product, service, option, or edge (examples: "remove Flat edge", "delete polished edge", "take off the sink cutout", "remove line X"): omit that entire line object from the lines array. Do not clear description, set qty to 0, or blank amount/price while leaving the row. Match by itemRefName and/or description text (case-insensitive, partial match is OK when unambiguous). If several lines match, remove every matching editable line the user clearly meant.
- Only omit editable lines (SalesItemLineDetail or DescriptionOnly with editable true). Always keep non-editable lines (SubTotal, Discount, Tax, etc.) unchanged and present in the output.
- Do NOT change non-editable lines except leave them as-is.
- If qty and unitPrice are both set on a sales line, set amount = qty * unitPrice (2 decimal places).
- You may update customerMemo, privateNote, billAddr, shipAddr, dueDate, txnDate when asked.
- Preserve existing line id values exactly for lines you keep.
- Output JSON with key "invoice" matching the input invoice schema."#;

/// Body of `POST /v1/ai/invoice-edit`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct InvoiceEditInput {
    /// The invoice as the editor has it.
    pub invoice: EditableInvoice,
    /// What to change.
    pub instruction: String,
}

/// `POST /v1/ai/invoice-edit`.
#[derive(Debug, Serialize, ToSchema)]
pub struct InvoiceEdit {
    /// The proposed invoice, for the editor to preview and save.
    pub invoice: EditableInvoice,
}

/// Renames object keys recursively with `rename`.
fn rekey(value: Value, rename: &dyn Fn(&str) -> String) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .into_iter()
                .map(|(key, value)| (rename(&key), rekey(value, rename)))
                .collect::<Map<_, _>>(),
        ),
        Value::Array(items) => {
            Value::Array(items.into_iter().map(|item| rekey(item, rename)).collect())
        }
        other => other,
    }
}

fn camel(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    let mut upper = false;
    for ch in key.chars() {
        if ch == '_' {
            upper = true;
        } else if upper {
            out.extend(ch.to_uppercase());
            upper = false;
        } else {
            out.push(ch);
        }
    }
    out
}

fn snake(key: &str) -> String {
    let mut out = String::with_capacity(key.len() + 4);
    for ch in key.chars() {
        if ch.is_ascii_uppercase() {
            out.push('_');
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

fn rounded(qty: f64, unit_price: f64) -> f64 {
    (qty * unit_price * 100.0).round() / 100.0
}

/// The CRM's `mergeAiInvoice`: only what the editor allows changes; lines
/// the model dropped are removed; new lines must reuse an item already on
/// the invoice.
pub fn merge(current: &EditableInvoice, proposed: EditableInvoice) -> EditableInvoice {
    let proposed_ids: HashSet<&str> = proposed
        .lines
        .iter()
        .filter_map(|line| line.id.as_deref())
        .collect();
    let current_ids: HashSet<&str> = current
        .lines
        .iter()
        .filter_map(|line| line.id.as_deref())
        .collect();
    let items: HashSet<&str> = current
        .lines
        .iter()
        .filter_map(|line| line.item_ref_value.as_deref())
        .filter(|item| !item.is_empty())
        .collect();
    let mut lines: Vec<Line> = current
        .lines
        .iter()
        .filter_map(|original| {
            let Some(id) = original.id.as_deref() else {
                return Some(original.clone());
            };
            if !original.editable {
                return Some(original.clone());
            }
            if !proposed_ids.contains(id) {
                return None;
            }
            let next = proposed
                .lines
                .iter()
                .find(|line| line.id.as_deref() == Some(id))?;
            let mut kept = original.clone();
            match original.detail_type.as_str() {
                "DescriptionOnly" => kept.description.clone_from(&next.description),
                "SalesItemLineDetail" => {
                    kept.description.clone_from(&next.description);
                    kept.qty = next.qty;
                    kept.unit_price = next.unit_price;
                    kept.amount = match (next.qty, next.unit_price) {
                        (Some(qty), Some(price)) => Some(rounded(qty, price)),
                        _ => next.amount,
                    };
                }
                _ => {}
            }
            Some(kept)
        })
        .collect();
    for line in &proposed.lines {
        let Some(id) = line.id.as_deref().filter(|id| id.starts_with("new:")) else {
            continue;
        };
        if current_ids.contains(id) {
            continue;
        }
        let Some(item) = line
            .item_ref_value
            .as_deref()
            .filter(|item| items.contains(item))
        else {
            continue;
        };
        let item_name = current
            .lines
            .iter()
            .find(|existing| existing.item_ref_value.as_deref() == Some(item))
            .and_then(|existing| existing.item_ref_name.clone())
            .or_else(|| line.item_ref_name.clone());
        let qty = line.qty.unwrap_or(1.0);
        let unit_price = line.unit_price.unwrap_or(0.0);
        lines.push(Line {
            id: Some(id.to_string()),
            line_num: None,
            detail_type: "SalesItemLineDetail".into(),
            description: Some(
                line.description
                    .clone()
                    .or_else(|| item_name.clone())
                    .unwrap_or_else(|| "New line".into()),
            ),
            amount: Some(rounded(qty, unit_price)),
            qty: Some(qty),
            unit_price: Some(unit_price),
            item_ref_value: Some(item.to_string()),
            item_ref_name: item_name,
            editable: true,
        });
    }
    EditableInvoice {
        txn_date: proposed.txn_date.or_else(|| current.txn_date.clone()),
        due_date: proposed.due_date.or_else(|| current.due_date.clone()),
        customer_memo: proposed.customer_memo,
        private_note: proposed.private_note,
        bill_addr: proposed.bill_addr.or_else(|| current.bill_addr.clone()),
        ship_addr: proposed.ship_addr.or_else(|| current.ship_addr.clone()),
        lines,
        ..current.clone()
    }
}

/// Propose an invoice edit
///
/// Applies a plain-language instruction to the invoice with AI and returns
/// the result for the editor to preview. Nothing is saved.
#[crate::openapi::path(
    post,
    path = "/v1/ai/invoice-edit",
    operation_id = "propose_invoice_edit",
    tag = "ai",
    request_body = InvoiceEditInput,
    responses(
        (status = 200, description = "Proposed invoice", body = InvoiceEdit),
        (status = 400, description = "Empty or too long instruction", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 502, description = "OpenAI failed or answered with something else", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn invoice_edit(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<InvoiceEditInput>,
) -> ApiResult<Json<InvoiceEdit>> {
    if input.instruction.trim().is_empty() || input.instruction.chars().count() > 4000 {
        return Err(ApiError::BadRequest("Invalid request data".into()));
    }
    let invoice =
        serde_json::to_value(&input.invoice).map_err(|err| ApiError::Internal(err.to_string()))?;
    let prompt = json!({ "instruction": input.instruction, "invoice": rekey(invoice, &camel) });
    let mut chat = Chat::new(MINI_MODEL)
        .message("system", SYSTEM_PROMPT)
        .message("user", &prompt.to_string());
    chat.json = true;
    let caller = Caller {
        feature: "qbo_invoice_edit",
        user_id: Some(user.id),
        company_id: Some(user.company_id),
    };
    let reply = state
        .ai
        .chat(&state.pool, caller, &chat)
        .await
        .map_err(|err| ApiError::Upstream(err.to_string()))?;
    let parsed: Value = serde_json::from_str(reply.trim())
        .map_err(|_| ApiError::Upstream("AI returned invalid JSON".into()))?;
    let proposed = parsed.get("invoice").cloned().unwrap_or(parsed);
    let proposed: EditableInvoice = serde_json::from_value(rekey(proposed, &snake))
        .map_err(|_| ApiError::Upstream("AI returned an invalid invoice shape".into()))?;
    Ok(Json(InvoiceEdit {
        invoice: merge(&input.invoice, proposed),
    }))
}

#[cfg(test)]
mod tests {
    use super::{camel, snake};

    #[test]
    fn keys_round_trip() {
        assert_eq!(camel("country_sub_division_code"), "countrySubDivisionCode");
        assert_eq!(snake("countrySubDivisionCode"), "country_sub_division_code");
        assert_eq!(camel("line1"), "line1");
    }
}
