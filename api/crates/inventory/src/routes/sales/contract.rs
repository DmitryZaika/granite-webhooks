//! `GET /v1/sales/{id}/contract`: the sale's contract, the homeowner or
//! commercial template filled in with the sale.
//!
//! Port of the Remix `api.pdf.$saleId` route: the same fields get the same
//! text. pdf-lib drew each field's appearance; here [`fill_form`] sets the
//! values and draws plain Helvetica appearances.

use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::pdf::winansi;
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::http::header;
use axum::response::IntoResponse;
use chrono::{DateTime, Utc};
use lopdf::{Dictionary, Document, Object, ObjectId, Stream, StringFormat};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::time::Duration;

const HOMEOWNER: &str = "base-contracts/Contract.pdf";
const COMMERCIAL: &str = "base-contracts/Contract_commercial.pdf";
const MAX_ROOMS: usize = 3;
/// Above this price (USD) the contract asks for half as the deposit, else all of it.
pub const HALF_DEPOSIT_OVER: f64 = 1000.0;

#[derive(sqlx::FromRow)]
struct SaleRow {
    sale_date: Option<DateTime<Utc>>,
    project_address: Option<String>,
    total_price: Option<String>,
    customer_name: Option<String>,
    phone: Option<String>,
    email: Option<String>,
    seller_name: Option<String>,
    room: Option<String>,
    backsplash: Option<String>,
    square_feet: Option<String>,
    tear_out: Option<String>,
    stove: Option<String>,
    waterfall: Option<String>,
    corbels: Option<i64>,
    seam: Option<String>,
    extras: Option<String>,
    stone_name: Option<String>,
    retail_price: Option<String>,
    company_name: Option<String>,
    billing_address: Option<String>,
    room_uuid: String,
}

#[derive(sqlx::FromRow)]
struct CountRow {
    name: String,
    count: i64,
    room_uuid: String,
}

/// The CRM's `replaceZero`: `45.00` → `45`; other text as given.
pub fn replace_zero(value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    let Ok(number) = value.trim().parse::<f64>() else {
        return value.to_string();
    };
    let rounded = (number * 100.0).round() / 100.0;
    if rounded.fract() == 0.0 || value.ends_with(".00") {
        rounded.to_string()
    } else {
        value.to_string()
    }
}

/// `charAt(0).toUpperCase() + slice(1).toLowerCase()`.
fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
    })
}

fn pretty_counts(items: &[&CountRow]) -> String {
    if items.is_empty() {
        return "N/A".into();
    }
    items
        .iter()
        .map(|item| {
            if item.count > 1 {
                format!("{} X {}", item.name, item.count)
            } else {
                item.name.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn extras(row: &SaleRow) -> Value {
    row.extras
        .as_deref()
        .and_then(|text| serde_json::from_str(text).ok())
        .unwrap_or(Value::Null)
}

fn edge(row: &SaleRow) -> String {
    match &extras(row)["edge_price"] {
        Value::Object(edge) => edge
            .get("edge_type")
            .and_then(Value::as_str)
            .filter(|edge| !edge.is_empty())
            .map_or_else(|| "N/A".into(), capitalized),
        _ => "N/A".into(),
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// The field values, homeowner (`Text1`…) or commercial (`Text123`…).
#[allow(clippy::too_many_lines)]
fn field_values(
    rows: &[SaleRow],
    sinks: &[CountRow],
    faucets: &[CountRow],
) -> Vec<(String, Option<String>)> {
    let first = &rows[0];
    let commercial = first
        .company_name
        .as_deref()
        .is_some_and(|name| !name.is_empty());
    let mut values: Vec<(String, Option<String>)> = Vec::new();
    let mut set = |name: &str, value: Option<String>| values.push((name.to_string(), value));
    let non_empty = |value: &Option<String>| value.clone().filter(|value| !value.is_empty());
    let date = first
        .sale_date
        .map(|date| date.format("%-m/%-d/%Y").to_string());
    let mut seen = HashSet::new();
    let unique: Vec<&SaleRow> = rows
        .iter()
        .filter(|row| seen.insert(row.room_uuid.clone()))
        .collect();
    let room_text = |row: &SaleRow| non_empty(&row.room).map(|room| capitalized(&room));
    let backsplash = |row: &SaleRow| {
        non_empty(&row.backsplash).map_or_else(|| "N/A".into(), |text| capitalized(&text))
    };
    let in_room = |items: &'_ [CountRow], room: &str| -> String {
        let matching: Vec<&CountRow> = items.iter().filter(|item| item.room_uuid == room).collect();
        pretty_counts(&matching)
    };
    let number_or_na =
        |value: &Option<String>| replace_zero(non_empty(value).as_deref().unwrap_or("N/A"));
    if commercial {
        set("Text123", date);
        set("Text124", non_empty(&first.seller_name));
        set("Text125", non_empty(&first.customer_name));
        set("Text126", non_empty(&first.company_name));
        set("Text127", non_empty(&first.project_address));
        set("Text128", non_empty(&first.billing_address));
        set("Text129", non_empty(&first.phone));
        set("Text130", non_empty(&first.email));
        let fields = [
            [
                "Text131", "Text132", "Text133", "Text134", "Text135", "Text146", "Text147",
            ],
            [
                "Text136", "Text137", "Text138", "Text139", "Text140", "Text148", "Text149",
            ],
            [
                "Text141", "Text142", "Text143", "Text144", "Text145", "Text150", "Text151",
            ],
        ];
        for (row, [room, color, sink, edge_field, back, sqft, price]) in
            unique.iter().take(MAX_ROOMS).zip(fields)
        {
            set(room, room_text(row));
            set(
                color,
                Some(non_empty(&row.stone_name).unwrap_or_else(|| "N/A".into())),
            );
            set(sink, Some(in_room(sinks, &row.room_uuid)));
            set(edge_field, Some(edge(row)));
            set(back, Some(backsplash(row)));
            set(sqft, Some(number_or_na(&row.square_feet)));
            set(price, Some(number_or_na(&row.retail_price)));
        }
    } else {
        set("Text1", date);
        set("Text2", non_empty(&first.seller_name));
        set("Text3", non_empty(&first.customer_name));
        set("Text4", non_empty(&first.project_address));
        set("Text5", non_empty(&first.phone));
        set("Text6", non_empty(&first.email));
        for (index, row) in unique.iter().enumerate() {
            let field = |offset: usize| format!("Text{}", offset + index * 6);
            set(&field(7), room_text(row));
            set(
                &field(8),
                Some(non_empty(&row.stone_name).unwrap_or_else(|| "N/A".into())),
            );
            set(&field(9), Some(in_room(sinks, &row.room_uuid)));
            set(&field(10), Some(in_room(faucets, &row.room_uuid)));
            set(&field(11), Some(edge(row)));
            set(&field(12), Some(backsplash(row)));
            set(
                &format!("Text{}", 25 + index * 2),
                Some(number_or_na(&row.square_feet)),
            );
            set(
                &format!("Text{}", 26 + index * 2),
                Some(number_or_na(&row.retail_price)),
            );
        }
    }
    let corbels: i64 = unique.iter().map(|row| row.corbels.unwrap_or(0)).sum();
    let yes_no = |flag: bool| Some(if flag { "Yes" } else { "No" }.to_string());
    let laminate_tear_out = rows
        .iter()
        .any(|row| matches!(row.tear_out.as_deref(), Some("laminate_t/o" | "vanity_t/o")));
    let stone_tear_out = rows
        .iter()
        .any(|row| row.tear_out.as_deref() == Some("stone_t/o"));
    let sealer = rows
        .iter()
        .any(|row| truthy(&extras(row)["ten_year_sealer"]));
    let waterfall = rows
        .iter()
        .any(|row| row.waterfall.as_deref() == Some("yes"));
    let stove = match first.stove.as_deref() {
        Some("f/s") => "F/S",
        Some("s/i") => "S/I",
        Some("c/t") => "C/T",
        Some("grill") => "Grill",
        _ => "N/A",
    };
    let seam = match first.seam.as_deref().unwrap_or("").to_lowercase().as_str() {
        "standard" => "STD",
        "phantom" => "SPH",
        "extended" => "EXT",
        "european" => "EU",
        "none!" => "NONE",
        _ => "N/A",
    };
    let full = replace_zero(non_empty(&first.total_price).as_deref().unwrap_or("0"));
    let full_number = full.trim().parse::<f64>().unwrap_or(f64::NAN);
    let deposit = if full_number > HALF_DEPOSIT_OVER {
        (full_number * 0.5).to_string()
    } else {
        full.clone()
    };
    let names: [&str; 9] = if commercial {
        [
            "Text152",
            "Text153",
            "Text154",
            "Text155",
            "Text156",
            "Text157",
            "Dropdown159",
            "Text160",
            "Text161",
        ]
    } else {
        [
            "Text31",
            "Text32",
            "Text33",
            "Text34",
            "Text35",
            "Text36",
            "Dropdown42",
            "Text37",
            "Text38",
        ]
    };
    set(names[0], yes_no(laminate_tear_out));
    set(names[1], yes_no(stone_tear_out));
    set(names[2], Some(stove.into()));
    set(names[3], yes_no(sealer));
    set(names[4], yes_no(waterfall));
    set(names[5], Some(corbels.to_string()));
    set(names[6], Some(seam.into()));
    set(names[7], Some(full));
    set(names[8], Some(deposit));
    values
}

/// A PDF text string: plain bytes for ASCII, else UTF-16BE with a BOM.
fn text_string(text: &str) -> Object {
    if text.is_ascii() {
        return Object::String(text.as_bytes().to_vec(), StringFormat::Literal);
    }
    let mut bytes = vec![0xFE, 0xFF];
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_be_bytes());
    }
    Object::String(bytes, StringFormat::Hexadecimal)
}

fn dict_of<'a>(doc: &'a Document, object: &'a Object) -> Option<&'a Dictionary> {
    doc.dereference(object).ok()?.1.as_dict().ok()
}

const fn number(object: &Object) -> Option<f32> {
    match object {
        Object::Integer(value) => {
            #[allow(clippy::cast_precision_loss)]
            let value = *value as f32;
            Some(value)
        }
        Object::Real(value) => Some(*value),
        _ => None,
    }
}

/// Fully qualified field name → (field object, its widget objects).
fn fields(doc: &Document) -> HashMap<String, (ObjectId, Vec<ObjectId>)> {
    let mut found = HashMap::new();
    let Ok(catalog) = doc.catalog() else {
        return found;
    };
    let Some(form) = catalog
        .get(b"AcroForm")
        .ok()
        .and_then(|form| dict_of(doc, form))
    else {
        return found;
    };
    let mut stack: Vec<(ObjectId, String)> = form
        .get(b"Fields")
        .ok()
        .and_then(|fields| doc.dereference(fields).ok())
        .and_then(|(_, fields)| fields.as_array().ok())
        .into_iter()
        .flatten()
        .filter_map(|field| field.as_reference().ok())
        .map(|id| (id, String::new()))
        .collect();
    while let Some((id, parent)) = stack.pop() {
        let Ok(field) = doc.get_dictionary(id) else {
            continue;
        };
        let partial = field
            .get(b"T")
            .ok()
            .and_then(|t| t.as_str().ok())
            .map(|t| String::from_utf8_lossy(t).into_owned());
        let name = match (&partial, parent.is_empty()) {
            (Some(partial), true) => partial.clone(),
            (Some(partial), false) => format!("{parent}.{partial}"),
            (None, _) => parent.clone(),
        };
        let kids: Vec<ObjectId> = field
            .get(b"Kids")
            .ok()
            .and_then(|kids| kids.as_array().ok())
            .into_iter()
            .flatten()
            .filter_map(|kid| kid.as_reference().ok())
            .collect();
        let mut widgets = Vec::new();
        if field.has(b"Rect") {
            widgets.push(id);
        }
        for kid in kids {
            let is_field = doc.get_dictionary(kid).is_ok_and(|kid| kid.has(b"T"));
            if is_field {
                stack.push((kid, name.clone()));
            } else {
                widgets.push(kid);
            }
        }
        if partial.is_some() {
            found.insert(name, (id, widgets));
        }
    }
    found
}

/// The `Helv` font of the form's default resources, added when missing.
fn form_font(doc: &mut Document) -> Option<ObjectId> {
    let form_id = doc
        .catalog()
        .ok()?
        .get(b"AcroForm")
        .ok()?
        .as_reference()
        .ok();
    let existing = {
        let form = match form_id {
            Some(id) => doc.get_dictionary(id).ok()?,
            None => doc.catalog().ok()?.get(b"AcroForm").ok()?.as_dict().ok()?,
        };
        form.get(b"DR")
            .ok()
            .and_then(|resources| dict_of(doc, resources))
            .and_then(|resources| resources.get(b"Font").ok())
            .and_then(|fonts| dict_of(doc, fonts))
            .and_then(|fonts| fonts.get(b"Helv").ok())
            .and_then(|font| font.as_reference().ok())
    };
    if existing.is_some() {
        return existing;
    }
    let mut font = Dictionary::new();
    font.set("Type", Object::Name(b"Font".to_vec()));
    font.set("Subtype", Object::Name(b"Type1".to_vec()));
    font.set("BaseFont", Object::Name(b"Helvetica".to_vec()));
    font.set("Encoding", Object::Name(b"WinAnsiEncoding".to_vec()));
    Some(doc.add_object(font))
}

fn escape_literal(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 2);
    for byte in bytes {
        if matches!(byte, b'(' | b')' | b'\\') {
            out.push(b'\\');
        }
        out.push(*byte);
    }
    out
}

/// A single line of Helvetica 12 inside the widget, vertically centered.
/// The font size of a `DA` such as `/Helv 9 Tf 0 g`; 0 (auto) and a
/// missing size read as 12.
fn da_size(da: Option<&[u8]>) -> f32 {
    let da = String::from_utf8_lossy(da.unwrap_or_default()).into_owned();
    let words: Vec<&str> = da.split_whitespace().collect();
    words
        .iter()
        .position(|word| *word == "Tf")
        .and_then(|at| at.checked_sub(1))
        .and_then(|at| words[at].parse::<f32>().ok())
        .filter(|size| *size > 0.0)
        .unwrap_or(12.0)
}

/// A single line of Helvetica at the field's `DA` size inside the widget,
/// vertically centered and clipped to the box.
fn appearance(
    doc: &mut Document,
    widget: ObjectId,
    text: &str,
    font: ObjectId,
    size: f32,
) -> Option<()> {
    let rect: Vec<f32> = doc
        .get_dictionary(widget)
        .ok()?
        .get(b"Rect")
        .ok()?
        .as_array()
        .ok()?
        .iter()
        .filter_map(number)
        .collect();
    let [x1, y1, x2, y2] = rect.as_slice() else {
        return None;
    };
    let (width, height) = ((x2 - x1).abs(), (y2 - y1).abs());
    // Helvetica's cap height (0.718 em) centered in the box.
    let baseline = size.mul_add(-0.718, height) / 2.0;
    let mut content = format!(
        "/Tx BMC\nq\n1 1 {:.2} {:.2} re W n\nBT\n/Helv {size} Tf\n0 g\n2 {baseline:.2} Td\n(",
        width - 2.0,
        height - 2.0
    )
    .into_bytes();
    content.extend(escape_literal(&winansi(text)));
    content.extend_from_slice(b") Tj\nET\nQ\nEMC\n");
    let mut fonts = Dictionary::new();
    fonts.set("Helv", Object::Reference(font));
    let mut resources = Dictionary::new();
    resources.set("Font", Object::Dictionary(fonts));
    let mut stream_dict = Dictionary::new();
    stream_dict.set("Type", Object::Name(b"XObject".to_vec()));
    stream_dict.set("Subtype", Object::Name(b"Form".to_vec()));
    stream_dict.set(
        "BBox",
        Object::Array(vec![
            0.into(),
            0.into(),
            Object::Real(width),
            Object::Real(height),
        ]),
    );
    stream_dict.set("Resources", Object::Dictionary(resources));
    let stream = doc.add_object(Stream::new(stream_dict, content));
    let mut ap = Dictionary::new();
    ap.set("N", Object::Reference(stream));
    doc.get_dictionary_mut(widget)
        .ok()?
        .set("AP", Object::Dictionary(ap));
    Some(())
}

/// Sets the fields' values (a `None` clears one) and draws them.
pub fn fill_form(template: &[u8], values: &[(String, Option<String>)]) -> Result<Vec<u8>, String> {
    let mut doc = Document::load_mem(template).map_err(|err| err.to_string())?;
    let fields = fields(&doc);
    let font = form_font(&mut doc).ok_or("the template has no form")?;
    for (name, value) in values {
        let Some((field, widgets)) = fields.get(name) else {
            continue;
        };
        let dict = doc
            .get_dictionary_mut(*field)
            .map_err(|err| err.to_string())?;
        match value {
            Some(text) => dict.set("V", text_string(text)),
            None => {
                dict.remove(b"V");
            }
        }
        let size = da_size(
            doc.get_dictionary(*field)
                .ok()
                .and_then(|field| field.get(b"DA").ok())
                .and_then(|da| da.as_str().ok()),
        );
        for widget in widgets {
            appearance(
                &mut doc,
                *widget,
                value.as_deref().unwrap_or(""),
                font,
                size,
            );
        }
    }
    let mut out = Vec::new();
    doc.save_to(&mut out).map_err(|err| err.to_string())?;
    Ok(out)
}

async fn template(state: &AppState, key: &str) -> Result<Vec<u8>, ApiError> {
    let response = state
        .telephony_http
        .get(format!("{}/{key}", state.contract_templates_base))
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|err| ApiError::Upstream(err.without_url().to_string()))?;
    if !response.status().is_success() {
        return Err(ApiError::Upstream(format!(
            "contract template: {}",
            response.status()
        )));
    }
    Ok(response
        .bytes()
        .await
        .map_err(|err| ApiError::Upstream(err.without_url().to_string()))?
        .to_vec())
}

/// The CRM's `sanitizeFilename`.
fn file_name(name: &str) -> String {
    let kept: String = name
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || ch.is_whitespace() || *ch == '-')
        .collect();
    kept.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Get a sale's contract
///
/// The homeowner contract (or the commercial one when the customer has a
/// company name) filled in with the sale: customer, rooms (at most three),
/// stone, sinks, faucets, edges, extras and the price with its deposit.
#[crate::openapi::path(
    get,
    path = "/v1/sales/{id}/contract",
    operation_id = "get_sale_contract",
    tag = "sales",
    params(("id" = i32, Path, description = "Sale id")),
    responses(
        (status = 200, description = "The contract", content_type = "application/pdf", body = Vec<u8>),
        (status = 400, description = "More than three rooms", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such sale (with slabs) in the caller's company", body = ErrorBody),
        (status = 502, description = "The template could not be read", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn contract(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(sale_id): Path<i32>,
) -> ApiResult<impl IntoResponse> {
    let rows: Vec<SaleRow> = sqlx::query_as(
        "SELECT sales.sale_date, sales.project_address, CAST(sales.price AS CHAR) AS total_price,
                customers.name AS customer_name, customers.phone, primary_email.email AS email,
                users.name AS seller_name, slab_inventory.room, slab_inventory.backsplash,
                CAST(slab_inventory.square_feet AS CHAR) AS square_feet, slab_inventory.tear_out,
                slab_inventory.stove, slab_inventory.waterfall, CAST(slab_inventory.corbels AS SIGNED) AS corbels,
                slab_inventory.seam, CAST(slab_inventory.extras AS CHAR) AS extras, stones.name AS stone_name,
                CAST(slab_inventory.price AS CHAR) AS retail_price, customers.company_name,
                customers.address AS billing_address, COALESCE(HEX(slab_inventory.room_uuid), '') AS room_uuid
           FROM sales
           JOIN customers ON customers.id = sales.customer_id
           LEFT JOIN customers_emails primary_email
             ON primary_email.id = customers.email_id AND primary_email.customer_id = customers.id
           JOIN users ON users.id = sales.seller_id
           JOIN slab_inventory ON slab_inventory.sale_id = sales.id
           JOIN stones ON stones.id = slab_inventory.stone_id
          WHERE sales.id = ? AND customers.company_id = ?
          ORDER BY slab_inventory.id",
    )
    .bind(sale_id)
    .bind(user.company_id)
    .fetch_all(&state.pool)
    .await?;
    if rows.is_empty() {
        return Err(ApiError::NotFound);
    }
    if rows
        .iter()
        .map(|row| &row.room_uuid)
        .collect::<HashSet<_>>()
        .len()
        > MAX_ROOMS
    {
        return Err(ApiError::BadRequest("Current limit is three items".into()));
    }
    let counts = |table: &str, type_table: &str, column: &str| {
        format!(
            "SELECT {type_table}.name AS name, COUNT({table}.id) AS count, COALESCE(HEX(slab_inventory.room_uuid), '') AS room_uuid
               FROM {table}
               JOIN slab_inventory ON slab_inventory.id = {table}.slab_id
               JOIN {type_table} ON {type_table}.id = {table}.{column}
               JOIN sales ON sales.id = slab_inventory.sale_id
               JOIN customers ON customers.id = sales.customer_id
              WHERE slab_inventory.sale_id = ? AND customers.company_id = ?
              GROUP BY {type_table}.name, slab_inventory.room_uuid"
        )
    };
    let sinks: Vec<CountRow> = sqlx::query_as(sqlx::AssertSqlSafe(counts(
        "sinks",
        "sink_type",
        "sink_type_id",
    )))
    .bind(sale_id)
    .bind(user.company_id)
    .fetch_all(&state.pool)
    .await?;
    let faucets: Vec<CountRow> = sqlx::query_as(sqlx::AssertSqlSafe(counts(
        "faucets",
        "faucet_type",
        "faucet_type_id",
    )))
    .bind(sale_id)
    .bind(user.company_id)
    .fetch_all(&state.pool)
    .await?;
    let commercial = rows[0]
        .company_name
        .as_deref()
        .is_some_and(|name| !name.is_empty());
    let template = template(&state, if commercial { COMMERCIAL } else { HOMEOWNER }).await?;
    let values = field_values(&rows, &sinks, &faucets);
    let bytes = fill_form(&template, &values).map_err(ApiError::Internal)?;
    let name = file_name(
        rows[0]
            .customer_name
            .as_deref()
            .filter(|name| !name.is_empty())
            .unwrap_or("Customer"),
    );
    Ok((
        [
            (header::CONTENT_TYPE, "application/pdf".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("inline; filename=\"{name}.pdf\""),
            ),
        ],
        bytes,
    ))
}

#[cfg(test)]
mod tests {
    use super::{da_size, file_name, replace_zero};

    #[test]
    fn numbers_and_names_read_like_the_crm() {
        assert_eq!(replace_zero("45.00"), "45");
        assert_eq!(replace_zero("45.50"), "45.50");
        assert_eq!(replace_zero("N/A"), "N/A");
        assert_eq!(replace_zero("1500"), "1500");
        assert!((da_size(Some(b"/Helv 9 Tf 0 g".as_slice())) - 9.0).abs() < f32::EPSILON);
        assert!((da_size(Some(b"/Helv 0 Tf 0 g".as_slice())) - 12.0).abs() < f32::EPSILON);
        assert_eq!(file_name(" Zoë  O'Brien-Smith "), "Zo OBrien-Smith");
    }
}
