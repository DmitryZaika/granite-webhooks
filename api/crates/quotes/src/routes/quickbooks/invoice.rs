//! `QuickBooks` invoices as the CRM shows and edits them. Port of
//! `parseQboInvoice`, `parseQboInvoiceEditable` and
//! `updateQboInvoiceFromEditable` of `quickbooks.server.ts`.

use crate::openapi::ToSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};

/// A string or number field as text; `None` when empty.
pub fn text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) if !text.trim().is_empty() => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// A number or numeric string.
pub fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse().ok().filter(|n: &f64| n.is_finite()),
        _ => None,
    }
}

/// An invoice in a list.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct InvoiceSummary {
    /// `QuickBooks` invoice id.
    pub id: String,
    /// Invoice number.
    pub doc_number: Option<String>,
    /// Invoice date (`YYYY-MM-DD`).
    pub txn_date: Option<String>,
    /// Due date (`YYYY-MM-DD`).
    pub due_date: Option<String>,
    /// Total.
    pub total_amt: Option<f64>,
    /// Still owed.
    pub balance: Option<f64>,
    /// Currency code.
    pub currency: Option<String>,
    /// The shipping address, on one line.
    pub project_address: Option<String>,
    /// When `QuickBooks` last changed it (a paid invoice: about when it was paid).
    pub last_updated: Option<String>,
}

fn one_line_address(value: &Value) -> Option<String> {
    let parts: Vec<String> = [
        "Line1",
        "Line2",
        "Line3",
        "City",
        "CountrySubDivisionCode",
        "PostalCode",
    ]
    .iter()
    .filter_map(|key| text(&value[*key]))
    .collect();
    (!parts.is_empty()).then(|| parts.join(", "))
}

/// The bill-to lines after a "Project Address:" line, on one line.
fn bill_to_project_address(value: &Value) -> Option<String> {
    let lines: Vec<String> = ["Line1", "Line2", "Line3", "Line4", "Line5"]
        .iter()
        .filter_map(|key| text(&value[*key]))
        .collect();
    let at = lines
        .iter()
        .position(|line| line.trim().to_lowercase().starts_with("project address"))?;
    let inline = lines[at]
        .trim()
        .get("project address".len()..)
        .unwrap_or("")
        .trim_start_matches(':')
        .to_string();
    let parts: Vec<String> = std::iter::once(inline)
        .chain(lines[at + 1..].iter().cloned())
        .chain(
            ["City", "CountrySubDivisionCode", "PostalCode"]
                .iter()
                .filter_map(|key| text(&value[*key])),
        )
        .map(|part| part.trim().to_string())
        .filter(|part| !part.is_empty())
        .collect();
    (!parts.is_empty()).then(|| parts.join(", "))
}

/// One `Invoice` row of a query.
pub fn summary(value: &Value) -> Option<InvoiceSummary> {
    Some(InvoiceSummary {
        id: text(&value["Id"])?,
        doc_number: text(&value["DocNumber"]),
        txn_date: text(&value["TxnDate"]),
        due_date: text(&value["DueDate"]),
        total_amt: number(&value["TotalAmt"]),
        balance: number(&value["Balance"]),
        currency: text(&value["CurrencyRef"]["value"]),
        project_address: one_line_address(&value["ShipAddr"])
            .or_else(|| bill_to_project_address(&value["BillAddr"])),
        last_updated: text(&value["MetaData"]["LastUpdatedTime"]),
    })
}

/// An address of an invoice.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct Address {
    /// First street line.
    pub line1: Option<String>,
    /// Second line.
    pub line2: Option<String>,
    /// Third line.
    pub line3: Option<String>,
    /// City.
    pub city: Option<String>,
    /// State code.
    pub country_sub_division_code: Option<String>,
    /// ZIP code.
    pub postal_code: Option<String>,
}

const ADDRESS_KEYS: [&str; 6] = [
    "Line1",
    "Line2",
    "Line3",
    "City",
    "CountrySubDivisionCode",
    "PostalCode",
];

impl Address {
    fn parse(value: &Value) -> Self {
        let [
            line1,
            line2,
            line3,
            city,
            country_sub_division_code,
            postal_code,
        ] = ADDRESS_KEYS.map(|key| text(&value[key]));
        Self {
            line1,
            line2,
            line3,
            city,
            country_sub_division_code,
            postal_code,
        }
    }

    fn to_qbo(&self) -> Option<Value> {
        let parts = [
            &self.line1,
            &self.line2,
            &self.line3,
            &self.city,
            &self.country_sub_division_code,
            &self.postal_code,
        ];
        let body: Map<String, Value> = ADDRESS_KEYS
            .iter()
            .zip(parts)
            .filter_map(|(key, part)| {
                part.as_deref()
                    .filter(|part| !part.is_empty())
                    .map(|part| ((*key).to_string(), json!(part)))
            })
            .collect();
        (!body.is_empty()).then_some(Value::Object(body))
    }
}

/// One line of an invoice.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Line {
    /// `QuickBooks` line id; `new:<anything>` for a line the editor added.
    pub id: Option<String>,
    /// Position.
    pub line_num: Option<f64>,
    /// `SalesItemLineDetail`, `DescriptionOnly`, `SubTotalLineDetail`, ...
    pub detail_type: String,
    /// Line text.
    pub description: Option<String>,
    /// Line amount.
    pub amount: Option<f64>,
    /// Quantity (sales lines).
    pub qty: Option<f64>,
    /// Unit price (sales lines).
    pub unit_price: Option<f64>,
    /// The product or service id (sales lines).
    pub item_ref_value: Option<String>,
    /// The product or service name.
    pub item_ref_name: Option<String>,
    /// Sales and description lines can be edited.
    pub editable: bool,
}

/// An invoice as the editor shows it.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct EditableInvoice {
    /// `QuickBooks` invoice id.
    pub id: String,
    /// `QuickBooks`' version; an update must send the current one.
    pub sync_token: String,
    /// Invoice number.
    pub doc_number: Option<String>,
    /// Invoice date (`YYYY-MM-DD`).
    pub txn_date: Option<String>,
    /// Due date (`YYYY-MM-DD`).
    pub due_date: Option<String>,
    /// Total.
    pub total_amt: Option<f64>,
    /// Still owed.
    pub balance: Option<f64>,
    /// Currency code.
    pub currency: Option<String>,
    /// Message shown on the invoice.
    pub customer_memo: Option<String>,
    /// Internal note.
    pub private_note: Option<String>,
    /// Billing address.
    pub bill_addr: Option<Address>,
    /// Shipping (project) address.
    pub ship_addr: Option<Address>,
    /// The lines, in order.
    pub lines: Vec<Line>,
}

fn rows(value: &Value) -> Vec<Value> {
    match value {
        Value::Array(rows) => rows.clone(),
        Value::Null => Vec::new(),
        other => vec![other.clone()],
    }
}

fn parse_line(value: &Value) -> Option<Line> {
    let detail_type = text(&value["DetailType"])?;
    let sales = &value["SalesItemLineDetail"];
    Some(Line {
        id: text(&value["Id"]),
        line_num: number(&value["LineNum"]),
        editable: detail_type == "SalesItemLineDetail" || detail_type == "DescriptionOnly",
        detail_type,
        description: text(&value["Description"]),
        amount: number(&value["Amount"]),
        qty: number(&sales["Qty"]),
        unit_price: number(&sales["UnitPrice"]),
        item_ref_value: text(&sales["ItemRef"]["value"]),
        item_ref_name: text(&sales["ItemRef"]["name"]),
    })
}

/// The editor's view of a `QuickBooks` invoice.
pub fn editable(value: &Value) -> Option<EditableInvoice> {
    let address = |key: &str| value.get(key).filter(|v| !v.is_null()).map(Address::parse);
    Some(EditableInvoice {
        id: text(&value["Id"])?,
        sync_token: match &value["SyncToken"] {
            Value::String(token) => token.clone(),
            Value::Number(number) => number.to_string(),
            _ => return None,
        },
        doc_number: text(&value["DocNumber"]),
        txn_date: text(&value["TxnDate"]),
        due_date: text(&value["DueDate"]),
        total_amt: number(&value["TotalAmt"]),
        balance: number(&value["Balance"]),
        currency: text(&value["CurrencyRef"]["value"]),
        customer_memo: text(&value["CustomerMemo"]["value"]),
        private_note: text(&value["PrivateNote"]),
        bill_addr: address("BillAddr"),
        ship_addr: address("ShipAddr"),
        lines: rows(&value["Line"]).iter().filter_map(parse_line).collect(),
    })
}

fn sales_amount(edited: &Line) -> (f64, f64, f64) {
    let qty = edited.qty.unwrap_or(1.0);
    let unit_price = edited.unit_price.unwrap_or(0.0);
    let amount = edited
        .amount
        .unwrap_or_else(|| (qty * unit_price * 100.0).round() / 100.0);
    (qty, unit_price, amount)
}

fn item_ref(edited: &Line, value: &str) -> Value {
    let mut item = json!({ "value": value });
    if let Some(name) = &edited.item_ref_name {
        item["name"] = json!(name);
    }
    item
}

fn description_only(edited: &Line) -> Map<String, Value> {
    let mut line = Map::new();
    line.insert("DetailType".into(), json!("DescriptionOnly"));
    if let Some(description) = &edited.description {
        line.insert("Description".into(), json!(description));
    }
    line.insert("DescriptionLineDetail".into(), json!({}));
    line
}

fn new_sales_line(edited: &Line, item: &str) -> Map<String, Value> {
    let (qty, unit_price, amount) = sales_amount(edited);
    let mut line = Map::new();
    line.insert("DetailType".into(), json!("SalesItemLineDetail"));
    line.insert("Amount".into(), json!(amount));
    if let Some(description) = &edited.description {
        line.insert("Description".into(), json!(description));
    }
    line.insert(
        "SalesItemLineDetail".into(),
        json!({ "ItemRef": item_ref(edited, item), "Qty": qty, "UnitPrice": unit_price }),
    );
    line
}

fn new_line(edited: &Line) -> Value {
    match edited.item_ref_value.as_deref() {
        Some(item) if edited.detail_type != "DescriptionOnly" => {
            Value::Object(new_sales_line(edited, item))
        }
        _ => Value::Object(description_only(edited)),
    }
}

fn keep_position(line: &mut Map<String, Value>, original: &Value) {
    if let Some(id) = text(&original["Id"]) {
        line.insert("Id".into(), json!(id));
    }
    if let Some(num) = original.get("LineNum") {
        line.insert("LineNum".into(), num.clone());
    }
}

fn updated_line(original: &Value, edited: &Line) -> Result<Value, String> {
    match edited.detail_type.as_str() {
        "DescriptionOnly" => {
            let mut line = description_only(edited);
            keep_position(&mut line, original);
            Ok(Value::Object(line))
        }
        "SalesItemLineDetail" => {
            let item = edited
                .item_ref_value
                .as_deref()
                .ok_or_else(|| "Sales line requires a QuickBooks item".to_string())?;
            if original["DetailType"] != "SalesItemLineDetail" {
                let mut line = new_sales_line(edited, item);
                keep_position(&mut line, original);
                return Ok(Value::Object(line));
            }
            let (qty, unit_price, amount) = sales_amount(edited);
            let mut line = original.as_object().cloned().unwrap_or_default();
            let mut detail = original["SalesItemLineDetail"]
                .as_object()
                .cloned()
                .unwrap_or_default();
            detail.insert("Qty".into(), json!(qty));
            detail.insert("UnitPrice".into(), json!(unit_price));
            detail.insert("ItemRef".into(), item_ref(edited, item));
            match &edited.description {
                Some(description) => line.insert("Description".into(), json!(description)),
                None => line.remove("Description"),
            };
            line.insert("Amount".into(), json!(amount));
            line.insert("SalesItemLineDetail".into(), Value::Object(detail));
            Ok(Value::Object(line))
        }
        _ => Ok(original.clone()),
    }
}

fn is_new(id: Option<&str>) -> bool {
    id.is_some_and(|id| id.starts_with("new:"))
}

/// The sparse update `QuickBooks` takes for `edited`, on top of `current`.
pub fn update_payload(current: &Value, edited: &EditableInvoice) -> Result<Value, String> {
    let current_id = text(&current["Id"]).ok_or("Invoice is missing Id or SyncToken")?;
    let sync_token = match &current["SyncToken"] {
        Value::String(token) => token.clone(),
        Value::Number(number) => number.to_string(),
        _ => return Err("Invoice is missing Id or SyncToken".into()),
    };
    if current_id != edited.id {
        return Err("Invoice id mismatch".into());
    }
    let edited_by_id: HashMap<&str, &Line> = edited
        .lines
        .iter()
        .filter(|line| !is_new(line.id.as_deref()))
        .filter_map(|line| line.id.as_deref().map(|id| (id, line)))
        .collect();
    let mut next = Vec::new();
    let mut trailer = Vec::new();
    let mut matched = HashSet::new();
    for original in rows(&current["Line"]) {
        if !original.is_object() {
            next.push(original);
            continue;
        }
        let detail_type = text(&original["DetailType"]);
        match detail_type.as_deref() {
            Some("SubTotalLineDetail") => continue,
            Some("DiscountLineDetail" | "TaxLineDetail") => {
                trailer.push(original);
                continue;
            }
            _ => {}
        }
        let line_id = text(&original["Id"]);
        let edited_line = line_id
            .as_deref()
            .and_then(|id| edited_by_id.get(id).copied());
        let editable_kind = matches!(
            detail_type.as_deref(),
            Some("SalesItemLineDetail" | "DescriptionOnly")
        );
        match (line_id, edited_line) {
            // The editor removed it.
            (Some(_), None) if editable_kind => {}
            (Some(id), Some(line)) => {
                matched.insert(id);
                next.push(updated_line(&original, line)?);
            }
            _ => next.push(original),
        }
    }
    for line in edited
        .lines
        .iter()
        .filter(|line| is_new(line.id.as_deref()))
    {
        next.push(new_line(line));
    }
    for line in &edited.lines {
        let Some(id) = line.id.as_deref() else {
            continue;
        };
        let unmatched_edit = !is_new(Some(id))
            && !matched.contains(id)
            && line.editable
            && matches!(
                line.detail_type.as_str(),
                "SalesItemLineDetail" | "DescriptionOnly"
            );
        if unmatched_edit {
            next.push(new_line(line));
        }
    }
    next.extend(trailer);
    let mut payload =
        json!({ "Id": current_id, "SyncToken": sync_token, "sparse": true, "Line": next });
    if let Some(date) = edited.due_date.as_deref().filter(|date| !date.is_empty()) {
        payload["DueDate"] = json!(date);
    }
    if let Some(date) = edited.txn_date.as_deref().filter(|date| !date.is_empty()) {
        payload["TxnDate"] = json!(date);
    }
    if let Some(note) = &edited.private_note {
        payload["PrivateNote"] = json!(note);
    }
    if let Some(memo) = &edited.customer_memo {
        payload["CustomerMemo"] = json!({ "value": memo });
    }
    if let Some(address) = edited.bill_addr.as_ref().and_then(Address::to_qbo) {
        payload["BillAddr"] = address;
    }
    if let Some(address) = edited.ship_addr.as_ref().and_then(Address::to_qbo) {
        payload["ShipAddr"] = address;
    }
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn current() -> Value {
        json!({
            "Id": "77", "SyncToken": "3",
            "Line": [
                { "Id": "1", "LineNum": 1, "DetailType": "SalesItemLineDetail", "Amount": 100, "Description": "Quartz",
                  "SalesItemLineDetail": { "ItemRef": { "value": "9", "name": "Stone" }, "Qty": 1, "UnitPrice": 100, "TaxCodeRef": { "value": "TAX" } } },
                { "Id": "2", "LineNum": 2, "DetailType": "DescriptionOnly", "Description": "Old note" },
                { "DetailType": "SubTotalLineDetail", "Amount": 100 },
                { "DetailType": "DiscountLineDetail", "Amount": 5 },
            ],
        })
    }

    #[test]
    fn an_edit_keeps_untouched_details_and_trailers() {
        let mut invoice = editable(&current()).unwrap();
        assert_eq!(invoice.lines.len(), 4);
        invoice.lines[0].qty = Some(2.0);
        invoice.lines[0].amount = None;
        invoice.lines.remove(1);
        invoice.lines.push(Line {
            id: Some("new:1".into()),
            line_num: None,
            detail_type: "DescriptionOnly".into(),
            description: Some("Thanks".into()),
            amount: None,
            qty: None,
            unit_price: None,
            item_ref_value: None,
            item_ref_name: None,
            editable: true,
        });
        invoice.private_note = Some("call first".into());
        let payload = update_payload(&current(), &invoice).unwrap();
        let lines = payload["Line"].as_array().unwrap();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0]["Amount"], 200.0);
        assert_eq!(
            lines[0]["SalesItemLineDetail"]["TaxCodeRef"]["value"],
            "TAX"
        );
        assert_eq!(lines[1]["Description"], "Thanks");
        assert_eq!(lines[2]["DetailType"], "DiscountLineDetail");
        assert_eq!(payload["SyncToken"], "3");
        assert_eq!(payload["PrivateNote"], "call first");
        assert!(payload.get("BillAddr").is_none());
    }
}
