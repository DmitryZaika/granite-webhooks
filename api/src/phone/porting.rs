//! Porting a company's existing numbers into Telnyx (port-in orders).
//!
//! A company admin checks the numbers, creates a draft (Telnyx may split it
//! into several orders: by carrier, number type or FastPort), fills in the
//! business details from the current carrier's bill, uploads the bill and
//! the signed LOA, picks a port date and submits. Telnyx then works with the
//! losing carrier: `in-process` / `submitted`, maybe `exception` (fix and
//! resubmit), `foc-date-confirmed`, and `ported` on the date; or
//! `cancel-pending` / `cancelled`.
//!
//! `phone_port_orders` keeps our copy of each order (the admin page lists
//! from it). It is brought up to date from Telnyx ([`sync`]) when an admin
//! opens or changes the order, when Telnyx notifies us
//! (`POST /v1/webhooks/telnyx/{token}/porting`) and by the phone job
//! ([`run_job`]). Each status change is applied once (compare and set), so
//! its timeline entry and the admins' notification (bell and email) happen
//! once whichever path saw it first.
//!
//! Once ported, [`complete`] adds the numbers to the company
//! (`telnyx_numbers`: direct lines, unassigned, the order's label) and makes
//! sure Telnyx has them on the company's TeXML app (and its messaging
//! profile, when texting is set up), retried by the job until done.
//!
//! Business details and documents live in Telnyx only. Nothing here logs
//! phone numbers, names or comment text.

use super::alerts::company_admins;
use super::company::{self, CompanyConfig};
use super::numbers::{last10, to_e164};
use super::wire_enum;
use crate::routes::auth::{app_origin, company_subdomain, sha256_hex};
use crate::state::AppState;
use crate::telnyx::porting::{PortingError, PortingOrder};
use crate::webhooks_client::OutgoingEmail;
use chrono::{DateTime, Utc};
use lambda_http::tracing;
use serde_json::{Value, json};
use sqlx::{FromRow, MySqlExecutor, MySqlPool};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Most numbers one port request may hold (Telnyx lists 50 per order).
pub const MAX_NUMBERS: usize = 50;
/// Telnyx refuses a cancel this close to the port date (support must do it).
pub const CANCEL_CUTOFF_HOURS: i64 = 48;
/// How long a request may spend setting up ported numbers before the job
/// takes over.
const INLINE_BUDGET: Duration = Duration::from_secs(8);
/// Orders read again from Telnyx, and completions tried, per job run.
const JOB_BATCH: i64 = 5;
/// Completions tried in all (each failure waits longer, up to 12 hours).
const MAX_COMPLETION_ATTEMPTS: i64 = 30;
/// Who the porting notifications are from.
const ACTOR: &str = "Telnyx porting";
const NO_REPLY: &str = "noreply@granite-manager.com";

wire_enum! {
    /// Where a port-in order is: Telnyx's status, or `deleted` for a draft
    /// that is gone (deleted by an admin, or by Telnyx after 30 days).
    pub enum PortStatus {
        /// Being filled in; nothing sent to the losing carrier yet.
        Draft = "draft",
        /// Submitted; Telnyx is checking it.
        InProcess = "in-process",
        /// Sent to the losing carrier.
        Submitted = "submitted",
        /// Refused or stuck: fix what `statusDetails` and the comments say,
        /// then submit again.
        Exception = "exception",
        /// The losing carrier confirmed the port date (`focAt`).
        FocDateConfirmed = "foc-date-confirmed",
        /// The numbers are on Telnyx.
        Ported = "ported",
        /// Cancellation asked; Telnyx's porting team is on it.
        CancelPending = "cancel-pending",
        /// Cancelled: the numbers stay with the current carrier.
        Cancelled = "cancelled",
        /// The draft was deleted.
        Deleted = "deleted",
    }
}

/// SQL list of the statuses an order can still move from.
pub const OPEN_STATUSES: &str =
    "'draft', 'in-process', 'submitted', 'exception', 'foc-date-confirmed', 'cancel-pending'";

impl PortStatus {
    /// It can still change (not ported, cancelled or deleted).
    pub const fn is_open(self) -> bool {
        !matches!(self, Self::Ported | Self::Cancelled | Self::Deleted)
    }

    /// Telnyx takes changes to the order (business details, documents,
    /// date) in this status.
    pub const fn editable(self) -> bool {
        matches!(self, Self::Draft | Self::InProcess | Self::Exception)
    }

    /// It can be submitted: a draft, or again after an exception.
    pub const fn submittable(self) -> bool {
        matches!(self, Self::Draft | Self::Exception)
    }

    /// Telnyx may be asked to cancel it (a draft is deleted instead).
    pub const fn cancellable(self) -> bool {
        matches!(
            self,
            Self::InProcess | Self::Submitted | Self::Exception | Self::FocDateConfirmed
        )
    }
}

/// One of our port orders.
#[derive(Debug, Clone, FromRow)]
pub struct PortRow {
    pub id: i64,
    pub company_id: i32,
    pub request_id: String,
    pub telnyx_order_id: String,
    #[sqlx(try_from = "String")]
    pub status: PortStatus,
    /// JSON `[{"code", "description"}]`.
    pub status_details: Option<String>,
    pub label: String,
    /// JSON `["+1...", ...]`.
    pub phone_numbers: String,
    pub support_key: Option<String>,
    pub old_carrier: Option<String>,
    pub number_type: Option<String>,
    pub fast_port: bool,
    pub foc_requested_at: Option<DateTime<Utc>>,
    pub foc_at: Option<DateTime<Utc>>,
    pub requirements_met: bool,
    pub messaging_status: Option<String>,
    pub created_by_user_id: Option<i32>,
    pub created_by_name: Option<String>,
    pub submitted_at: Option<DateTime<Utc>>,
    pub cancel_requested_at: Option<DateTime<Utc>>,
    pub ported_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub refreshed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

const SELECT: &str = "SELECT o.id, o.company_id, o.request_id, o.telnyx_order_id, o.status,
        CAST(o.status_details AS CHAR) AS status_details, o.label,
        CAST(o.phone_numbers AS CHAR) AS phone_numbers, o.support_key, o.old_carrier,
        o.number_type, o.fast_port <> 0 AS fast_port, o.foc_requested_at, o.foc_at,
        o.requirements_met <> 0 AS requirements_met, o.messaging_status, o.created_by_user_id,
        COALESCE(NULLIF(TRIM(u.name), ''), u.email) AS created_by_name,
        o.submitted_at, o.cancel_requested_at, o.ported_at, o.completed_at, o.last_error,
        o.refreshed_at, o.created_at, o.updated_at
   FROM phone_port_orders o
   LEFT JOIN users u ON u.id = o.created_by_user_id";

impl PortRow {
    /// The order's numbers, E.164.
    pub fn numbers(&self) -> Vec<String> {
        serde_json::from_str(&self.phone_numbers).unwrap_or_default()
    }

    /// Telnyx's reasons for the status: (code, description).
    pub fn details(&self) -> Vec<(String, Option<String>)> {
        let parsed: Vec<Value> = self
            .status_details
            .as_deref()
            .and_then(|json| serde_json::from_str(json).ok())
            .unwrap_or_default();
        parsed
            .iter()
            .filter_map(|detail| {
                let code = detail["code"].as_str()?.to_string();
                Some((code, detail["description"].as_str().map(str::to_string)))
            })
            .collect()
    }

    /// Telnyx would refuse a cancel now: the confirmed port date is less
    /// than 48 hours away (or past).
    pub fn too_close_to_cancel(&self, now: DateTime<Utc>) -> bool {
        self.foc_at
            .is_some_and(|at| at - now < chrono::Duration::hours(CANCEL_CUTOFF_HOURS))
    }
}

/// The company's port orders, newest first.
pub async fn list(pool: &MySqlPool, company_id: i32) -> sqlx::Result<Vec<PortRow>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{SELECT} WHERE o.company_id = ? ORDER BY o.id DESC"
    )))
    .bind(company_id)
    .fetch_all(pool)
    .await
}

/// One port order of the company.
pub async fn load(pool: &MySqlPool, company_id: i32, id: i64) -> sqlx::Result<Option<PortRow>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{SELECT} WHERE o.company_id = ? AND o.id = ?"
    )))
    .bind(company_id)
    .bind(id)
    .fetch_optional(pool)
    .await
}

/// The company's order Telnyx knows as `telnyx_order_id`.
pub async fn load_by_telnyx(
    pool: &MySqlPool,
    company_id: i32,
    telnyx_order_id: &str,
) -> sqlx::Result<Option<PortRow>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{SELECT} WHERE o.company_id = ? AND o.telnyx_order_id = ?"
    )))
    .bind(company_id)
    .bind(telnyx_order_id)
    .fetch_optional(pool)
    .await
}

/// What we keep of a Telnyx order.
struct Snapshot {
    status: Option<PortStatus>,
    details: String,
    numbers: Option<String>,
    support_key: Option<String>,
    old_carrier: Option<String>,
    number_type: Option<String>,
    fast_port: bool,
    foc_requested: Option<DateTime<Utc>>,
    foc_actual: Option<DateTime<Utc>>,
    requirements_met: bool,
    messaging_status: Option<String>,
    updated_at: Option<DateTime<Utc>>,
}

/// The order's numbers in E.164, as Telnyx lists them.
pub fn order_numbers(order: &PortingOrder) -> Vec<String> {
    let mut numbers: Vec<String> = Vec::new();
    for number in &order.phone_numbers {
        if let Some(e164) = to_e164(&number.phone_number)
            && !numbers.contains(&e164)
        {
            numbers.push(e164);
        }
    }
    numbers
}

fn clip(text: Option<&String>, chars: usize) -> Option<String> {
    text.map(|text| text.chars().take(chars).collect())
}

fn snapshot(order: &PortingOrder) -> Snapshot {
    let status = PortStatus::parse(&order.status);
    if status.is_none() {
        tracing::warn!(status = %order.status, "unknown Telnyx porting status; kept ours");
    }
    let details: Vec<Value> = order
        .status_details
        .iter()
        .map(|detail| json!({ "code": detail.code, "description": detail.description }))
        .collect();
    let numbers = order_numbers(order);
    Snapshot {
        status,
        details: Value::Array(details).to_string(),
        // An answer without numbers (a PATCH's) keeps the ones we have.
        numbers: (!numbers.is_empty()).then(|| json!(numbers).to_string()),
        support_key: clip(order.support_key.as_ref(), 64),
        old_carrier: clip(order.old_service_provider.as_ref(), 128),
        number_type: clip(order.phone_number_type.as_ref(), 32),
        fast_port: order.fast_port_eligible,
        foc_requested: order.foc_requested,
        foc_actual: order.foc_actual,
        requirements_met: order.requirements_met,
        messaging_status: clip(order.messaging_status.as_ref(), 32),
        updated_at: order.updated_at,
    }
}

/// Saves a Telnyx order we just created (or one split from ours) as the
/// company's; its id. A second save of the same order keeps the first.
pub async fn insert(
    pool: &MySqlPool,
    company_id: i32,
    request_id: &str,
    label: &str,
    created_by: Option<i32>,
    order: &PortingOrder,
) -> sqlx::Result<i64> {
    let snap = snapshot(order);
    sqlx::query(
        "INSERT IGNORE INTO phone_port_orders
           (company_id, request_id, telnyx_order_id, status, status_details, label,
            phone_numbers, support_key, old_carrier, number_type, fast_port, foc_requested_at,
            foc_at, requirements_met, messaging_status, created_by_user_id, refreshed_at,
            telnyx_updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, UTC_TIMESTAMP(), ?)",
    )
    .bind(company_id)
    .bind(request_id)
    .bind(&order.id)
    .bind(snap.status.unwrap_or(PortStatus::Draft).as_str())
    .bind(&snap.details)
    .bind(label.chars().take(100).collect::<String>())
    .bind(snap.numbers.unwrap_or_else(|| "[]".into()))
    .bind(&snap.support_key)
    .bind(&snap.old_carrier)
    .bind(&snap.number_type)
    .bind(snap.fast_port)
    .bind(snap.foc_requested)
    .bind(snap.foc_actual)
    .bind(snap.requirements_met)
    .bind(&snap.messaging_status)
    .bind(created_by)
    .bind(snap.updated_at)
    .execute(pool)
    .await?;
    sqlx::query_scalar("SELECT id FROM phone_port_orders WHERE telnyx_order_id = ?")
        .bind(&order.id)
        .fetch_one(pool)
        .await
}

/// Adds an entry to the order's timeline.
pub async fn record<'e>(
    executor: impl MySqlExecutor<'e>,
    company_id: i32,
    order_id: Option<i64>,
    kind: &str,
    status: Option<PortStatus>,
    detail: Option<&str>,
    actor: Option<i32>,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO phone_port_events
           (company_id, port_order_id, kind, status, detail, actor_user_id, occurred_at)
         VALUES (?, ?, ?, ?, ?, ?, UTC_TIMESTAMP(3))",
    )
    .bind(company_id)
    .bind(order_id)
    .bind(kind)
    .bind(status.map(PortStatus::as_str))
    .bind(detail.map(|detail| detail.chars().take(255).collect::<String>()))
    .bind(actor)
    .execute(executor)
    .await?;
    Ok(())
}

/// One timeline entry.
#[derive(Debug, Clone, FromRow)]
pub struct TimelineRow {
    pub kind: String,
    pub status: Option<String>,
    pub detail: Option<String>,
    pub actor_name: Option<String>,
    pub occurred_at: DateTime<Utc>,
}

/// The order's timeline, oldest first (notifications taken left out).
pub async fn timeline(
    pool: &MySqlPool,
    company_id: i32,
    order_id: i64,
) -> sqlx::Result<Vec<TimelineRow>> {
    sqlx::query_as(
        "SELECT e.kind, e.status, e.detail,
                COALESCE(NULLIF(TRIM(u.name), ''), u.email) AS actor_name, e.occurred_at
           FROM phone_port_events e
           LEFT JOIN users u ON u.id = e.actor_user_id
          WHERE e.company_id = ? AND e.port_order_id = ? AND e.kind <> 'webhook'
          ORDER BY e.occurred_at, e.id",
    )
    .bind(company_id)
    .bind(order_id)
    .fetch_all(pool)
    .await
}

/// Writes one entry of the phone audit log (a write without an admin, such
/// as adding ported numbers, has no actor).
pub async fn audit<'e>(
    executor: impl MySqlExecutor<'e>,
    company_id: i32,
    actor: Option<i32>,
    action: &str,
    order_id: i64,
    details: &Value,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO phone_audit_log (company_id, actor_user_id, action, target, details)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(company_id)
    .bind(actor)
    .bind(action)
    .bind(format!("port_order:{order_id}"))
    .bind(details.to_string())
    .execute(executor)
    .await?;
    Ok(())
}

/// Takes a Telnyx notification once: `Some(key)` when it is new (ours to
/// handle; give it back with [`release_notification`] when that fails),
/// `None` when it was taken before. One without an event id is always new.
pub async fn claim_notification(
    pool: &MySqlPool,
    company_id: i32,
    order_id: Option<i64>,
    event_id: Option<&str>,
    event_type: &str,
) -> sqlx::Result<Option<String>> {
    let key = match event_id.map(str::trim).filter(|id| !id.is_empty()) {
        Some(id) => sha256_hex(&format!("porting|{id}")),
        None => sha256_hex(&uuid::Uuid::new_v4().to_string()),
    };
    let inserted = sqlx::query(
        "INSERT INTO phone_port_events
           (company_id, port_order_id, kind, detail, dedupe_key, occurred_at)
         VALUES (?, ?, 'webhook', ?, ?, UTC_TIMESTAMP(3))",
    )
    .bind(company_id)
    .bind(order_id)
    .bind(event_type.chars().take(64).collect::<String>())
    .bind(&key)
    .execute(pool)
    .await;
    match inserted {
        Ok(_) => Ok(Some(key)),
        Err(err)
            if err
                .as_database_error()
                .is_some_and(sqlx::error::DatabaseError::is_unique_violation) =>
        {
            Ok(None)
        }
        Err(err) => Err(err),
    }
}

/// Gives a notification back, so Telnyx's retry is handled.
pub async fn release_notification(pool: &MySqlPool, key: &str) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM phone_port_events WHERE dedupe_key = ?")
        .bind(key)
        .execute(pool)
        .await?;
    Ok(())
}

/// Which of `numbers` are in an open port order (of any company).
pub async fn in_open_ports(pool: &MySqlPool, numbers: &[String]) -> sqlx::Result<Vec<String>> {
    let mut found = Vec::new();
    for number in numbers {
        let open: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT CAST(COUNT(*) AS SIGNED) FROM phone_port_orders
              WHERE status IN ({OPEN_STATUSES}) AND JSON_CONTAINS(phone_numbers, JSON_QUOTE(?))"
        )))
        .bind(number)
        .fetch_one(pool)
        .await?;
        if open > 0 {
            found.push(number.clone());
        }
    }
    Ok(found)
}

/// Which of `numbers` are company numbers already (of any company).
pub async fn on_telnyx(pool: &MySqlPool, numbers: &[String]) -> sqlx::Result<Vec<String>> {
    if numbers.is_empty() {
        return Ok(Vec::new());
    }
    let mut query = sqlx::QueryBuilder::new("SELECT e164 FROM telnyx_numbers WHERE 1 = 1");
    crate::sql::push_in(&mut query, "e164", numbers);
    query.build_query_scalar().fetch_all(pool).await
}

/// The company's messaging profile, when Telnyx texting is set up.
///
/// TODO(stream T, Telnyx texting): `telnyx_company_config.messaging_profile_id`
/// comes with its migration. Until that runs the column is missing (MySQL
/// "Unknown column", SQLSTATE 42S22) and ports carry no messaging profile.
pub async fn messaging_profile_id(
    pool: &MySqlPool,
    company_id: i32,
) -> sqlx::Result<Option<String>> {
    let found: sqlx::Result<Option<Option<String>>> = sqlx::query_scalar(
        "SELECT messaging_profile_id FROM telnyx_company_config WHERE company_id = ?",
    )
    .bind(company_id)
    .fetch_optional(pool)
    .await;
    match found {
        Ok(profile) => Ok(profile
            .flatten()
            .map(|profile| profile.trim().to_string())
            .filter(|profile| !profile.is_empty())),
        Err(err) if is_unknown_column(&err) => Ok(None),
        Err(err) => Err(err),
    }
}

fn is_unknown_column(err: &sqlx::Error) -> bool {
    err.as_database_error().is_some_and(|db| {
        db.code().as_deref() == Some("42S22") || db.message().contains("Unknown column")
    })
}

/// What every change we send an order carries, so it stays set up: our
/// webhook, and what the numbers get once ported (the company's TeXML app,
/// its billing group and messaging profile; messaging ported too when the
/// company texts through Telnyx and the order can).
pub fn setup_changes(
    config: &CompanyConfig,
    webhook_url: &str,
    messaging_profile: Option<&str>,
    messaging_capable: Option<bool>,
) -> Value {
    let mut configuration = json!({ "connection_id": config.texml_app_id });
    if let Some(billing_group) = &config.billing_group_id {
        configuration["billing_group_id"] = json!(billing_group);
    }
    if let Some(profile) = messaging_profile {
        configuration["messaging_profile_id"] = json!(profile);
    }
    let mut changes = json!({
        "webhook_url": webhook_url,
        "phone_number_configuration": configuration,
    });
    if messaging_profile.is_some() && messaging_capable == Some(true) {
        changes["messaging"] = json!({ "enable_messaging": true });
    }
    changes
}

/// Where Telnyx sends the company's porting notifications.
pub fn webhook_url(state: &AppState, config: &CompanyConfig) -> String {
    format!("{}/porting", config.webhook_base(&state.phone))
}

/// Brings our row up to date with Telnyx's `order` (a status change once:
/// its timeline entry and notification), then sets up the numbers once it
/// is ported (for a few seconds; the job finishes). The row as it is now.
pub async fn sync(
    state: &AppState,
    row: &PortRow,
    order: &PortingOrder,
    actor: Option<i32>,
) -> sqlx::Result<PortRow> {
    let pool = &state.pool;
    let snap = snapshot(order);
    sqlx::query(
        "UPDATE phone_port_orders
            SET status_details = ?, phone_numbers = COALESCE(?, phone_numbers),
                support_key = ?, old_carrier = ?, number_type = ?, fast_port = ?,
                foc_requested_at = ?, foc_at = ?, requirements_met = ?, messaging_status = ?,
                telnyx_updated_at = ?, refreshed_at = UTC_TIMESTAMP(), last_error = NULL,
                last_error_at = NULL
          WHERE id = ?",
    )
    .bind(&snap.details)
    .bind(&snap.numbers)
    .bind(&snap.support_key)
    .bind(&snap.old_carrier)
    .bind(&snap.number_type)
    .bind(snap.fast_port)
    .bind(snap.foc_requested)
    .bind(snap.foc_actual)
    .bind(snap.requirements_met)
    .bind(&snap.messaging_status)
    .bind(snap.updated_at)
    .bind(row.id)
    .execute(pool)
    .await?;
    if snap.foc_actual.is_some() && snap.foc_actual != row.foc_at && row.foc_at.is_some() {
        record(pool, row.company_id, Some(row.id), "foc", None, None, None).await?;
    }
    if snap.messaging_status != row.messaging_status
        && matches!(
            snap.messaging_status.as_deref(),
            Some("ported" | "exception")
        )
    {
        record(
            pool,
            row.company_id,
            Some(row.id),
            "messaging",
            None,
            snap.messaging_status.as_deref(),
            None,
        )
        .await?;
    }
    if let Some(status) = snap.status.filter(|status| *status != row.status) {
        let codes: Vec<&str> = order
            .status_details
            .iter()
            .map(|detail| detail.code.as_str())
            .collect();
        change_status(state, row, status, &codes.join(", "), actor).await?;
    }
    let now = load(pool, row.company_id, row.id)
        .await?
        .unwrap_or_else(|| row.clone());
    if now.status == PortStatus::Ported && now.completed_at.is_none() {
        complete(state, &now, Instant::now() + INLINE_BUDGET).await?;
        return Ok(load(pool, row.company_id, row.id).await?.unwrap_or(now));
    }
    Ok(now)
}

/// Moves the order from the status it had to `status`, once: whoever moves
/// it writes the timeline entry and tells the admins.
async fn change_status(
    state: &AppState,
    row: &PortRow,
    status: PortStatus,
    detail: &str,
    actor: Option<i32>,
) -> sqlx::Result<bool> {
    let moved = sqlx::query(
        "UPDATE phone_port_orders
            SET status = ?,
                ported_at = IF(? = 'ported', COALESCE(ported_at, UTC_TIMESTAMP()), ported_at)
          WHERE id = ? AND status = ?",
    )
    .bind(status.as_str())
    .bind(status.as_str())
    .bind(row.id)
    .bind(row.status.as_str())
    .execute(&state.pool)
    .await?
    .rows_affected()
        == 1;
    if !moved {
        return Ok(false);
    }
    let detail = Some(detail).filter(|detail| !detail.is_empty());
    record(
        &state.pool,
        row.company_id,
        Some(row.id),
        "status",
        Some(status),
        detail,
        actor,
    )
    .await?;
    let notice = match status {
        PortStatus::Exception => Some(Notice::Exception),
        PortStatus::FocDateConfirmed => Some(Notice::FocConfirmed),
        PortStatus::Cancelled => Some(Notice::Cancelled),
        _ => None,
    };
    if let Some(notice) = notice {
        let now = load(&state.pool, row.company_id, row.id)
            .await?
            .unwrap_or_else(|| row.clone());
        notify(state, &now, notice).await?;
    }
    Ok(true)
}

/// A draft Telnyx no longer has (deleted by us, or after 30 days).
pub async fn mark_deleted(state: &AppState, row: &PortRow, actor: Option<i32>) -> sqlx::Result<()> {
    if row.status.is_open() {
        change_status(state, row, PortStatus::Deleted, "", actor).await?;
    }
    Ok(())
}

/// Why an order could not be read from Telnyx.
#[derive(Debug, thiserror::Error)]
pub enum RefreshError {
    #[error(transparent)]
    Telnyx(#[from] PortingError),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

/// Reads the order from Telnyx and brings our row up to date ([`sync`]). A
/// draft Telnyx no longer has is marked deleted.
pub async fn refresh(state: &AppState, row: &PortRow) -> Result<PortRow, RefreshError> {
    match state.telnyx.get_porting_order(&row.telnyx_order_id).await {
        Ok(order) => Ok(sync(state, row, &order, None).await?),
        Err(err) if err.status() == Some(404) && row.status == PortStatus::Draft => {
            mark_deleted(state, row, None).await?;
            Ok(load(&state.pool, row.company_id, row.id)
                .await?
                .unwrap_or_else(|| row.clone()))
        }
        Err(err) => Err(err.into()),
    }
}

/// Saves what went wrong last (no personal data).
pub async fn set_error(pool: &MySqlPool, order_id: i64, error: &str) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE phone_port_orders SET last_error = ?, last_error_at = UTC_TIMESTAMP() WHERE id = ?",
    )
    .bind(error.chars().take(500).collect::<String>())
    .bind(order_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Adds the ported numbers to the company: new ones as unassigned direct
/// lines with the order's label, ones it has switched back on. Returns how
/// many are new; numbers of another company are left alone (logged).
async fn add_numbers(pool: &MySqlPool, row: &PortRow) -> sqlx::Result<usize> {
    let label: String = row.label.trim().chars().take(100).collect();
    let mut added = 0;
    let mut elsewhere = 0;
    let mut tx = pool.begin().await?;
    for e164 in row.numbers() {
        let Some(digits) = last10(&e164) else {
            continue;
        };
        let owner: Option<i32> =
            sqlx::query_scalar("SELECT company_id FROM telnyx_numbers WHERE e164 = ?")
                .bind(&e164)
                .fetch_optional(&mut *tx)
                .await?;
        match owner {
            Some(owner) if owner == row.company_id => {
                sqlx::query(
                    "UPDATE telnyx_numbers SET active = 1 WHERE e164 = ? AND company_id = ?",
                )
                .bind(&e164)
                .bind(row.company_id)
                .execute(&mut *tx)
                .await?;
            }
            Some(_) => elsewhere += 1,
            None => {
                let inserted = sqlx::query(
                    "INSERT IGNORE INTO telnyx_numbers (company_id, e164, last10, label, kind, active)
                     VALUES (?, ?, ?, ?, 'direct', 1)",
                )
                .bind(row.company_id)
                .bind(&e164)
                .bind(&digits)
                .bind(&label)
                .execute(&mut *tx)
                .await?
                .rows_affected();
                added += usize::try_from(inserted).unwrap_or_default();
            }
        }
    }
    tx.commit().await?;
    if elsewhere > 0 {
        tracing::warn!(
            company_id = row.company_id,
            order_id = row.id,
            numbers = elsewhere,
            "ported numbers belong to another company; not added"
        );
    }
    Ok(added)
}

/// Sets up the numbers of a ported order: adds them to the company, puts
/// them on its TeXML app on Telnyx (and its messaging profile), and once
/// every number is done marks the order complete and tells the admins.
/// Stops at `deadline` (the job goes on). Whether it completed now.
pub async fn complete(state: &AppState, row: &PortRow, deadline: Instant) -> sqlx::Result<bool> {
    let pool = &state.pool;
    let started = sqlx::query(
        "UPDATE phone_port_orders
            SET completion_attempts = completion_attempts + 1, completion_tried_at = UTC_TIMESTAMP()
          WHERE id = ? AND status = 'ported' AND completed_at IS NULL",
    )
    .bind(row.id)
    .execute(pool)
    .await?
    .rows_affected();
    if started == 0 {
        return Ok(false);
    }
    let added = add_numbers(pool, row).await?;
    let config = company::load(pool, row.company_id).await?;
    let Some(app_id) = config
        .as_ref()
        .and_then(|config| config.texml_app_id.clone())
    else {
        set_error(
            pool,
            row.id,
            "The numbers were added, but Telnyx is not set up for this company, so they \
             are not connected to calling yet. Click Connect Telnyx on the Phone page.",
        )
        .await?;
        return Ok(false);
    };
    if !state.telnyx.is_enabled() {
        set_error(pool, row.id, "Telnyx is not set up on the server.").await?;
        return Ok(false);
    }
    let billing_group = config
        .as_ref()
        .and_then(|config| config.billing_group_id.clone());
    let profile = messaging_profile_id(pool, row.company_id).await?;
    match connect_numbers(
        state,
        row,
        &app_id,
        billing_group.as_deref(),
        profile.as_deref(),
        deadline,
    )
    .await
    {
        Ok(true) => {}
        Ok(false) => {
            set_error(
                pool,
                row.id,
                "The numbers were added. Telnyx is still finishing the port; they are \
                 connected to calling as soon as Telnyx lists them.",
            )
            .await?;
            return Ok(false);
        }
        Err(err) => {
            tracing::warn!(error = %err, order_id = row.id, "ported numbers not set up on Telnyx");
            set_error(
                pool,
                row.id,
                "The numbers were added, but setting them up on Telnyx failed. It is retried \
                 automatically.",
            )
            .await?;
            return Ok(false);
        }
    }
    let done = sqlx::query(
        "UPDATE phone_port_orders
            SET completed_at = UTC_TIMESTAMP(), last_error = NULL, last_error_at = NULL
          WHERE id = ? AND completed_at IS NULL",
    )
    .bind(row.id)
    .execute(pool)
    .await?
    .rows_affected()
        == 1;
    if !done {
        return Ok(false);
    }
    let count = row.numbers().len();
    record(
        pool,
        row.company_id,
        Some(row.id),
        "completed",
        None,
        Some(&count.to_string()),
        None,
    )
    .await?;
    audit(
        pool,
        row.company_id,
        None,
        "port_completed",
        row.id,
        &json!({ "numbers": count, "added": added }),
    )
    .await?;
    let now = load(pool, row.company_id, row.id)
        .await?
        .unwrap_or_else(|| row.clone());
    notify(state, &now, Notice::Ported).await?;
    Ok(true)
}

/// Puts each number of the order on the TeXML app (and the messaging
/// profile) on Telnyx and saves its Telnyx id. `Ok(false)` when time ran
/// out or Telnyx does not list a number yet.
async fn connect_numbers(
    state: &AppState,
    row: &PortRow,
    app_id: &str,
    billing_group: Option<&str>,
    profile: Option<&str>,
    deadline: Instant,
) -> Result<bool, PortingError> {
    let telnyx = &state.telnyx;
    let on_app: HashMap<String, crate::telnyx::porting::AccountNumber> = telnyx
        .account_numbers_on(app_id)
        .await?
        .into_iter()
        .filter_map(|number| Some((to_e164(&number.phone_number)?, number)))
        .collect();
    let mut all = true;
    for e164 in row.numbers() {
        if Instant::now() >= deadline {
            return Ok(false);
        }
        let number = match on_app.get(&e164) {
            Some(number) => number.clone(),
            None => {
                let Some(number) = telnyx.find_account_number(&e164).await? else {
                    all = false;
                    continue;
                };
                telnyx
                    .set_number_connection(&number.id, app_id, billing_group)
                    .await?;
                number
            }
        };
        if let Some(profile) = profile
            && number.messaging_profile_id.as_deref() != Some(profile)
        {
            telnyx
                .set_number_messaging_profile(&number.id, profile)
                .await?;
        }
        let telnyx_id: String = number.id.chars().take(64).collect();
        if let Err(err) = sqlx::query(
            "UPDATE telnyx_numbers SET telnyx_number_id = ?
              WHERE e164 = ? AND company_id = ? AND telnyx_number_id IS NULL",
        )
        .bind(&telnyx_id)
        .bind(&e164)
        .bind(row.company_id)
        .execute(&state.pool)
        .await
        {
            tracing::error!(error = %err, order_id = row.id, "Telnyx number id not saved");
        }
    }
    Ok(all)
}

/// What the admins are told about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    /// Telnyx found a problem: the order needs a fix.
    Exception,
    /// The port date is confirmed.
    FocConfirmed,
    /// The numbers are on Telnyx and on the Phone page.
    Ported,
    /// The port was cancelled.
    Cancelled,
    /// Telnyx's porting team wrote a comment.
    Comment,
}

/// "“Main line” (2 numbers)", or "2 numbers" without a label.
fn port_name(row: &PortRow) -> String {
    let count = row.numbers().len();
    let numbers = if count == 1 {
        "1 number".to_string()
    } else {
        format!("{count} numbers")
    };
    let label = row.label.trim();
    if label.is_empty() {
        numbers
    } else {
        format!("“{label}” ({numbers})")
    }
}

fn html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The port date in the company's time zone: "Oct 30, 2026 at 10:00 AM EDT".
async fn port_date(pool: &MySqlPool, row: &PortRow) -> sqlx::Result<String> {
    let Some(at) = row.foc_at.or(row.foc_requested_at) else {
        return Ok("the confirmed date".into());
    };
    let config = company::load(pool, row.company_id).await?;
    let zone = company::time_zone_of(config.as_ref());
    Ok(at
        .with_timezone(&zone)
        .format("%b %-d, %Y at %-I:%M %p %Z")
        .to_string())
}

/// Tells the company's admins (bell and email) about the order.
pub async fn notify(state: &AppState, row: &PortRow, notice: Notice) -> sqlx::Result<()> {
    let pool = &state.pool;
    let name = port_name(row);
    let (subject, message) = match notice {
        Notice::Exception => (
            "A number port needs your attention".to_string(),
            format!(
                "Telnyx found a problem with the port of {name}. Open Admin → Phone to see \
                 what to fix."
            ),
        ),
        Notice::FocConfirmed => (
            "Port date confirmed".to_string(),
            format!(
                "The port of {name} to Telnyx is confirmed for {}.",
                port_date(pool, row).await?
            ),
        ),
        Notice::Ported => (
            "Your numbers moved to Telnyx".to_string(),
            format!(
                "The port of {name} is done: the numbers are on Admin → Phone. Choose who \
                 they ring."
            ),
        ),
        Notice::Cancelled => (
            "Number port cancelled".to_string(),
            format!(
                "The port of {name} was cancelled. The numbers stay with your current carrier."
            ),
        ),
        Notice::Comment => (
            "Telnyx commented on your number port".to_string(),
            format!(
                "Telnyx's porting team wrote about the port of {name}. Open Admin → Phone to \
                 read it and reply."
            ),
        ),
    };
    let admins = company_admins(pool, row.company_id, None).await?;
    if admins.is_empty() {
        return Ok(());
    }
    let bell: String = message.chars().take(255).collect();
    let mut tx = pool.begin().await?;
    for admin in &admins {
        sqlx::query(
            "INSERT INTO notifications (user_id, message, notification_type, actor_name, due_at)
             VALUES (?, ?, 'phone_port', ?, NOW())",
        )
        .bind(admin)
        .bind(&bell)
        .bind(ACTOR)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    let mut query = sqlx::QueryBuilder::new(
        "SELECT email FROM users WHERE is_deleted = 0 AND email IS NOT NULL AND email <> ''",
    );
    crate::sql::push_in(&mut query, "id", &admins);
    let emails: Vec<String> = query.build_query_scalar().fetch_all(pool).await?;
    let subdomain = company_subdomain(pool, row.company_id).await?;
    let link = format!(
        "{}/admin/phone/ports/{}",
        app_origin(&state.app_url, subdomain.as_deref()),
        row.id
    );
    for email in emails {
        let outgoing = OutgoingEmail {
            from_name: Some("Granite Phone".into()),
            from_address: NO_REPLY.into(),
            to: vec![email],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: format!("[Granite Phone] {subject}"),
            body: format!(
                "<p>{}</p><p><a href=\"{}\">Open the port</a></p>",
                html(&message),
                html(&link)
            ),
            signature: None,
            reply_to: Vec::new(),
            in_reply_to: None,
            references: Vec::new(),
            attachments: Vec::new(),
            transactional: true,
        };
        if let Err(err) = state.webhooks.send_email(&outgoing).await {
            tracing::warn!(?err, order_id = row.id, "port notification email not sent");
        }
    }
    Ok(())
}

/// What the job's porting step did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct JobRun {
    /// Orders read again from Telnyx.
    pub refreshed: usize,
    /// Ported orders whose numbers were set up.
    pub completed: usize,
}

/// The phone job's porting step: retries setting up ported numbers (each
/// failure waits longer, up to 12 hours; 30 tries), then reads open orders
/// from Telnyx again (drafts every 6 hours, orders with a confirmed date
/// every 15 minutes, others hourly; webhooks are the fast path). A few
/// each run, none started after `deadline`.
pub async fn run_job(state: &AppState, deadline: Instant) -> sqlx::Result<JobRun> {
    let mut run = JobRun::default();
    if !state.telnyx.is_enabled() {
        return Ok(run);
    }
    let pool = &state.pool;
    let due: Vec<(i64, i32)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT id, company_id FROM phone_port_orders
          WHERE status = 'ported' AND completed_at IS NULL
            AND completion_attempts < {MAX_COMPLETION_ATTEMPTS}
            AND (completion_tried_at IS NULL
                 OR completion_tried_at < UTC_TIMESTAMP()
                    - INTERVAL LEAST(1 << LEAST(completion_attempts, 10), 720) MINUTE)
          ORDER BY id LIMIT {JOB_BATCH}"
    )))
    .fetch_all(pool)
    .await?;
    for (id, company_id) in due {
        if Instant::now() >= deadline {
            return Ok(run);
        }
        if let Some(row) = load(pool, company_id, id).await?
            && complete(state, &row, deadline).await?
        {
            run.completed += 1;
        }
    }
    let stale: Vec<(i64, i32, Option<DateTime<Utc>>)> =
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT id, company_id, refreshed_at FROM phone_port_orders
          WHERE status IN ({OPEN_STATUSES})
            AND (refreshed_at IS NULL OR refreshed_at < UTC_TIMESTAMP() - INTERVAL
                 (CASE status WHEN 'draft' THEN 360 WHEN 'foc-date-confirmed' THEN 15 ELSE 60 END)
                 MINUTE)
          ORDER BY refreshed_at IS NOT NULL, refreshed_at, id LIMIT {JOB_BATCH}"
        )))
        .fetch_all(pool)
        .await?;
    for (id, company_id, seen) in stale {
        if Instant::now() >= deadline {
            break;
        }
        // Claimed by moving `refreshed_at`: an overlapping run skips it, and a
        // failure waits for the next interval.
        let claimed = sqlx::query(
            "UPDATE phone_port_orders SET refreshed_at = UTC_TIMESTAMP()
              WHERE id = ? AND refreshed_at <=> ?",
        )
        .bind(id)
        .bind(seen)
        .execute(pool)
        .await?
        .rows_affected()
            == 1;
        if !claimed {
            continue;
        }
        let Some(row) = load(pool, company_id, id).await? else {
            continue;
        };
        match refresh(state, &row).await {
            Ok(after) => {
                run.refreshed += 1;
                if after.completed_at.is_some() && row.completed_at.is_none() {
                    run.completed += 1;
                }
            }
            Err(RefreshError::Database(err)) => return Err(err),
            Err(RefreshError::Telnyx(err)) => {
                tracing::warn!(error = %err, order_id = id, "port order not refreshed");
            }
        }
    }
    Ok(run)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(billing: Option<&str>) -> CompanyConfig {
        CompanyConfig {
            company_id: 100,
            enabled: true,
            company_deleted: false,
            webhook_token: "t".into(),
            credential_connection_id: Some("conn-100".into()),
            texml_app_id: Some("app-100".into()),
            outbound_voice_profile_id: None,
            billing_group_id: billing.map(str::to_string),
            telnyx_account_id: None,
            ai_assistant_id: None,
            fallback_texml_url: None,
            recording_mode: company::RecordingMode::Inbound,
            recording_notice_url: None,
            messaging_profile_id: None,
            business_hours: None,
            holidays: Vec::new(),
            min_app_version: None,
            daily_call_limit: 300,
        }
    }

    #[test]
    fn orders_are_set_up_for_calls_and_texts() {
        assert_eq!(
            setup_changes(&config(None), "https://api.test/hook", None, Some(true)),
            json!({ "webhook_url": "https://api.test/hook",
                    "phone_number_configuration": { "connection_id": "app-100" } })
        );
        assert_eq!(
            setup_changes(
                &config(Some("bg-1")),
                "https://api.test/hook",
                Some("mp-1"),
                Some(true)
            ),
            json!({ "webhook_url": "https://api.test/hook",
                    "phone_number_configuration": { "connection_id": "app-100",
                        "billing_group_id": "bg-1", "messaging_profile_id": "mp-1" },
                    "messaging": { "enable_messaging": true } })
        );
        // Messaging is not asked for numbers that cannot text on Telnyx.
        let changes = setup_changes(&config(None), "h", Some("mp-1"), Some(false));
        assert_eq!(changes.get("messaging"), None);
        assert_eq!(
            changes["phone_number_configuration"]["messaging_profile_id"],
            "mp-1"
        );
    }

    #[test]
    fn statuses_say_what_an_admin_may_do() {
        use PortStatus::*;
        assert!(Draft.editable() && Draft.submittable() && !Draft.cancellable());
        assert!(Exception.editable() && Exception.submittable() && Exception.cancellable());
        assert!(!Submitted.editable() && Submitted.cancellable());
        assert!(FocDateConfirmed.cancellable() && FocDateConfirmed.is_open());
        assert!(!CancelPending.cancellable() && CancelPending.is_open());
        for done in [Ported, Cancelled, Deleted] {
            assert!(!done.is_open() && !done.editable() && !done.cancellable());
        }
        assert_eq!(
            PortStatus::parse("foc-date-confirmed"),
            Some(FocDateConfirmed)
        );
        assert_eq!(PortStatus::parse("weird"), None);
    }
}
