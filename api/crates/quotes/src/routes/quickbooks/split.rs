//! A quote's invoice split into several, the company's own way.
//!
//! E.g. a deposit, a payment due the day the cabinets go in, and the
//! balance. Every invoice keeps all the lines and charges its share of each priced
//! line (the rest at $0), so the customer sees the whole job each time; a
//! line keeps its own class, so cabinets and countertops are paid to their
//! own classes. How the company splits is kept: its next split of a quote
//! with the same parts starts the same way, and each rep's lines start in
//! the classes they last gave them.

use super::invoice::{EditableInvoice, Line, create_payload, editable, line_amount};
use super::{prepare, qbo_refusal, refuse_installer, rules};
use crate::auth::EmployeeUser;
use crate::auth::session::SessionUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::{IntoParams, ToSchema};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, Query, State};
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::MySqlPool;
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// At most this many invoices in one split.
const MAX_INVOICES: usize = 12;

/// A line's part of the quote, which a split's shares and classes are
/// kept by: `stone` (countertops), `area` (cabinets and other areas
/// without stone), `other`, `discount`, or `added` (a line the rep added).
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct Part {
    /// The line's `id`.
    pub line_id: String,
    /// Its part.
    pub key: String,
}

/// What one invoice of a split charges of one priced line.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct Share {
    /// The line's `id`.
    pub line_id: String,
    /// The amount this invoice charges of it.
    pub amount: f64,
}

/// One invoice of a split.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct SplitInvoice {
    /// Its message to the customer (e.g. "Deposit"); none keeps the
    /// invoice's message.
    pub label: Option<String>,
    /// What it charges of each priced line; a line it leaves out is $0.
    pub shares: Vec<Share>,
}

/// Body of `POST /v1/customers/{id}/qbo-invoices/split`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct SplitCreate {
    /// The invoice as the editor left it, each priced line at its full
    /// amount (`id` and `sync_token` unused).
    pub invoice: EditableInvoice,
    /// The quote it is made from, which keeps the first invoice's id.
    pub quote_id: Option<i32>,
    /// The lines the editor opened with, as for one invoice.
    pub draft: Option<Vec<Line>>,
    /// The invoices, in order; each priced line's shares add up to it.
    pub invoices: Vec<SplitInvoice>,
    /// Each line's part of the quote.
    #[serde(default)]
    pub parts: Vec<Part>,
}

/// `POST /v1/customers/{id}/qbo-invoices/split`.
#[derive(Debug, Serialize, ToSchema)]
pub struct SplitCreated {
    /// The invoices as `QuickBooks` made them, in order.
    pub invoices: Vec<EditableInvoice>,
}

/// One invoice of the company's usual split.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct LayoutInvoice {
    /// Its message.
    pub label: Option<String>,
    /// The percent it charges of each part (`stone`, `area`, ...).
    pub shares: BTreeMap<String, f64>,
}

/// `GET /v1/qbo/invoice-split`.
#[derive(Debug, Serialize, ToSchema)]
pub struct SplitTemplate {
    /// The company's newest split of a quote with these parts (else with
    /// more, else any); none before its first split.
    pub invoices: Option<Vec<LayoutInvoice>>,
    /// The class the rep last gave each part's lines; null for the
    /// invoice's class.
    pub classes: BTreeMap<String, Option<String>>,
}

/// Query of `GET /v1/qbo/invoice-split`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct TemplateQuery {
    /// The quote's parts, comma-separated (e.g. `stone,area`).
    pub parts: Option<String>,
}

fn cents(amount: f64) -> f64 {
    (amount * 100.0).round() / 100.0
}

fn money(amount: f64) -> String {
    format!("${:.2}", cents(amount))
}

fn is_sales(line: &Line) -> bool {
    line.detail_type == "SalesItemLineDetail"
}

/// The invoices of a split of `base`, each with every line and its share
/// of each priced line; an error says what does not add up.
pub fn split(
    base: &EditableInvoice,
    invoices: &[SplitInvoice],
) -> Result<Vec<EditableInvoice>, String> {
    let count = invoices.len();
    if !(2..=MAX_INVOICES).contains(&count) {
        return Err(format!("Split into 2 to {MAX_INVOICES} invoices."));
    }
    let mut priced: Vec<(&str, f64, &Line)> = Vec::new();
    for line in base.lines.iter().filter(|line| is_sales(line)) {
        let id = line
            .id
            .as_deref()
            .ok_or_else(|| "Every priced line needs an id.".to_string())?;
        priced.push((id, cents(line_amount(line)), line));
    }
    let mut table: Vec<HashMap<&str, f64>> = vec![HashMap::new(); count];
    for (index, invoice) in invoices.iter().enumerate() {
        for share in &invoice.shares {
            if !share.amount.is_finite() {
                return Err(format!(
                    "Invoice {} has an amount that is not a number.",
                    index + 1
                ));
            }
            if !priced.iter().any(|(id, ..)| *id == share.line_id) {
                return Err(format!(
                    "Invoice {} charges a line the invoice does not have.",
                    index + 1
                ));
            }
            *table[index].entry(share.line_id.as_str()).or_insert(0.0) += cents(share.amount);
        }
    }
    for (id, full, line) in &priced {
        let shared: f64 = table.iter().filter_map(|row| row.get(id)).sum();
        if (shared - full).abs() > 0.005 {
            let name = line.description.as_deref().unwrap_or("A line");
            return Err(format!(
                "\"{name}\" is split into {}, not its {}.",
                money(shared),
                money(*full)
            ));
        }
    }
    let note = base.private_note.as_deref().map_or("", str::trim);
    let mut out = Vec::with_capacity(count);
    for (index, invoice) in invoices.iter().enumerate() {
        let mut made = base.clone();
        let mut total = 0.0;
        for line in made.lines.iter_mut().filter(|line| is_sales(line)) {
            let Some((_, full, _)) = priced
                .iter()
                .find(|(id, ..)| Some(*id) == line.id.as_deref())
            else {
                continue;
            };
            let share = line
                .id
                .as_deref()
                .and_then(|id| table[index].get(id))
                .copied()
                .map_or(0.0, cents);
            total += share;
            // QuickBooks wants the amount to be qty × price: a share is one
            // of its amount.
            if (share - full).abs() > 0.005 {
                line.qty = Some(1.0);
                line.unit_price = Some(share);
                line.amount = Some(share);
            }
        }
        let total = cents(total);
        if total.abs() < 0.005 {
            return Err(format!("Invoice {} charges nothing.", index + 1));
        }
        if total < 0.0 {
            return Err(format!("Invoice {} comes to less than $0.", index + 1));
        }
        made.total_amt = Some(total);
        if let Some(label) = invoice
            .label
            .as_deref()
            .map(str::trim)
            .filter(|label| !label.is_empty())
        {
            made.customer_memo = Some(label.to_string());
        }
        let place = format!("{} of {count}", index + 1);
        made.private_note = Some(if note.is_empty() {
            format!("Invoice {place}")
        } else {
            format!("{note} · Invoice {place}")
        });
        out.push(made);
    }
    Ok(out)
}

/// Each priced line's part; a line without one is `added`.
fn parts_by_line<'a>(base: &'a EditableInvoice, parts: &'a [Part]) -> Vec<(&'a Line, &'a str)> {
    let by_id: HashMap<&str, &str> = parts
        .iter()
        .map(|part| (part.line_id.as_str(), part.key.trim()))
        .filter(|(_, key)| !key.is_empty())
        .collect();
    base.lines
        .iter()
        .filter(|line| is_sales(line))
        .map(|line| {
            let key = line
                .id
                .as_deref()
                .and_then(|id| by_id.get(id).copied())
                .unwrap_or("added");
            (line, key)
        })
        .collect()
}

/// The percent each invoice charged of each part, to start the next split.
pub fn layout(
    base: &EditableInvoice,
    made: &[EditableInvoice],
    labels: &[Option<String>],
    parts: &[Part],
) -> Vec<LayoutInvoice> {
    let keyed = parts_by_line(base, parts);
    let mut full: BTreeMap<&str, f64> = BTreeMap::new();
    for (line, key) in &keyed {
        *full.entry(key).or_insert(0.0) += line_amount(line);
    }
    made.iter()
        .enumerate()
        .map(|(index, invoice)| {
            let mut charged: BTreeMap<&str, f64> = BTreeMap::new();
            for (line, key) in &keyed {
                let share = invoice
                    .lines
                    .iter()
                    .find(|made| made.id == line.id)
                    .map_or(0.0, line_amount);
                *charged.entry(key).or_insert(0.0) += share;
            }
            let shares = charged
                .into_iter()
                .filter_map(|(key, amount)| {
                    let whole = full.get(key).copied().unwrap_or(0.0);
                    (whole.abs() > 0.005)
                        .then(|| (key.to_string(), (amount / whole * 10_000.0).round() / 100.0))
                })
                .collect();
            LayoutInvoice {
                label: labels
                    .get(index)
                    .cloned()
                    .flatten()
                    .map(|label| label.trim().to_string())
                    .filter(|label| !label.is_empty()),
                shares,
            }
        })
        .collect()
}

/// The class of each part's lines (the first line's); null for the invoice's.
fn part_classes(base: &EditableInvoice, parts: &[Part]) -> BTreeMap<String, Option<String>> {
    let mut classes = BTreeMap::new();
    for (line, key) in parts_by_line(base, parts) {
        classes.entry(key.to_string()).or_insert_with(|| {
            line.class_name
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(String::from)
        });
    }
    classes
}

fn part_keys(base: &EditableInvoice, parts: &[Part]) -> String {
    parts_by_line(base, parts)
        .into_iter()
        .map(|(_, key)| key)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .join(",")
}

/// Keeps how a quote was invoiced: the `made` invoices, the split's
/// `layout` (empty for one invoice) and the classes its parts got. Losing
/// it loses only the lesson.
pub async fn remember(
    pool: &MySqlPool,
    user: &SessionUser,
    quote_id: Option<i32>,
    base: &EditableInvoice,
    made: &[EditableInvoice],
    layout: &[LayoutInvoice],
    parts: &[Part],
) {
    let ids = made
        .iter()
        .map(|invoice| invoice.id.as_str())
        .collect::<Vec<_>>()
        .join(",");
    let result = sqlx::query(
        "INSERT INTO qbo_invoice_splits
            (company_id, user_id, quote_id, qbo_invoice_ids, invoice_count, part_keys,
             invoices_json, classes_json)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(user.company_id)
    .bind(user.id)
    .bind(quote_id)
    .bind(ids.chars().take(1000).collect::<String>())
    .bind(i32::try_from(made.len()).unwrap_or(i32::MAX))
    .bind(part_keys(base, parts))
    .bind(json!(layout).to_string())
    .bind(json!(part_classes(base, parts)).to_string())
    .execute(pool)
    .await;
    if let Err(err) = result {
        tracing::error!(error = %err, "invoice split not kept");
    }
}

/// Split a quote's invoice into several in QuickBooks
///
/// Creates one `QuickBooks` invoice per split invoice, in order, each with
/// every line of the editor's invoice and its share of each priced line
/// (the shares of a line add up to it). The message of an invoice is its
/// label. When `QuickBooks` refuses one, those already made are deleted.
/// The quote keeps the first invoice's id.
#[crate::openapi::path(
    post,
    path = "/v1/customers/{id}/qbo-invoices/split",
    operation_id = "create_customer_qbo_invoice_split",
    tag = "quickbooks",
    params(("id" = i32, Path, description = "Customer id")),
    request_body = SplitCreate,
    responses(
        (status = 200, description = "The created invoices", body = SplitCreated),
        (status = 400, description = "The shares do not add up, or QuickBooks refused an invoice", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin, or the quote is another customer's", body = ErrorBody),
        (status = 404, description = "No such customer", body = ErrorBody),
        (status = 409, description = "QuickBooks is not connected", body = ErrorBody),
        (status = 500, description = "An invoice was made and could not be deleted after a later one failed", body = ErrorBody),
        (status = 502, description = "QuickBooks failed", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create_split(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(customer_id): Path<i32>,
    JsonBody(input): JsonBody<SplitCreate>,
) -> ApiResult<Json<SplitCreated>> {
    refuse_installer(&state, &user).await?;
    let planned = split(&input.invoice, &input.invoices).map_err(ApiError::BadRequest)?;
    let ready = prepare(&state, &user, customer_id, input.quote_id, &input.invoice).await?;
    let mut payloads = Vec::with_capacity(planned.len());
    for invoice in &planned {
        payloads.push(
            create_payload(
                &ready.qbo_id,
                invoice,
                ready.email.as_deref(),
                &ready.classes,
            )
            .map_err(ApiError::BadRequest)?,
        );
    }
    let count = payloads.len();
    let mut made: Vec<EditableInvoice> = Vec::with_capacity(count);
    for (index, payload) in payloads.iter().enumerate() {
        let created = state
            .qbo
            .post(&ready.access, "invoice", payload)
            .await
            .map_err(qbo_refusal)
            .and_then(|created| {
                let created = if created["Invoice"].is_object() {
                    &created["Invoice"]
                } else {
                    &created
                };
                editable(created)
                    .ok_or_else(|| ApiError::Upstream("Failed to parse created invoice".into()))
            });
        match created {
            Ok(invoice) => made.push(invoice),
            Err(err) => {
                let mut kept = Vec::new();
                for invoice in made.iter().rev() {
                    let body = json!({ "Id": invoice.id, "SyncToken": invoice.sync_token });
                    if let Err(delete) = state
                        .qbo
                        .post(&ready.access, "invoice?operation=delete", &body)
                        .await
                    {
                        tracing::error!(error = %delete, invoice = %invoice.id, "split invoice not deleted");
                        kept.push(
                            invoice
                                .doc_number
                                .clone()
                                .unwrap_or_else(|| invoice.id.clone()),
                        );
                    }
                }
                let why = err.to_string();
                let message = format!("Invoice {} of {count} was not made: {why}", index + 1);
                return Err(if kept.is_empty() {
                    let undone = if made.is_empty() {
                        String::new()
                    } else {
                        " The invoices made before it were deleted.".to_string()
                    };
                    match err {
                        ApiError::BadRequest(_) => {
                            ApiError::BadRequest(format!("{message}{undone}"))
                        }
                        ApiError::Conflict(_) => err,
                        _ => ApiError::Upstream(format!("{message}{undone}")),
                    }
                } else {
                    ApiError::Internal(format!(
                        "{message} Invoice {} could not be deleted; delete it in QuickBooks.",
                        kept.join(", ")
                    ))
                });
            }
        }
    }
    let company_id = user.company_id;
    if let (Some(quote_id), Some(first)) = (input.quote_id, made.first()) {
        sqlx::query("UPDATE quotes SET qbo_invoice_id = ? WHERE id = ? AND company_id = ?")
            .bind(&first.id)
            .bind(quote_id)
            .bind(company_id)
            .execute(&state.pool)
            .await?;
    }
    if let (Some(draft), Some(first)) = (&input.draft, made.first())
        && let Err(err) = rules::record(
            &state.pool,
            &user,
            input.quote_id,
            &first.id,
            draft,
            &input.invoice,
        )
        .await
    {
        tracing::error!(error = %err, "invoice draft outcomes not kept");
    }
    let labels: Vec<Option<String>> = input
        .invoices
        .iter()
        .map(|invoice| invoice.label.clone())
        .collect();
    remember(
        &state.pool,
        &user,
        input.quote_id,
        &input.invoice,
        &made,
        // The planned invoices keep the editor's line ids (`QuickBooks`
        // gives the made ones its own).
        &layout(&input.invoice, &planned, &labels, &input.parts),
        &input.parts,
    )
    .await;
    Ok(Json(SplitCreated { invoices: made }))
}

/// The company's usual invoice split
///
/// How the company last split the invoice of a quote with these parts
/// (else one with more parts, else any), as the percent each invoice
/// charged of each part, and the class the rep last gave each part's
/// lines. A starting point: the editor changes anything.
#[crate::openapi::path(
    get,
    path = "/v1/qbo/invoice-split",
    operation_id = "get_qbo_invoice_split",
    tag = "quickbooks",
    params(TemplateQuery),
    responses(
        (status = 200, description = "The usual split", body = SplitTemplate),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn split_template(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Query(query): Query<TemplateQuery>,
) -> ApiResult<Json<SplitTemplate>> {
    refuse_installer(&state, &user).await?;
    let wanted: BTreeSet<&str> = query
        .parts
        .as_deref()
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .collect();
    let splits: Vec<(String, String)> = sqlx::query_as(
        "SELECT part_keys, CAST(invoices_json AS CHAR) FROM qbo_invoice_splits
          WHERE company_id = ? AND invoice_count > 1 ORDER BY id DESC LIMIT 50",
    )
    .bind(user.company_id)
    .fetch_all(&state.pool)
    .await?;
    let keys_of = |keys: &str| -> BTreeSet<String> {
        keys.split(',')
            .map(str::trim)
            .filter(|key| !key.is_empty())
            .map(String::from)
            .collect()
    };
    let wanted_owned: BTreeSet<String> = wanted.iter().map(|key| (*key).to_string()).collect();
    let chosen = splits
        .iter()
        .find(|(keys, _)| keys_of(keys) == wanted_owned)
        .or_else(|| {
            splits
                .iter()
                .find(|(keys, _)| keys_of(keys).is_superset(&wanted_owned))
        })
        .or_else(|| splits.first());
    let invoices = chosen.and_then(|(_, raw)| serde_json::from_str::<Vec<LayoutInvoice>>(raw).ok());
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT CAST(classes_json AS CHAR) FROM qbo_invoice_splits
          WHERE company_id = ? AND user_id = ? ORDER BY id DESC LIMIT 20",
    )
    .bind(user.company_id)
    .bind(user.id)
    .fetch_all(&state.pool)
    .await?;
    let mut classes: BTreeMap<String, Option<String>> = BTreeMap::new();
    for (raw,) in rows {
        let Ok(kept) = serde_json::from_str::<BTreeMap<String, Option<String>>>(&raw) else {
            continue;
        };
        for (key, class) in kept {
            classes.entry(key).or_insert(class);
        }
    }
    Ok(Json(SplitTemplate { invoices, classes }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sale(id: &str, description: &str, amount: f64) -> Line {
        Line {
            id: Some(id.into()),
            line_num: None,
            detail_type: "SalesItemLineDetail".into(),
            description: Some(description.into()),
            amount: Some(amount),
            qty: Some(1.0),
            unit_price: Some(amount),
            item_ref_value: Some("1".into()),
            item_ref_name: None,
            editable: true,
            class_name: None,
        }
    }

    fn base() -> EditableInvoice {
        let mut note = sale("new:2", "Flat Edge", 0.0);
        note.detail_type = "DescriptionOnly".into();
        let mut cabinets = sale("new:3", "Cabinets - Shaker White", 12_465.0);
        cabinets.class_name = Some("Cabinets:Tania".into());
        EditableInvoice {
            id: String::new(),
            sync_token: String::new(),
            doc_number: None,
            txn_date: Some("2026-11-03".into()),
            due_date: None,
            total_amt: Some(18_994.0),
            balance: None,
            currency: None,
            customer_memo: Some("Payment Terms".into()),
            private_note: Some("Quote: Kitchen".into()),
            bill_addr: None,
            ship_addr: None,
            lines: vec![
                sale("new:1", "Kitchen based @ 37.1 sqft", 6_529.0),
                note,
                cabinets,
            ],
            class_name: Some("Countertops:Tania".into()),
            bill_email_cc: None,
            bill_email_bcc: None,
        }
    }

    fn share(line_id: &str, amount: f64) -> Share {
        Share {
            line_id: line_id.into(),
            amount,
        }
    }

    fn three() -> Vec<SplitInvoice> {
        vec![
            SplitInvoice {
                label: Some("Deposit".into()),
                shares: vec![share("new:1", 3_264.5), share("new:3", 6_232.5)],
            },
            SplitInvoice {
                label: Some(" Due the day the cabinets go in ".into()),
                shares: vec![share("new:3", 3_116.25)],
            },
            SplitInvoice {
                label: None,
                shares: vec![share("new:1", 3_264.5), share("new:3", 3_116.25)],
            },
        ]
    }

    #[test]
    fn each_invoice_keeps_every_line_and_charges_its_share() {
        let made = split(&base(), &three()).unwrap();
        assert_eq!(made.len(), 3);
        assert_eq!(made[0].total_amt, Some(9_497.0));
        assert_eq!(made[1].total_amt, Some(3_116.25));
        assert_eq!(made[2].total_amt, Some(6_380.75));
        // The countertops are $0 on the cabinets' invoice, still listed.
        assert_eq!(made[1].lines.len(), 3);
        assert_eq!(made[1].lines[0].amount, Some(0.0));
        assert_eq!(made[1].lines[2].unit_price, Some(3_116.25));
        assert_eq!(
            made[1].lines[2].class_name.as_deref(),
            Some("Cabinets:Tania")
        );
        assert_eq!(made[0].customer_memo.as_deref(), Some("Deposit"));
        assert_eq!(
            made[1].customer_memo.as_deref(),
            Some("Due the day the cabinets go in")
        );
        assert_eq!(made[2].customer_memo.as_deref(), Some("Payment Terms"));
        assert_eq!(
            made[2].private_note.as_deref(),
            Some("Quote: Kitchen · Invoice 3 of 3")
        );
    }

    #[test]
    fn shares_must_add_up_to_each_line() {
        let mut invoices = three();
        invoices[2].shares[1].amount = 3_000.0;
        assert_eq!(
            split(&base(), &invoices).unwrap_err(),
            "\"Cabinets - Shaker White\" is split into $12348.75, not its $12465.00."
        );
        let mut invoices = three();
        invoices[1].shares.clear();
        invoices[2].shares[1].amount = 6_232.5;
        assert_eq!(
            split(&base(), &invoices).unwrap_err(),
            "Invoice 2 charges nothing."
        );
        let mut invoices = three();
        invoices[0].shares.push(share("new:9", 1.0));
        assert!(split(&base(), &invoices).is_err());
        assert!(split(&base(), &three()[..1]).is_err());
    }

    #[test]
    fn the_layout_is_kept_by_part_in_percent() {
        let base = base();
        let made = split(&base, &three()).unwrap();
        let parts = vec![
            Part {
                line_id: "new:1".into(),
                key: "stone".into(),
            },
            Part {
                line_id: "new:3".into(),
                key: "area".into(),
            },
        ];
        let labels: Vec<Option<String>> =
            three().into_iter().map(|invoice| invoice.label).collect();
        let kept = layout(&base, &made, &labels, &parts);
        assert_eq!(kept[0].shares["stone"], 50.0);
        assert_eq!(kept[0].shares["area"], 50.0);
        assert_eq!(kept[1].shares["stone"], 0.0);
        assert_eq!(kept[1].shares["area"], 25.0);
        assert_eq!(kept[2].shares["area"], 25.0);
        assert_eq!(kept[0].label.as_deref(), Some("Deposit"));
        assert_eq!(kept[2].label, None);
        assert_eq!(part_keys(&base, &parts), "area,stone");
        let classes = part_classes(&base, &parts);
        assert_eq!(classes["area"].as_deref(), Some("Cabinets:Tania"));
        assert_eq!(classes["stone"], None);
    }
}
