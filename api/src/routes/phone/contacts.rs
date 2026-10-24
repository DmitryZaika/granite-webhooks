//! Contacts for the phone app: the offline index, number lookups, the
//! contact card, new customers and added numbers.

use super::bad_request;
use crate::auth::device::DeviceUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::{JsonBody, MultiQuery};
use crate::phone::calls::{self, CallRow};
use crate::phone::contacts::{
    self, CardDeal, Coworker, Cursor, INDEX_PAGE, IndexContact, SAMPLE_CUSTOMER_ID,
};
use crate::phone::customer_index;
use crate::phone::numbers::{is_nanp, last10, to_e164};
use crate::routes::customers::phones::storage_format;
use crate::routes::customers::service::{
    audit_name, open_deal_in, record_reassignment, start_drips,
};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::header::{CACHE_CONTROL, ETAG, IF_NONE_MATCH};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use utoipa::{IntoParams, ToSchema};

/// Most numbers one lookup takes.
const MAX_LOOKUP: usize = 20;
/// Calls on a contact card.
const CARD_CALLS: usize = 20;

/// Query of `GET /v1/phone/contacts/index`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct IndexQuery {
    /// The `cursor` of the last sync: only what changed since (a too old or
    /// unreadable cursor gets a full download instead).
    pub since: Option<String>,
    /// Pages a full download: the `nextAfter` of the previous page.
    pub after: Option<i64>,
}

/// `GET /v1/phone/contacts/index`.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContactIndex {
    /// True: a full download; replace the phone's copy with this page and
    /// the following ones (`nextAfter`). False: changes to apply.
    pub full: bool,
    /// Contacts, by id: a page of the full download, or the ones changed,
    /// added or called since the cursor.
    pub contacts: Vec<IndexContact>,
    /// Since-sync only: ids of customers deleted since the cursor.
    pub deleted: Vec<i32>,
    /// Full download: pass as `?after=` for the next page; null on the last
    /// page (and for a since-sync).
    pub next_after: Option<i64>,
    /// Pass as `?since=` next time (opaque). Keep the one of a full
    /// download's first page.
    pub cursor: String,
    /// Coworkers with a direct line (first page and since-sync only; empty
    /// on later pages).
    pub coworkers: Vec<Coworker>,
}

/// The `If-None-Match` header names `etag` (quoted or not, weak or not).
fn etag_matches(headers: &HeaderMap, etag: &str) -> bool {
    let bare = etag.trim_matches('"');
    headers
        .get_all(IF_NONE_MATCH)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|tag| tag.trim().trim_start_matches("W/").trim_matches('"'))
        .any(|tag| tag == bare || tag == "*")
}

/// The first page (or a later one) of a full download.
fn full_page(mut contacts: Vec<IndexContact>) -> (Vec<IndexContact>, Option<i64>) {
    let next_after = if contacts.len() > INDEX_PAGE {
        contacts.truncate(INDEX_PAGE);
        contacts.last().map(|contact| i64::from(contact.id))
    } else {
        None
    };
    (contacts, next_after)
}

/// Download the contact index
///
/// The compact index of the company's customers (all live ones but the
/// onboarding sample) and coworkers, for search on the phone. Without
/// parameters: the first page of a full download (5,000 contacts by id;
/// follow `nextAfter` with `?after=`). With `?since=<cursor>`: the customers
/// changed, added or called since, and the ids deleted since (a full
/// download when the cursor is too old or there are more than 5,000
/// changes). Deal changes (open deal, stage) reach the phone with a full
/// download, which the app does daily, so a full download always answers
/// the index. `If-None-Match` with the ETag of the last answer (or
/// `GET /v1/phone/bootstrap` `contacts.etag`) answers 304 when nothing
/// changed, on a since-sync only. Later pages (`after`) carry no `ETag`.
/// Device token only.
#[utoipa::path(
    get,
    path = "/v1/phone/contacts/index",
    operation_id = "get_phone_contact_index",
    tag = "phone",
    params(
        IndexQuery,
        ("If-None-Match" = Option<String>, Header, description = "The ETag of the phone's copy: 304 when the index is unchanged (since-syncs only)"),
    ),
    responses(
        (status = 200, description = "Contacts (the `ETag` header is the index version; none on `after` pages)", body = ContactIndex),
        (status = 304, description = "Unchanged since the `If-None-Match` ETag (a since-sync)"),
        (status = 400, description = "Malformed query", body = ErrorBody),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn contact_index(
    State(state): State<AppState>,
    device: DeviceUser,
    headers: HeaderMap,
    MultiQuery(query): MultiQuery<IndexQuery>,
) -> ApiResult<Response> {
    let user = &device.user;
    let pool = &state.pool;
    let now = Utc::now();
    if let Some(after) = query.after {
        // A later page: the index's version is not needed (the app keeps the
        // first page's cursor and ETag).
        let cursor = Cursor::now(now, contacts::max_id(pool, user.company_id).await?).encode();
        let after = i32::try_from(after.max(0)).unwrap_or(i32::MAX);
        let rows = contacts::page(pool, user.company_id, user.id, after, INDEX_PAGE + 1).await?;
        let (contacts, next_after) = full_page(rows);
        let body = ContactIndex {
            full: true,
            contacts,
            deleted: Vec::new(),
            next_after,
            cursor,
            coworkers: Vec::new(),
        };
        return Ok(([(CACHE_CONTROL, "private, no-cache")], Json(body)).into_response());
    }
    let coworkers = contacts::coworkers(pool, user.company_id, user.id).await?;
    let version = contacts::version(pool, user.company_id, user.id, &coworkers).await?;
    let headers_out = [
        (ETAG, version.etag.clone()),
        (CACHE_CONTROL, "private, no-cache".to_string()),
    ];
    let since = query
        .since
        .as_deref()
        .and_then(|since| Cursor::parse(since, now));
    // A full download always answers: deal changes reach the phone only so.
    if since.is_some() && etag_matches(&headers, &version.etag) {
        return Ok((StatusCode::NOT_MODIFIED, headers_out).into_response());
    }
    let cursor = Cursor::now(now, version.max_id).encode();
    if let Some(since) = since {
        let changed =
            contacts::changed_since(pool, user.company_id, user.id, since, INDEX_PAGE + 1).await?;
        if changed.len() <= INDEX_PAGE {
            let deleted = contacts::deleted_since(pool, user.company_id, since).await?;
            let body = ContactIndex {
                full: false,
                contacts: changed,
                deleted,
                next_after: None,
                cursor,
                coworkers,
            };
            return Ok((headers_out, Json(body)).into_response());
        }
    }
    let rows = contacts::page(pool, user.company_id, user.id, 0, INDEX_PAGE + 1).await?;
    let (contacts, next_after) = full_page(rows);
    let body = ContactIndex {
        full: true,
        contacts,
        deleted: Vec::new(),
        next_after,
        cursor,
        coworkers,
    };
    Ok((headers_out, Json(body)).into_response())
}

/// Body of `POST /v1/phone/contacts/lookup`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct LookupInput {
    /// Up to 20 numbers, in any format (matched by their last 10 digits).
    pub numbers: Vec<String>,
}

/// Who one number is.
#[derive(Debug, Serialize, ToSchema)]
pub struct LookupMatch {
    /// The number as sent.
    pub number: String,
    /// The newest customer with that phone or phone 2; null when none.
    pub contact: Option<IndexContact>,
    /// The coworker whose direct line it is; null when none.
    pub coworker: Option<Coworker>,
}

/// `POST /v1/phone/contacts/lookup`.
#[derive(Debug, Serialize, ToSchema)]
pub struct ContactLookup {
    /// One match per number, in the order sent.
    pub matches: Vec<LookupMatch>,
}

/// Look numbers up
///
/// Who each number is: the newest customer of the company with that phone
/// or phone 2, and the coworker whose direct line it is (matched by the
/// last 10 digits; numbers with fewer digits match nothing). Up to 20
/// numbers. Device token only.
#[utoipa::path(
    post,
    path = "/v1/phone/contacts/lookup",
    operation_id = "lookup_phone_contacts",
    tag = "phone",
    request_body = LookupInput,
    responses(
        (status = 200, description = "One match per number", body = ContactLookup),
        (status = 400, description = "More than 20 numbers", body = ErrorBody),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn lookup_contacts(
    State(state): State<AppState>,
    device: DeviceUser,
    JsonBody(input): JsonBody<LookupInput>,
) -> ApiResult<Json<ContactLookup>> {
    if input.numbers.len() > MAX_LOOKUP {
        return Err(ApiError::BadRequest("At most 20 numbers".into()));
    }
    let user = &device.user;
    let pool = &state.pool;
    let keys: Vec<Option<String>> = input.numbers.iter().map(|number| last10(number)).collect();
    let mut wanted: Vec<String> = keys.iter().flatten().cloned().collect();
    wanted.sort();
    wanted.dedup();
    let customers = contacts::customers_for_numbers(pool, user.company_id, &wanted).await?;
    let mut ids: Vec<i32> = customers.values().copied().collect();
    ids.sort_unstable();
    ids.dedup();
    let found: HashMap<i32, IndexContact> = contacts::by_ids(pool, user.company_id, user.id, &ids)
        .await?
        .into_iter()
        .map(|contact| (contact.id, contact))
        .collect();
    let coworkers = contacts::coworkers(pool, user.company_id, user.id).await?;
    let matches = input
        .numbers
        .into_iter()
        .zip(keys)
        .map(|(number, key)| {
            let contact = key
                .as_ref()
                .and_then(|key| customers.get(key))
                .and_then(|id| found.get(id))
                .cloned();
            let coworker = key
                .as_ref()
                .and_then(|key| {
                    coworkers.iter().find(|coworker| {
                        coworker.e164.as_deref().and_then(last10).as_ref() == Some(key)
                    })
                })
                .cloned();
            LookupMatch {
                number,
                contact,
                coworker,
            }
        })
        .collect();
    Ok(Json(ContactLookup { matches }))
}

/// The contact card's customer: the index fields and more.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CardContact {
    #[serde(flatten)]
    pub contact: IndexContact,
    /// The primary email (else the oldest one); null when none.
    pub email: Option<String>,
    /// The sales rep's name; null when unassigned.
    pub rep_name: Option<String>,
    /// Street, city, state and ZIP on one line; null when none.
    pub address: Option<String>,
}

/// `GET /v1/phone/contacts/{id}`.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContactCard {
    /// The customer.
    pub contact: CardContact,
    /// Their live deals, open ones first, then newest (at most 20).
    pub deals: Vec<CardDeal>,
    /// The last 20 calls with them (anyone's), newest first; recordings and
    /// AI notes only of the calls the caller may hear.
    pub calls: Vec<CallRow>,
    /// The customer's page in the CRM.
    pub crm_url: String,
}

/// Get a contact card
///
/// A customer of the company with their email, rep and address, their live
/// deals (stage, outcome, amount, newest note) and the last 20 calls with
/// them, and the link to their CRM page. 404 for other companies' customers,
/// deleted ones and the onboarding sample. Device token only.
#[utoipa::path(
    get,
    path = "/v1/phone/contacts/{id}",
    operation_id = "get_phone_contact_card",
    tag = "phone",
    params(("id" = i32, Path, description = "Customer id")),
    responses(
        (status = 200, description = "The contact card", body = ContactCard),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 404, description = "No such customer in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn contact_card(
    State(state): State<AppState>,
    device: DeviceUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<ContactCard>> {
    let user = &device.user;
    let pool = &state.pool;
    let contact = contacts::contact(pool, user.company_id, user.id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let (email, rep_name, address) = contacts::card_details(pool, user.company_id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let deals = contacts::card_deals(pool, id).await?;
    let calls = calls::customer_calls(
        pool,
        user.company_id,
        calls::Hearer::of(user),
        id,
        CARD_CALLS,
    )
    .await?;
    Ok(Json(ContactCard {
        contact: CardContact {
            contact,
            email,
            rep_name,
            address,
        },
        deals,
        calls,
        crm_url: format!("{}/employee/customers/info/{id}/info", state.app_url),
    }))
}

/// A phone typed in the app as the CRM stores it (`317-555-0100`):
/// callable US and Canada numbers only.
fn stored_phone(typed: &str) -> Option<String> {
    let e164 = to_e164(typed).filter(|e164| is_nanp(e164))?;
    storage_format(e164.trim_start_matches('+'))
}

/// Body of `POST /v1/phone/contacts`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct NewContact {
    /// The customer's name (1 to 255 characters).
    pub name: String,
    /// Their phone: E.164 or 10 digits (US or Canada).
    pub phone: String,
    /// Their company name; null or empty for none.
    #[serde(default)]
    pub company: Option<String>,
}

/// Create a contact
///
/// Creates a customer with this name, phone and company, with the caller as
/// the sales rep (source `call-in`) and an open deal, and returns them as an
/// index contact; calls from the number match them at once. 400
/// `invalid_phone` unless a US or Canada number; 400 for an empty or too
/// long name. Device token only.
#[utoipa::path(
    post,
    path = "/v1/phone/contacts",
    operation_id = "create_phone_contact",
    tag = "phone",
    request_body = NewContact,
    responses(
        (status = 201, description = "Created", body = IndexContact),
        (status = 400, description = "`invalid_phone`, or the name is empty or too long", body = ErrorBody),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn create_contact(
    State(state): State<AppState>,
    device: DeviceUser,
    JsonBody(input): JsonBody<NewContact>,
) -> ApiResult<(StatusCode, Json<IndexContact>)> {
    let user = &device.user;
    let pool = &state.pool;
    let name = input.name.trim();
    if name.is_empty() || name.chars().count() > 255 {
        return Err(ApiError::BadRequest(
            "Enter a name of 1 to 255 characters.".into(),
        ));
    }
    let phone = stored_phone(&input.phone).ok_or_else(|| bad_request("invalid_phone"))?;
    let company = input
        .company
        .as_deref()
        .map(str::trim)
        .filter(|company| !company.is_empty())
        .map(|company| company.chars().take(255).collect::<String>());
    let created_by = audit_name(user.name.as_deref());
    // The customer, its audit line and its open deal together, or nothing.
    let mut tx = pool.begin().await?;
    let id = sqlx::query(
        "INSERT INTO customers (name, phone, company_id, company_name, source, sales_rep,
                                assigned_date, created_by)
         VALUES (?, ?, ?, ?, 'call-in', ?, NOW(), ?)",
    )
    .bind(name)
    .bind(&phone)
    .bind(user.company_id)
    .bind(&company)
    .bind(user.id)
    .bind(&created_by)
    .execute(&mut *tx)
    .await?
    .last_insert_id();
    let id =
        i32::try_from(id).map_err(|_| ApiError::Internal("customer id out of range".into()))?;
    if id == SAMPLE_CUSTOMER_ID {
        tx.rollback().await?;
        return Err(ApiError::Internal(
            "customer id clashes with the sample".into(),
        ));
    }
    record_reassignment(
        &mut *tx,
        id,
        created_by.as_deref(),
        created_by.as_deref(),
        None,
    )
    .await?;
    let deal = open_deal_in(&mut tx, id, user.company_id, user.id).await?;
    // Calls from the number find the new customer at once.
    customer_index::index_new_customer(&mut tx, user.company_id, id, &phone).await?;
    tx.commit().await?;
    if let Some(list_id) = deal.new_in_list {
        start_drips(pool, deal.id, list_id).await;
    }
    state.webhooks.sync_contacts(user.company_id, id).await;
    let contact = contacts::contact(pool, user.company_id, user.id, id)
        .await?
        .ok_or_else(|| ApiError::Internal("the new customer is missing".into()))?;
    Ok((StatusCode::CREATED, Json(contact)))
}

/// Body of `POST /v1/phone/contacts/{id}/phones`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AddPhone {
    /// The number to add: E.164 or 10 digits (US or Canada).
    pub phone: String,
    /// When phone and phone 2 are both used: replace phone 2 (else 409
    /// `phone_2_taken`).
    #[serde(default)]
    pub replace_phone2: bool,
}

/// `409` when phone 2 would be replaced.
#[derive(Debug, Serialize, ToSchema)]
pub struct Phone2Taken {
    /// `phone_2_taken`.
    pub error: String,
    /// The phone 2 that would be replaced (its last 10 digits).
    pub phone2: String,
}

/// Add a number to a contact
///
/// Saves the number as the customer's phone when it is empty, else as phone
/// 2; when both are used, 409 `phone_2_taken` (with the current phone 2)
/// unless `replacePhone2`. A number the customer has already changes
/// nothing. Calls from the customer's numbers match them at once (a
/// replaced phone 2 no longer does). Returns the contact. 400
/// `invalid_phone`; 404 for other companies' customers. Device token only.
#[utoipa::path(
    post,
    path = "/v1/phone/contacts/{id}/phones",
    operation_id = "add_phone_contact_number",
    tag = "phone",
    params(("id" = i32, Path, description = "Customer id")),
    request_body = AddPhone,
    responses(
        (status = 200, description = "The contact now", body = IndexContact),
        (status = 400, description = "`invalid_phone`", body = ErrorBody),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 404, description = "No such customer in the company", body = ErrorBody),
        (status = 409, description = "`phone_2_taken`: ask before replacing phone 2", body = Phone2Taken),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn add_contact_phone(
    State(state): State<AppState>,
    device: DeviceUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<AddPhone>,
) -> ApiResult<Response> {
    let user = &device.user;
    let pool = &state.pool;
    let phone = stored_phone(&input.phone).ok_or_else(|| bad_request("invalid_phone"))?;
    let digits = last10(&phone);
    let mut tx = pool.begin().await?;
    let (current, current_2): (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT phone, phone_2 FROM customers
          WHERE id = ? AND company_id = ? AND deleted_at IS NULL AND id <> 990000021 FOR UPDATE",
    )
    .bind(id)
    .bind(user.company_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(ApiError::NotFound)?;
    let blank = |value: &Option<String>| value.as_deref().is_none_or(|v| v.trim().is_empty());
    let has = |value: &Option<String>| value.as_deref().and_then(last10) == digits;
    let column = if has(&current) || has(&current_2) {
        None
    } else if blank(&current) {
        Some("phone")
    } else if blank(&current_2) || input.replace_phone2 {
        Some("phone_2")
    } else {
        tx.rollback().await?;
        let taken = current_2.unwrap_or_default();
        let phone2 = last10(&taken).unwrap_or_else(|| {
            taken
                .chars()
                .filter(char::is_ascii_digit)
                .collect::<String>()
        });
        let body = Phone2Taken {
            error: "phone_2_taken".into(),
            phone2,
        };
        return Ok((StatusCode::CONFLICT, Json(body)).into_response());
    };
    if let Some(column) = column {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE customers SET {column} = ? WHERE id = ? AND company_id = ?"
        )))
        .bind(&phone)
        .bind(id)
        .bind(user.company_id)
        .execute(&mut *tx)
        .await?;
    }
    // Calls find the customer by their numbers now at once (the added one,
    // and not a replaced phone 2).
    let (saved, saved_2) = match column {
        Some("phone") => (Some(phone.as_str()), current_2.as_deref()),
        Some(_) => (current.as_deref(), Some(phone.as_str())),
        None => (current.as_deref(), current_2.as_deref()),
    };
    customer_index::reindex_customer(&mut tx, user.company_id, id, saved, saved_2).await?;
    tx.commit().await?;
    if column.is_some() {
        state.webhooks.sync_contacts(user.company_id, id).await;
    }
    let contact = contacts::contact(pool, user.company_id, user.id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(contact).into_response())
}
