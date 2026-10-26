//! Move to plan: a superuser invites a legacy company's Managers to accept a
//! subscription plan.
//!
//! The superuser picks the add-ons, the first charge day and how long the
//! invite works; every Manager of the company is emailed a link to the
//! payments app (`/upgrade/<token>`). A Manager accepts the consents and a
//! payment method there (`accept`); the company then gets a Stripe
//! subscription whose free period ends on the first charge day, and only
//! after it exists does the company switch to `plan_source = 'subscription'`.
//! A live legacy subscription forces the first charge to its period end and
//! is cancelled at that end, so nobody is billed twice or gets free months.
//!
//! Tokens are bearer secrets: only their SHA-256 is stored, never logged.

pub mod accept;
pub mod emails;

use super::catalog::{Catalog, INCLUDED_USERS, PriceKind, SIGNUP_TZ, signup_day};
use super::lock::{CompanyLock, LockError};
use super::store::{BillingStore, MySqlBillingStore};
use super::stripe::{StripeError, subscription_snapshot};
use super::{
    CompanyBilling, MANAGER_POSITION, PaymentStatus, is_ended_subscription_status,
    is_paying_subscription_status, seats,
};
use crate::entitlements::AddonKey;
use crate::error::{ApiError, ApiResult};
use crate::signup::consent::{long_date, sha256_hex};
use crate::signup::payment;
use crate::state::AppState;
use axum::http::StatusCode;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use lambda_http::tracing;
use serde::Serialize;
use sqlx::MySqlPool;
use utoipa::ToSchema;

/// How long an invite works unless the superuser says otherwise.
pub const DEFAULT_EXPIRES_IN_DAYS: i64 = 14;
pub const MAX_EXPIRES_IN_DAYS: i64 = 30;
/// The first charge day offered by default: today + 30 days.
pub const DEFAULT_FIRST_CHARGE_DAYS: i64 = 30;
/// The latest first charge day: today + 730 days (Stripe's trial limit).
pub const MAX_FIRST_CHARGE_DAYS: i64 = 730;
/// A picked first charge day is charged at 09:00 in [`SIGNUP_TZ`].
const FIRST_CHARGE_HOUR: u32 = 9;
/// Days between accepting (or the invite's expiry) and the first charge, so
/// the reminder promised 7 days before it can be sent.
pub const MIN_DAYS_BEFORE_FIRST_CHARGE: i64 = 8;
/// A forced legacy date leaves the invite at least this long to be accepted.
const LEGACY_MIN_INVITE_DAYS: i64 = 1;
/// Invites listed on the company screen.
const LIST_LIMIT: i64 = 20;

/// Where an invite is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum InviteStatus {
    /// Waiting for a Manager (expired once `expires_at` passed).
    Open,
    /// A Manager accepted; the subscription is being made.
    Accepting,
    /// The company is on the plan.
    Accepted,
    /// Withdrawn by a superuser, or replaced by a newer invite.
    Revoked,
}

impl InviteStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Accepting => "accepting",
            Self::Accepted => "accepted",
            Self::Revoked => "revoked",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        [Self::Open, Self::Accepting, Self::Accepted, Self::Revoked]
            .into_iter()
            .find(|status| status.as_str() == value)
    }
}

/// An invite as people see it: `expired` is an open invite past its time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum InviteState {
    /// Waiting for a Manager to accept.
    Open,
    /// A Manager accepted; the subscription is being made.
    Accepting,
    /// The company is on the plan.
    Accepted,
    /// Withdrawn by a superuser, or replaced by a newer invite.
    Revoked,
    /// Nobody accepted before `expires_at`.
    Expired,
}

/// A `plan_invites` row.
#[derive(Debug, Clone)]
pub struct Invite {
    pub id: String,
    pub company_id: i32,
    pub created_by: i32,
    pub addons: Vec<AddonKey>,
    /// The first charge day shown when it was made.
    pub first_charge_on: NaiveDate,
    /// Its instant (UTC).
    pub first_charge_at: NaiveDateTime,
    /// The live legacy subscription that forced the date (cancelled at its
    /// period end on acceptance).
    pub legacy_subscription_id: Option<String>,
    pub expires_at: NaiveDateTime,
    pub status: InviteStatus,
    pub accepted_by: Option<i32>,
    pub accepting_at: Option<NaiveDateTime>,
    /// Frozen at acceptance: when the free period ends.
    pub trial_end_at: Option<NaiveDateTime>,
    /// Frozen at acceptance: the base price, then the add-ons.
    pub price_ids: Option<Vec<String>>,
    pub stripe_customer_id: Option<String>,
    pub payment_method_id: Option<String>,
    pub stripe_subscription_id: Option<String>,
    pub provision_attempts: u8,
    /// Why the last run failed (codes only).
    pub last_error: Option<String>,
    /// When the maintenance job stopped resuming the acceptance.
    pub gave_up_at: Option<NaiveDateTime>,
    pub accepted_at: Option<NaiveDateTime>,
    pub revoked_at: Option<NaiveDateTime>,
    pub created_at: NaiveDateTime,
}

impl Invite {
    /// Open and past its time.
    pub fn is_expired(&self, now: NaiveDateTime) -> bool {
        self.status == InviteStatus::Open && self.expires_at <= now
    }

    pub fn state(&self, now: NaiveDateTime) -> InviteState {
        match self.status {
            InviteStatus::Open if self.is_expired(now) => InviteState::Expired,
            InviteStatus::Open => InviteState::Open,
            InviteStatus::Accepting => InviteState::Accepting,
            InviteStatus::Accepted => InviteState::Accepted,
            InviteStatus::Revoked => InviteState::Revoked,
        }
    }

    /// The day the first charge lands on ([`SIGNUP_TZ`]): the frozen free
    /// period's end once accepted, else the day the invite was made with.
    pub fn first_charge_day(&self) -> NaiveDate {
        self.trial_end_at.map_or(self.first_charge_on, local_day)
    }
}

/// The columns of [`Invite`] on the `plan_invites` alias `p`.
fn columns(p: &str) -> String {
    format!(
        "{p}id, {p}company_id, {p}created_by, CAST({p}addons AS CHAR) AS addons, {p}first_charge_on,
         {p}first_charge_at, {p}legacy_subscription_id, {p}expires_at,
         CAST({p}status AS CHAR) AS status, {p}accepted_by, {p}accepting_at, {p}trial_end_at,
         CAST({p}price_ids AS CHAR) AS price_ids, {p}stripe_customer_id, {p}payment_method_id,
         {p}stripe_subscription_id, {p}provision_attempts, {p}last_error, {p}gave_up_at,
         {p}accepted_at, {p}revoked_at, {p}created_at"
    )
}

#[derive(sqlx::FromRow)]
struct Row {
    id: String,
    company_id: i32,
    created_by: i32,
    addons: String,
    first_charge_on: NaiveDate,
    first_charge_at: NaiveDateTime,
    legacy_subscription_id: Option<String>,
    expires_at: NaiveDateTime,
    status: String,
    accepted_by: Option<i32>,
    accepting_at: Option<NaiveDateTime>,
    trial_end_at: Option<NaiveDateTime>,
    price_ids: Option<String>,
    stripe_customer_id: Option<String>,
    payment_method_id: Option<String>,
    stripe_subscription_id: Option<String>,
    provision_attempts: u8,
    last_error: Option<String>,
    gave_up_at: Option<NaiveDateTime>,
    accepted_at: Option<NaiveDateTime>,
    revoked_at: Option<NaiveDateTime>,
    created_at: NaiveDateTime,
}

fn corrupt(column: &str) -> sqlx::Error {
    sqlx::Error::ColumnDecode {
        index: column.to_string(),
        source: format!("plan_invites.{column} is not valid").into(),
    }
}

impl TryFrom<Row> for Invite {
    type Error = sqlx::Error;

    fn try_from(row: Row) -> sqlx::Result<Self> {
        let status = InviteStatus::parse(&row.status).ok_or_else(|| corrupt("status"))?;
        let addons: Vec<AddonKey> =
            serde_json::from_str(&row.addons).map_err(|_| corrupt("addons"))?;
        let price_ids = row
            .price_ids
            .map(|json| serde_json::from_str(&json).map_err(|_| corrupt("price_ids")))
            .transpose()?;
        Ok(Self {
            id: row.id,
            company_id: row.company_id,
            created_by: row.created_by,
            addons,
            first_charge_on: row.first_charge_on,
            first_charge_at: row.first_charge_at,
            legacy_subscription_id: row.legacy_subscription_id,
            expires_at: row.expires_at,
            status,
            accepted_by: row.accepted_by,
            accepting_at: row.accepting_at,
            trial_end_at: row.trial_end_at,
            price_ids,
            stripe_customer_id: row.stripe_customer_id,
            payment_method_id: row.payment_method_id,
            stripe_subscription_id: row.stripe_subscription_id,
            provision_attempts: row.provision_attempts,
            last_error: row.last_error,
            gave_up_at: row.gave_up_at,
            accepted_at: row.accepted_at,
            revoked_at: row.revoked_at,
            created_at: row.created_at,
        })
    }
}

/// The invite by id; `None` when unknown (or not a UUID).
pub async fn load(pool: &MySqlPool, id: &str) -> sqlx::Result<Option<Invite>> {
    if uuid::Uuid::parse_str(id).is_err() {
        return Ok(None);
    }
    let row: Option<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {} FROM plan_invites p WHERE p.id = ?",
        columns("p.")
    )))
    .bind(id.to_ascii_lowercase())
    .fetch_optional(pool)
    .await?;
    row.map(Invite::try_from).transpose()
}

/// The invite of `company_id` (another company's id is `None`).
pub async fn load_for_company(
    pool: &MySqlPool,
    company_id: i32,
    id: &str,
) -> sqlx::Result<Option<Invite>> {
    Ok(load(pool, id)
        .await?
        .filter(|invite| invite.company_id == company_id))
}

/// A token shaped like ours (base64url); anything else is never looked up.
fn plausible_token(token: &str) -> bool {
    (20..=128).contains(&token.len())
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The invite a link's token opens; `None` for an unknown token.
pub async fn by_token(pool: &MySqlPool, token: &str) -> sqlx::Result<Option<Invite>> {
    if !plausible_token(token) {
        return Ok(None);
    }
    let invite_id: Option<String> =
        sqlx::query_scalar("SELECT invite_id FROM plan_invite_tokens WHERE token_sha256 = ?")
            .bind(token_hash(token))
            .fetch_optional(pool)
            .await?;
    match invite_id {
        Some(id) => load(pool, &id).await,
        None => Ok(None),
    }
}

/// The company's invite a Manager can still accept (open, not expired).
pub async fn open_for_company(pool: &MySqlPool, company_id: i32) -> sqlx::Result<Option<Invite>> {
    let row: Option<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {} FROM plan_invites p
          WHERE p.live_company_id = ? AND p.status = 'open' AND p.expires_at > ?",
        columns("p.")
    )))
    .bind(company_id)
    .bind(super::now())
    .fetch_optional(pool)
    .await?;
    row.map(Invite::try_from).transpose()
}

/// A new link token: 32 random bytes, base64url without padding.
pub fn new_token() -> ApiResult<String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| ApiError::Internal("no randomness".into()))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// What `plan_invite_tokens` stores for `token`: its SHA-256, hex.
pub fn token_hash(token: &str) -> String {
    sha256_hex(token)
}

/// The link a Manager opens: `PAYMENTS_APP_URL` + `/upgrade/<token>`.
pub fn upgrade_url(token: &str) -> String {
    format!("{}/upgrade/{token}", super::payments_app_url())
}

/// Stores a token for `user_id` and returns its link.
pub async fn mint_link(
    conn: impl sqlx::MySqlExecutor<'_>,
    invite_id: &str,
    user_id: i32,
) -> ApiResult<String> {
    let token = new_token()?;
    sqlx::query(
        "INSERT INTO plan_invite_tokens (token_sha256, invite_id, user_id, created_at)
         VALUES (?, ?, ?, ?)",
    )
    .bind(token_hash(&token))
    .bind(invite_id)
    .bind(user_id)
    .bind(super::now())
    .execute(conn)
    .await?;
    Ok(upgrade_url(&token))
}

/// The buyer's calendar day of a UTC instant ([`SIGNUP_TZ`]).
pub fn local_day(at: NaiveDateTime) -> NaiveDate {
    SIGNUP_TZ.from_utc_datetime(&at).date_naive()
}

/// 09:00 in [`SIGNUP_TZ`] on `day`, in UTC (a DST gap moves it an hour on).
pub fn first_charge_instant(day: NaiveDate) -> NaiveDateTime {
    let wall = day
        .and_hms_opt(FIRST_CHARGE_HOUR, 0, 0)
        .unwrap_or_else(|| day.and_time(NaiveTime::MIN));
    SIGNUP_TZ
        .from_local_datetime(&wall)
        .earliest()
        .or_else(|| {
            SIGNUP_TZ
                .from_local_datetime(&(wall + Duration::hours(1)))
                .earliest()
        })
        .map_or(wall, |at| at.naive_utc())
}

fn coded(status: StatusCode, code: &'static str, message: &str) -> ApiError {
    ApiError::coded(status, code, message)
}

fn conflict(code: &'static str, message: &str) -> ApiError {
    coded(StatusCode::CONFLICT, code, message)
}

fn bad(code: &'static str, message: &str) -> ApiError {
    coded(StatusCode::BAD_REQUEST, code, message)
}

/// 404 `invite_not_found`.
pub fn not_found() -> ApiError {
    coded(
        StatusCode::NOT_FOUND,
        "invite_not_found",
        "This invite was not found.",
    )
}

/// 503 `catalog_unavailable`.
pub fn catalog_unavailable() -> ApiError {
    coded(
        StatusCode::SERVICE_UNAVAILABLE,
        "catalog_unavailable",
        CATALOG_UNAVAILABLE,
    )
}

const CATALOG_UNAVAILABLE: &str = "Prices are not available right now. Try again later.";
const ALREADY_ON_PLAN: &str = "This company is already on a plan.";
const NO_MANAGERS: &str = "This company has no Manager with an email. Add one first.";
const LEGACY_OVERDUE: &str =
    "This company has an overdue payment. Settle it before moving them to a plan.";
const INVITE_ACCEPTING: &str =
    "A Manager is accepting the current invite. Try again in a few minutes.";
const INVITE_CLOSED: &str = "This invite is no longer open.";
const PAYMENT_UNAVAILABLE: &str = "Payment collection is not live yet, so a Manager could not accept. Invites can be sent once it is.";
const INVITE_HAS_SUBSCRIPTION: &str = "Stripe already has a subscription for this invite. Finish or cancel it in Stripe before revoking.";
const STRIPE_UNREADABLE: &str =
    "Stripe cannot be read right now. Nothing changed. Try again later.";

fn lock_failure(err: LockError) -> ApiError {
    match err {
        LockError::Busy => ApiError::Unavailable(super::lock::BUSY_MESSAGE.into()),
        LockError::Database(err) => ApiError::Database(err),
    }
}

/// The prices invites quote and charge; `None` (logged) when Stripe cannot
/// say or they no longer match what the consent texts state.
pub async fn load_catalog(stripe: &super::stripe::Stripe) -> Option<Catalog> {
    match Catalog::load(stripe).await {
        Ok(catalog) => match crate::signup::consent::catalog_mismatch(&catalog) {
            Some(mismatch) => {
                tracing::error!(%mismatch, "plan invite prices differ from the consent texts");
                None
            }
            None => Some(catalog),
        },
        Err(err) => {
            tracing::warn!(error = %err.reason(), "plan invite catalog unavailable");
            None
        }
    }
}

/// A Manager an invite goes to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema, sqlx::FromRow)]
pub struct InviteManager {
    /// User id.
    pub id: i32,
    /// Name (the email when the user has no name).
    pub name: String,
    /// Where the invite is emailed.
    pub email: String,
}

/// The company's live users holding the Manager position in it and having
/// an email (the rule of `MySqlBillingStore::managers`), by id.
pub async fn managers(pool: &MySqlPool, company_id: i32) -> sqlx::Result<Vec<InviteManager>> {
    sqlx::query_as(
        "SELECT u.id, COALESCE(NULLIF(TRIM(u.name), ''), TRIM(u.email)) AS name,
                TRIM(u.email) AS email
           FROM users u
          WHERE u.company_id = ? AND u.is_deleted = 0 AND TRIM(COALESCE(u.email, '')) <> ''
            AND EXISTS (SELECT 1 FROM users_positions up
                         WHERE up.user_id = u.id AND up.company_id = u.company_id
                           AND up.position_id = ?)
          ORDER BY u.id",
    )
    .bind(company_id)
    .bind(MANAGER_POSITION)
    .fetch_all(pool)
    .await
}

/// A live company's name and plan.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CompanyPlan {
    pub name: String,
    /// `legacy` or `subscription`.
    pub plan_source: String,
}

pub async fn company(pool: &MySqlPool, company_id: i32) -> sqlx::Result<Option<CompanyPlan>> {
    sqlx::query_as(
        "SELECT COALESCE(name, '') AS name, CAST(plan_source AS CHAR) AS plan_source
           FROM company WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(company_id)
    .fetch_optional(pool)
    .await
}

/// The company's legacy Stripe subscription while it has not ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacySubscription {
    pub id: String,
    /// Stripe's status (never `canceled` or `incomplete_expired`).
    pub status: String,
    pub plan_name: Option<String>,
    pub monthly_cents: Option<i64>,
    pub current_period_end: NaiveDateTime,
    /// It has self-serve prices: the company is on a plan already.
    pub self_serve: bool,
}

impl LegacySubscription {
    pub fn is_overdue(&self) -> bool {
        matches!(self.status.as_str(), "past_due" | "unpaid")
    }

    /// Paying or overdue; any other status (`paused`, `incomplete`) cannot be
    /// replaced safely.
    pub fn is_supported(&self) -> bool {
        is_paying_subscription_status(&self.status) || self.is_overdue()
    }
}

/// The company owes money: access paused for an unpaid invoice, or a balance due.
pub fn owes(record: &CompanyBilling) -> bool {
    record.payment_status == PaymentStatus::Suspended || record.outstanding_balance_cents > 0
}

/// The latest an invite forced to a legacy `period_end` may expire, so that
/// accepting it still leaves [`MIN_DAYS_BEFORE_FIRST_CHARGE`] days.
pub fn latest_legacy_expiry(period_end: NaiveDateTime) -> NaiveDateTime {
    period_end - Duration::days(MIN_DAYS_BEFORE_FIRST_CHARGE)
}

/// The subscription `record` names, read from Stripe now (the stored row
/// when Stripe cannot be read), unless it has ended.
pub async fn legacy_subscription(
    state: &AppState,
    record: Option<&CompanyBilling>,
) -> Option<LegacySubscription> {
    let record = record?;
    let id = record.stripe_subscription_id.clone()?;
    let live = if state.stripe.is_enabled() {
        match state.stripe.subscription(&id).await {
            Ok(subscription) => Some(subscription_snapshot(&subscription)),
            Err(StripeError::Missing(_)) => return None,
            Err(err) => {
                tracing::warn!(
                    company_id = record.company_id,
                    stripe_code = err.code(),
                    "legacy subscription not read; using the stored row"
                );
                None
            }
        }
    } else {
        None
    };
    let legacy = match live {
        Some(snapshot) => LegacySubscription {
            status: snapshot.status,
            plan_name: snapshot.plan_name.or_else(|| record.plan_name.clone()),
            monthly_cents: snapshot
                .monthly_total_cents
                .or_else(|| {
                    snapshot
                        .unit_amount_cents
                        .zip(snapshot.quantity)
                        .map(|(unit, quantity)| unit * quantity)
                })
                .or(record.monthly_price_cents),
            current_period_end: snapshot.period_end.or(record.current_period_end)?,
            self_serve: snapshot.items.iter().any(|item| {
                item.lookup_key
                    .as_deref()
                    .and_then(PriceKind::from_lookup_key)
                    .is_some()
            }),
            id,
        },
        None => LegacySubscription {
            status: record.subscription_status.clone(),
            plan_name: record.plan_name.clone(),
            monthly_cents: record.monthly_price_cents,
            current_period_end: record.current_period_end?,
            self_serve: record.extra_user_item_id.is_some(),
            id,
        },
    };
    (!is_ended_subscription_status(&legacy.status)).then_some(legacy)
}

/// What deciding a move needs.
pub struct MoveFacts {
    pub company: CompanyPlan,
    pub managers: Vec<InviteManager>,
    pub legacy: Option<LegacySubscription>,
    /// The billing record says the company owes money ([`owes`]).
    pub owes: bool,
    /// A Manager is accepting an invite right now.
    pub accepting: bool,
    /// A Manager can add a payment method on acceptance.
    pub payment_ready: bool,
    pub catalog: Option<Catalog>,
    pub now: NaiveDateTime,
}

impl MoveFacts {
    /// Why no invite can be made now: `(code, message)`, the first that applies.
    pub fn blocked(&self) -> Option<(&'static str, String)> {
        let fixed = |code: &'static str, message: &str| Some((code, message.to_string()));
        let legacy = self.legacy.as_ref();
        if self.company.plan_source == "subscription"
            || legacy.is_some_and(|legacy| legacy.self_serve)
        {
            fixed("already_on_plan", ALREADY_ON_PLAN)
        } else if self.accepting {
            fixed("invite_accepting", INVITE_ACCEPTING)
        } else if !self.payment_ready {
            // Step 13 removes this guard once the Payment Element collects payment.
            fixed("payment_unavailable", PAYMENT_UNAVAILABLE)
        } else if self.managers.is_empty() {
            fixed("no_managers", NO_MANAGERS)
        } else if self.owes || legacy.is_some_and(LegacySubscription::is_overdue) {
            fixed("legacy_overdue", LEGACY_OVERDUE)
        } else if let Some(legacy) = legacy.filter(|legacy| !legacy.is_supported()) {
            Some((
                "legacy_subscription_unsupported",
                format!(
                    "The company's current Stripe subscription is {}. Resolve it in Stripe before moving them to a plan.",
                    legacy.status
                ),
            ))
        } else if let Some(legacy) = self.forcing_legacy().filter(|legacy| {
            latest_legacy_expiry(legacy.current_period_end)
                < self.now + Duration::days(LEGACY_MIN_INVITE_DAYS)
        }) {
            Some((
                "legacy_period_too_close",
                format!(
                    "The current plan renews on {}, too soon to switch. Send the invite after it renews.",
                    long_date(local_day(legacy.current_period_end))
                ),
            ))
        } else if self.catalog.is_none() {
            fixed("catalog_unavailable", CATALOG_UNAVAILABLE)
        } else {
            None
        }
    }

    /// The live legacy subscription that forces the first charge date.
    pub fn forcing_legacy(&self) -> Option<&LegacySubscription> {
        self.legacy
            .as_ref()
            .filter(|legacy| is_paying_subscription_status(&legacy.status))
    }
}

/// The facts of a move for `company_id`; 404 for an unknown company.
pub async fn move_facts(state: &AppState, company_id: i32) -> ApiResult<MoveFacts> {
    let pool = &state.pool;
    let company = company(pool, company_id).await?.ok_or(ApiError::NotFound)?;
    let accepting: i64 = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM plan_invites WHERE company_id = ? AND status = 'accepting')",
    )
    .bind(company_id)
    .fetch_one(pool)
    .await?;
    let on_plan = company.plan_source == "subscription";
    let record = if on_plan {
        None
    } else {
        MySqlBillingStore(pool).by_company(company_id).await?
    };
    Ok(MoveFacts {
        managers: managers(pool, company_id).await?,
        legacy: legacy_subscription(state, record.as_ref()).await,
        owes: record.as_ref().is_some_and(owes),
        accepting: accepting != 0,
        payment_ready: payment::is_test_mode(state),
        catalog: if on_plan {
            None
        } else {
            load_catalog(&state.stripe).await
        },
        now: super::now(),
        company,
    })
}

/// The add-on keys of a request: known keys, deduplicated, in
/// [`AddonKey::ALL`] order; 400 `invalid_addon` otherwise.
pub fn check_addons(addons: &[String]) -> ApiResult<Vec<AddonKey>> {
    let mut keys = Vec::new();
    for addon in addons {
        let key = AddonKey::parse(addon.trim()).ok_or_else(|| {
            bad(
                "invalid_addon",
                "Pick add-ons from signatures, quotes and quickbooks.",
            )
        })?;
        keys.push(key);
    }
    Ok(AddonKey::ALL
        .into_iter()
        .filter(|key| keys.contains(key))
        .collect())
}

/// `expires_in_days`, 1 to 30 (default 14); 400 `invalid_expiry` otherwise.
pub fn check_expiry(days: Option<i64>) -> ApiResult<i64> {
    let days = days.unwrap_or(DEFAULT_EXPIRES_IN_DAYS);
    if (1..=MAX_EXPIRES_IN_DAYS).contains(&days) {
        Ok(days)
    } else {
        Err(bad("invalid_expiry", "The invite can last 1 to 30 days."))
    }
}

/// A picked first charge day: within today + 730 days, and at least
/// [`MIN_DAYS_BEFORE_FIRST_CHARGE`] days after the day the invite expires;
/// 400 `invalid_first_charge` otherwise.
pub fn check_first_charge(
    day: Option<&str>,
    today: NaiveDate,
    expires_on: NaiveDate,
) -> ApiResult<NaiveDate> {
    let invalid = |message: &str| bad("invalid_first_charge", message);
    let text = day
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| invalid("Pick a first charge date."))?;
    let day = NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .map_err(|_| invalid("Pick a first charge date like 2026-11-23."))?;
    if day <= today {
        return Err(invalid("Pick a first charge date from tomorrow on."));
    }
    if day > today + Duration::days(MAX_FIRST_CHARGE_DAYS) {
        return Err(invalid("Pick a first charge date within two years."));
    }
    if day < expires_on + Duration::days(MIN_DAYS_BEFORE_FIRST_CHARGE) {
        return Err(invalid(&format!(
            "Pick a first charge date at least {MIN_DAYS_BEFORE_FIRST_CHARGE} days after the invite expires."
        )));
    }
    Ok(day)
}

/// Body of an invite request, checked.
#[derive(Debug, Clone)]
pub struct NewInvite {
    pub addons: Vec<AddonKey>,
    /// `YYYY-MM-DD`; ignored when a legacy subscription forces the date.
    pub first_charge_on: Option<String>,
    pub expires_in_days: i64,
}

/// What sending invite emails did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sent {
    pub emailed: usize,
    /// Addresses the email could not be sent to.
    pub not_emailed: Vec<String>,
}

/// Makes an invite for `company_id` (replacing an open one) and emails every
/// Manager a link; under the company's billing lock. Once saved it succeeds:
/// emails that could not be sent are in `not_emailed`.
pub async fn create(
    state: &AppState,
    company_id: i32,
    superuser_id: i32,
    request: &NewInvite,
) -> ApiResult<(Invite, Sent)> {
    let lock = CompanyLock::acquire(&state.pool, company_id)
        .await
        .map_err(lock_failure)?;
    let created = create_locked(state, company_id, superuser_id, request).await;
    lock.release().await;
    let created = created?;
    let sent = send_invites(
        state,
        &created.invite,
        &created.links,
        &created.catalog,
        &created.company_name,
    )
    .await;
    Ok((created.invite, sent))
}

/// The links made for each Manager, with who they go to.
type Links = Vec<(InviteManager, String)>;

/// A saved invite and what its emails need.
struct Created {
    invite: Invite,
    links: Links,
    catalog: Catalog,
    company_name: String,
}

async fn create_locked(
    state: &AppState,
    company_id: i32,
    superuser_id: i32,
    request: &NewInvite,
) -> ApiResult<Created> {
    let facts = move_facts(state, company_id).await?;
    if let Some((code, message)) = facts.blocked() {
        let status = if code == "catalog_unavailable" {
            StatusCode::SERVICE_UNAVAILABLE
        } else {
            StatusCode::CONFLICT
        };
        return Err(coded(status, code, &message));
    }
    let now = facts.now;
    // Returned as saved: nothing after the commit can fail the request.
    let invite = new_invite(&facts, request, company_id, superuser_id)?;
    let MoveFacts {
        company,
        managers,
        catalog,
        ..
    } = facts;
    let catalog = catalog.ok_or_else(catalog_unavailable)?;
    let addons =
        serde_json::to_string(&invite.addons).map_err(|err| ApiError::Internal(err.to_string()))?;
    let mut tx = state.pool.begin().await?;
    sqlx::query(
        "UPDATE plan_invites SET status = 'revoked', revoked_at = ?, revoked_by = ?, updated_at = ?
          WHERE company_id = ? AND status = 'open'",
    )
    .bind(now)
    .bind(superuser_id)
    .bind(now)
    .bind(company_id)
    .execute(&mut *tx)
    .await?;
    let inserted = sqlx::query(
        "INSERT INTO plan_invites
           (id, company_id, created_by, addons, first_charge_on, first_charge_at,
            legacy_subscription_id, expires_at, status, created_at, updated_at)
         VALUES (?, ?, ?, CAST(? AS JSON), ?, ?, ?, ?, 'open', ?, ?)",
    )
    .bind(&invite.id)
    .bind(company_id)
    .bind(superuser_id)
    .bind(&addons)
    .bind(invite.first_charge_on)
    .bind(invite.first_charge_at)
    .bind(&invite.legacy_subscription_id)
    .bind(invite.expires_at)
    .bind(now)
    .bind(now)
    .execute(&mut *tx)
    .await;
    match inserted {
        Ok(_) => {}
        // An invite moved to `accepting` since the check.
        Err(err) if is_duplicate(&err) => {
            return Err(conflict("invite_accepting", INVITE_ACCEPTING));
        }
        Err(err) => return Err(err.into()),
    }
    let mut links = Vec::new();
    for manager in managers {
        let url = mint_link(&mut *tx, &invite.id, manager.id).await?;
        links.push((manager, url));
    }
    tx.commit().await?;
    Ok(Created {
        invite,
        links,
        catalog,
        company_name: company.name,
    })
}

/// The open invite to save: its first charge and expiry checked or forced.
fn new_invite(
    facts: &MoveFacts,
    request: &NewInvite,
    company_id: i32,
    superuser_id: i32,
) -> ApiResult<Invite> {
    let requested_expiry = facts.now + Duration::days(request.expires_in_days);
    let (expires_at, first_charge_on, first_charge_at, legacy_subscription_id) =
        match facts.forcing_legacy() {
            // The link never outlives the last day it can be accepted on.
            Some(legacy) => (
                requested_expiry.min(latest_legacy_expiry(legacy.current_period_end)),
                local_day(legacy.current_period_end),
                legacy.current_period_end,
                Some(legacy.id.clone()),
            ),
            None => {
                let day = check_first_charge(
                    request.first_charge_on.as_deref(),
                    signup_day(facts.now.and_utc()),
                    local_day(requested_expiry),
                )?;
                (requested_expiry, day, first_charge_instant(day), None)
            }
        };
    Ok(Invite {
        id: uuid::Uuid::new_v4().to_string(),
        company_id,
        created_by: superuser_id,
        addons: request.addons.clone(),
        first_charge_on,
        first_charge_at,
        legacy_subscription_id,
        expires_at,
        status: InviteStatus::Open,
        accepted_by: None,
        accepting_at: None,
        trial_end_at: None,
        price_ids: None,
        stripe_customer_id: None,
        payment_method_id: None,
        stripe_subscription_id: None,
        provision_attempts: 0,
        last_error: None,
        gave_up_at: None,
        accepted_at: None,
        revoked_at: None,
        created_at: facts.now,
    })
}

fn is_duplicate(err: &sqlx::Error) -> bool {
    err.as_database_error()
        .is_some_and(sqlx::error::DatabaseError::is_unique_violation)
}

/// New links for every current Manager of an open invite (the old ones keep
/// working), emailed. The prices load before any link is stored; emails that
/// could not be sent are in `not_emailed`.
pub async fn resend(state: &AppState, company_id: i32, invite_id: &str) -> ApiResult<Sent> {
    let invite = load_for_company(&state.pool, company_id, invite_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if invite.state(super::now()) != InviteState::Open {
        return Err(conflict("invite_closed", INVITE_CLOSED));
    }
    let company = company(&state.pool, company_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let catalog = load_catalog(&state.stripe)
        .await
        .ok_or_else(catalog_unavailable)?;
    let mut tx = state.pool.begin().await?;
    let mut links = Vec::new();
    for manager in managers(&state.pool, company_id).await? {
        let url = mint_link(&mut *tx, &invite.id, manager.id).await?;
        links.push((manager, url));
    }
    tx.commit().await?;
    Ok(send_invites(state, &invite, &links, &catalog, &company.name).await)
}

/// Withdraws an open invite (expired ones too), or an acceptance the
/// maintenance job gave up on while Stripe has no subscription for it.
pub async fn revoke(
    state: &AppState,
    company_id: i32,
    invite_id: &str,
    superuser_id: i32,
) -> ApiResult<()> {
    let invite = load_for_company(&state.pool, company_id, invite_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    // Under the lock a resumed run holds.
    let lock = CompanyLock::acquire(&state.pool, company_id)
        .await
        .map_err(lock_failure)?;
    let revoked = revoke_locked(state, &invite.id, company_id, superuser_id).await;
    lock.release().await;
    revoked
}

async fn revoke_locked(
    state: &AppState,
    invite_id: &str,
    company_id: i32,
    superuser_id: i32,
) -> ApiResult<()> {
    let invite = load(&state.pool, invite_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let given_up = invite.status == InviteStatus::Accepting && invite.gave_up_at.is_some();
    if given_up && has_subscription(state, &invite).await? {
        return Err(conflict("invite_has_subscription", INVITE_HAS_SUBSCRIPTION));
    }
    let now = super::now();
    let revoked = sqlx::query(
        "UPDATE plan_invites SET status = 'revoked', revoked_at = ?, revoked_by = ?, updated_at = ?
          WHERE id = ? AND company_id = ?
            AND (status = 'open'
                 OR (status = 'accepting' AND gave_up_at IS NOT NULL
                     AND stripe_subscription_id IS NULL))",
    )
    .bind(now)
    .bind(superuser_id)
    .bind(now)
    .bind(&invite.id)
    .bind(company_id)
    .execute(&state.pool)
    .await?;
    if revoked.rows_affected() == 1 {
        return Ok(());
    }
    let current = load(&state.pool, &invite.id).await?.map(|i| i.status);
    Err(match current {
        Some(InviteStatus::Accepting) => conflict("invite_accepting", INVITE_ACCEPTING),
        _ => conflict("invite_closed", INVITE_CLOSED),
    })
}

/// Stripe has a subscription for the invite that has not ended: the stored
/// one, or one a run made whose answer was lost (`metadata[plan_invite_id]`,
/// as acceptance looks for it). 503 when Stripe cannot be read.
async fn has_subscription(state: &AppState, invite: &Invite) -> ApiResult<bool> {
    if invite.stripe_subscription_id.is_some() {
        return Ok(true);
    }
    let mut customer = invite.stripe_customer_id.clone();
    if customer.is_none() {
        customer = MySqlBillingStore(&state.pool)
            .by_company(invite.company_id)
            .await?
            .and_then(|record| record.stripe_customer_id);
    }
    let Some(customer) = customer.filter(|_| state.stripe.is_enabled()) else {
        return Ok(false);
    };
    let subscriptions = state
        .stripe
        .list_subscriptions(&customer)
        .await
        .map_err(|err| {
            tracing::warn!(
                company_id = invite.company_id,
                stripe_code = err.code(),
                "plan invite subscriptions not read"
            );
            ApiError::Unavailable(STRIPE_UNREADABLE.into())
        })?;
    Ok(subscriptions.iter().any(|subscription| {
        subscription["metadata"]["plan_invite_id"].as_str() == Some(invite.id.as_str())
            && !is_ended_subscription_status(subscription["status"].as_str().unwrap_or_default())
    }))
}

/// Emails each Manager their link. A failure is logged with the user id,
/// never the address, and the address is returned in `not_emailed`.
async fn send_invites(
    state: &AppState,
    invite: &Invite,
    links: &[(InviteManager, String)],
    catalog: &Catalog,
    company_name: &str,
) -> Sent {
    let users = match seats::billable_users(&state.pool, invite.company_id).await {
        Ok(users) => users,
        Err(err) => {
            tracing::warn!(
                company_id = invite.company_id,
                error = %err,
                "plan invite emails not sent"
            );
            return Sent {
                emailed: 0,
                not_emailed: links
                    .iter()
                    .map(|(manager, _)| manager.email.clone())
                    .collect(),
            };
        }
    };
    let summary = emails::Summary::of(catalog, users, &invite.addons, invite.first_charge_on);
    let legacy_ends_on = invite
        .legacy_subscription_id
        .is_some()
        .then_some(invite.first_charge_on);
    let mut sent = Sent::default();
    for (manager, url) in links {
        let body = emails::invite_html(&emails::InviteEmail {
            name: &manager.name,
            company_name,
            summary: &summary,
            legacy_ends_on,
            link: url,
            expires_on: local_day(invite.expires_at),
        });
        match emails::send(state, &manager.email, emails::INVITE_SUBJECT, body).await {
            Ok(()) => sent.emailed += 1,
            Err(kind) => {
                tracing::warn!(
                    company_id = invite.company_id,
                    user_id = manager.id,
                    kind,
                    "plan invite email failed"
                );
                sent.not_emailed.push(manager.email.clone());
            }
        }
    }
    sent
}

/// An invite on the company screen.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PlanInviteView {
    /// Invite id (UUID).
    pub id: String,
    pub status: InviteState,
    /// The add-ons of the plan, in a fixed order.
    pub addons: Vec<AddonKey>,
    /// The first charge day shown to the Managers (`YYYY-MM-DD`).
    pub first_charge_on: NaiveDate,
    /// When the first charge happens (ISO 8601, UTC): the free period's end.
    pub first_charge_at: DateTime<Utc>,
    /// The date is the company's legacy subscription's period end, which
    /// ends there on acceptance.
    pub forced_by_legacy_subscription: bool,
    /// When the links stop working (ISO 8601, UTC).
    pub expires_at: DateTime<Utc>,
    /// When it was made (ISO 8601, UTC).
    pub created_at: DateTime<Utc>,
    /// The superuser who made it, or null when unknown.
    pub created_by_name: Option<String>,
    /// When the company switched to the plan (ISO 8601, UTC), or null.
    pub accepted_at: Option<DateTime<Utc>>,
    /// The Manager who accepted, or null.
    pub accepted_by_name: Option<String>,
    /// When it was withdrawn or replaced (ISO 8601, UTC), or null.
    pub revoked_at: Option<DateTime<Utc>>,
    /// Why the last attempt to finish the switch failed (an error code), or
    /// null.
    pub last_error: Option<String>,
    /// The acceptance stopped being retried (`accepting` only): staff finish
    /// or withdraw it by hand.
    pub gave_up: bool,
}

impl PlanInviteView {
    fn of(invite: &Invite, created_by: Option<String>, accepted_by: Option<String>) -> Self {
        Self {
            id: invite.id.clone(),
            status: invite.state(super::now()),
            addons: invite.addons.clone(),
            first_charge_on: invite.first_charge_day(),
            first_charge_at: invite
                .trial_end_at
                .unwrap_or(invite.first_charge_at)
                .and_utc(),
            forced_by_legacy_subscription: invite.legacy_subscription_id.is_some(),
            expires_at: invite.expires_at.and_utc(),
            created_at: invite.created_at.and_utc(),
            created_by_name: created_by,
            accepted_at: invite.accepted_at.map(|at| at.and_utc()),
            accepted_by_name: accepted_by,
            revoked_at: invite.revoked_at.map(|at| at.and_utc()),
            last_error: invite.last_error.clone(),
            gave_up: invite.gave_up_at.is_some(),
        }
    }
}

#[derive(sqlx::FromRow)]
struct ListRow {
    #[sqlx(flatten)]
    invite: Row,
    created_by_name: Option<String>,
    accepted_by_name: Option<String>,
}

/// The company's invites, newest first (at most 20).
pub async fn list(pool: &MySqlPool, company_id: i32) -> ApiResult<Vec<PlanInviteView>> {
    let rows: Vec<ListRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {}, cu.name AS created_by_name, au.name AS accepted_by_name
           FROM plan_invites p
           LEFT JOIN users cu ON cu.id = p.created_by
           LEFT JOIN users au ON au.id = p.accepted_by
          WHERE p.company_id = ?
          ORDER BY p.created_at DESC, p.live_company_id IS NULL, p.id DESC
          LIMIT ?",
        columns("p.")
    )))
    .bind(company_id)
    .bind(LIST_LIMIT)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| -> ApiResult<PlanInviteView> {
            let invite = Invite::try_from(row.invite)?;
            Ok(PlanInviteView::of(
                &invite,
                row.created_by_name,
                row.accepted_by_name,
            ))
        })
        .collect()
}

/// The view of one invite (after creating it); without names when they
/// cannot be read.
pub async fn view(pool: &MySqlPool, invite: &Invite) -> PlanInviteView {
    let names: Vec<(i32, Option<String>)> = sqlx::query_as(
        "SELECT id, name FROM users WHERE id IN (?, ?)",
    )
    .bind(invite.created_by)
    .bind(invite.accepted_by.unwrap_or(invite.created_by))
    .fetch_all(pool)
    .await
    .unwrap_or_else(|err| {
        tracing::warn!(company_id = invite.company_id, error = %err, "invite names not read");
        Vec::new()
    });
    let name_of = |id: Option<i32>| {
        names
            .iter()
            .find(|(user, _)| Some(*user) == id)
            .and_then(|(_, name)| name.clone())
    };
    PlanInviteView::of(
        invite,
        name_of(Some(invite.created_by)),
        name_of(invite.accepted_by),
    )
}

/// The company's legacy subscription (not ended), as the dialog shows it.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct LegacySubscriptionView {
    /// Stripe's status: `active`, `trialing`, `past_due`, `unpaid`, or one
    /// that blocks a move (e.g. `paused`, `incomplete`).
    pub status: String,
    /// The plan's name, or null.
    pub plan_name: Option<String>,
    /// What it charges per month, in cents, or null when unknown.
    pub monthly_cents: Option<i64>,
    /// When its current period ends (ISO 8601, UTC): the new plan's first
    /// charge.
    pub current_period_end: DateTime<Utc>,
    /// That day in the company's calendar (`YYYY-MM-DD`): the first charge
    /// date of an invite, which cannot be changed.
    pub first_charge_on: NaiveDate,
    /// The last day an invite can work (`YYYY-MM-DD`, company calendar): 8
    /// days before the period end. A longer expiry is shortened to it.
    pub latest_expiry_on: NaiveDate,
}

/// Why no invite can be made now.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct MoveBlocked {
    /// `already_on_plan`, `invite_accepting`, `payment_unavailable` (payment
    /// collection is not live yet), `no_managers`, `legacy_overdue` (an
    /// overdue legacy subscription, paused access or a balance due),
    /// `legacy_subscription_unsupported` (its status is not active,
    /// trialing, past_due or unpaid), `legacy_period_too_close` (it renews in
    /// under 9 days: send the invite after it renews) or `catalog_unavailable`.
    pub code: String,
    /// A sentence to show.
    pub message: String,
}

/// An add-on of the price list.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CatalogAddon {
    pub key: AddonKey,
    /// Name to show, e.g. `QuickBooks`.
    pub label: String,
    /// Monthly price in cents, before sales tax.
    pub unit_cents: i64,
}

/// The price list (before sales tax).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CatalogView {
    /// The base plan per month, in cents.
    pub base_cents: i64,
    /// Active users the base plan covers.
    pub included_users: i64,
    /// Each active user above them per month, in cents.
    pub extra_user_cents: i64,
    /// Every add-on.
    pub addons: Vec<CatalogAddon>,
}

impl CatalogView {
    pub fn of(catalog: &Catalog) -> Self {
        Self {
            base_cents: catalog.base.unit_cents,
            included_users: INCLUDED_USERS,
            extra_user_cents: catalog.extra_user.unit_cents,
            addons: catalog
                .addons
                .iter()
                .map(|(key, price)| CatalogAddon {
                    key: *key,
                    label: key.label().to_string(),
                    unit_cents: price.unit_cents,
                })
                .collect(),
        }
    }
}

/// What the dialog starts with, and the dates it allows.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct MoveDefaults {
    /// The first charge day offered (`YYYY-MM-DD`): 30 days from today.
    pub first_charge_on: NaiveDate,
    /// Days the invite works: 14 (1 to 30 allowed).
    pub expires_in_days: i64,
    /// The earliest first charge day: today + 9 (the shortest invite, 1 day,
    /// plus `min_days_after_expiry`).
    pub min_first_charge_on: NaiveDate,
    /// A picked first charge day is at least this many days (8) after the
    /// day the invite expires.
    pub min_days_after_expiry: i64,
    /// The latest first charge day: today + 730 days.
    pub max_first_charge_on: NaiveDate,
}

/// `GET /v1/superuser/companies/{id}/plan-move`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PlanMove {
    /// Company id.
    pub company_id: i32,
    /// Company name.
    pub company_name: String,
    /// `legacy` or `subscription`.
    pub plan_source: String,
    /// Active users the plan would bill now.
    pub billable_users: i64,
    /// The Managers an invite is emailed to.
    pub managers: Vec<InviteManager>,
    /// The company's legacy Stripe subscription that has not ended, or null.
    pub legacy_subscription: Option<LegacySubscriptionView>,
    /// Why no invite can be made now, or null.
    pub blocked: Option<MoveBlocked>,
    /// The price list, or null when it cannot be loaded (or the company is
    /// already on a plan).
    pub catalog: Option<CatalogView>,
    pub defaults: MoveDefaults,
    /// Its invites, newest first (at most 20).
    pub invites: Vec<PlanInviteView>,
}

/// The dialog's facts for `company_id`; 404 for an unknown company.
pub async fn plan_move(state: &AppState, company_id: i32) -> ApiResult<PlanMove> {
    let facts = move_facts(state, company_id).await?;
    let today = signup_day(Utc::now());
    Ok(PlanMove {
        company_id,
        company_name: facts.company.name.clone(),
        plan_source: facts.company.plan_source.clone(),
        billable_users: seats::billable_users(&state.pool, company_id).await?,
        managers: facts.managers.clone(),
        legacy_subscription: facts.legacy.as_ref().map(|legacy| LegacySubscriptionView {
            status: legacy.status.clone(),
            plan_name: legacy.plan_name.clone(),
            monthly_cents: legacy.monthly_cents,
            current_period_end: legacy.current_period_end.and_utc(),
            first_charge_on: local_day(legacy.current_period_end),
            latest_expiry_on: local_day(latest_legacy_expiry(legacy.current_period_end)),
        }),
        blocked: facts.blocked().map(|(code, message)| MoveBlocked {
            code: code.to_string(),
            message,
        }),
        catalog: facts.catalog.as_ref().map(CatalogView::of),
        defaults: MoveDefaults {
            first_charge_on: today + Duration::days(DEFAULT_FIRST_CHARGE_DAYS),
            expires_in_days: DEFAULT_EXPIRES_IN_DAYS,
            min_first_charge_on: today + Duration::days(1 + MIN_DAYS_BEFORE_FIRST_CHARGE),
            min_days_after_expiry: MIN_DAYS_BEFORE_FIRST_CHARGE,
            max_first_charge_on: today + Duration::days(MAX_FIRST_CHARGE_DAYS),
        },
        invites: list(&state.pool, company_id).await?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn code(err: &ApiError) -> &'static str {
        match err {
            ApiError::Coded(coded) => coded.code,
            other => panic!("expected a coded error, got {other:?}"),
        }
    }

    #[test]
    fn addons_are_known_deduplicated_and_ordered() {
        let keys = check_addons(&["quickbooks".into(), " quotes".into(), "quotes".into()]).unwrap();
        assert_eq!(keys, [AddonKey::Quotes, AddonKey::Quickbooks]);
        assert_eq!(check_addons(&[]).unwrap(), []);
        assert_eq!(
            code(&check_addons(&["fax".into()]).unwrap_err()),
            "invalid_addon"
        );
    }

    #[test]
    fn expiry_is_one_to_thirty_days() {
        assert_eq!(check_expiry(None).unwrap(), 14);
        assert_eq!(check_expiry(Some(1)).unwrap(), 1);
        assert_eq!(check_expiry(Some(30)).unwrap(), 30);
        for days in [0, 31, -3] {
            assert_eq!(
                code(&check_expiry(Some(days)).unwrap_err()),
                "invalid_expiry"
            );
        }
    }

    #[test]
    fn the_first_charge_is_eight_days_after_the_expiry_and_within_two_years() {
        let today = day(2026, 10, 24);
        let expires_on = day(2026, 11, 7);
        assert_eq!(
            check_first_charge(Some("2026-11-23"), today, expires_on).unwrap(),
            day(2026, 11, 23)
        );
        assert_eq!(
            check_first_charge(Some("2026-11-15"), today, expires_on).unwrap(),
            day(2026, 11, 15)
        );
        assert_eq!(
            check_first_charge(Some("2028-10-23"), today, day(2026, 10, 25)).unwrap(),
            day(2028, 10, 23)
        );
        for text in [
            None,
            Some(""),
            Some("11/23/2026"),
            Some("2026-10-24"),
            Some("2028-10-24"),
            Some("2026-11-07"),
            Some("2026-11-14"),
        ] {
            assert_eq!(
                code(&check_first_charge(text, today, expires_on).unwrap_err()),
                "invalid_first_charge",
                "{text:?}"
            );
        }
    }

    fn facts(legacy: Option<LegacySubscription>) -> MoveFacts {
        MoveFacts {
            company: CompanyPlan {
                name: "Acme Stone".into(),
                plan_source: "legacy".into(),
            },
            managers: vec![InviteManager {
                id: 1,
                name: "Mark".into(),
                email: "mark@acme.test".into(),
            }],
            legacy,
            owes: false,
            accepting: false,
            payment_ready: true,
            catalog: None,
            now: day(2026, 10, 24).and_hms_opt(12, 0, 0).unwrap(),
        }
    }

    fn legacy(status: &str, period_end_in_days: i64) -> LegacySubscription {
        LegacySubscription {
            id: "sub_legacy".into(),
            status: status.into(),
            plan_name: None,
            monthly_cents: None,
            current_period_end: day(2026, 10, 24).and_hms_opt(12, 0, 0).unwrap()
                + Duration::days(period_end_in_days),
            self_serve: false,
        }
    }

    fn blocked_code(facts: &MoveFacts) -> Option<&'static str> {
        facts.blocked().map(|(code, _)| code)
    }

    #[test]
    fn moves_are_blocked_for_payment_debt_and_unsafe_legacy_subscriptions() {
        // Nothing else blocks: only the missing price list.
        assert_eq!(blocked_code(&facts(None)), Some("catalog_unavailable"));
        assert_eq!(
            blocked_code(&facts(Some(legacy("active", 20)))),
            Some("catalog_unavailable")
        );
        let live = MoveFacts {
            payment_ready: false,
            ..facts(None)
        };
        assert_eq!(blocked_code(&live), Some("payment_unavailable"));
        let owing = MoveFacts {
            owes: true,
            ..facts(None)
        };
        assert_eq!(blocked_code(&owing), Some("legacy_overdue"));
        assert_eq!(
            blocked_code(&facts(Some(legacy("unpaid", 20)))),
            Some("legacy_overdue")
        );
        for status in ["paused", "incomplete"] {
            let (code, message) = facts(Some(legacy(status, 20))).blocked().unwrap();
            assert_eq!(code, "legacy_subscription_unsupported");
            assert_eq!(
                message,
                format!(
                    "The company's current Stripe subscription is {status}. Resolve it in Stripe before moving them to a plan."
                )
            );
        }
        // Renewing in under 9 days leaves no 8 days after a 1-day invite.
        let (code, message) = facts(Some(legacy("trialing", 8))).blocked().unwrap();
        assert_eq!(code, "legacy_period_too_close");
        assert_eq!(
            message,
            "The current plan renews on November 1, 2026, too soon to switch. Send the invite after it renews."
        );
        assert_eq!(
            blocked_code(&facts(Some(legacy("active", 9)))),
            Some("catalog_unavailable")
        );
    }

    #[test]
    fn debt_is_a_suspension_or_a_balance_due() {
        let mut record = CompanyBilling::empty(1);
        assert!(!owes(&record));
        record.outstanding_balance_cents = 1;
        assert!(owes(&record));
        record.outstanding_balance_cents = 0;
        record.payment_status = PaymentStatus::Suspended;
        assert!(owes(&record));
        record.payment_status = PaymentStatus::PastDue;
        assert!(!owes(&record));
    }

    #[test]
    fn a_picked_day_is_charged_at_nine_in_indianapolis() {
        // EST (UTC-5) in November, EDT (UTC-4) in July.
        assert_eq!(
            first_charge_instant(day(2026, 11, 23)).to_string(),
            "2026-11-23 14:00:00"
        );
        assert_eq!(
            first_charge_instant(day(2027, 7, 1)).to_string(),
            "2027-07-01 13:00:00"
        );
        assert_eq!(
            local_day(first_charge_instant(day(2026, 11, 23))),
            day(2026, 11, 23)
        );
    }

    #[test]
    fn tokens_are_32_random_bytes_and_only_their_hash_is_kept() {
        let token = new_token().unwrap();
        assert_eq!(token.len(), 43);
        assert!(plausible_token(&token));
        assert_ne!(token, new_token().unwrap());
        assert_eq!(token_hash(&token).len(), 64);
        assert_ne!(token_hash(&token), token);
        assert!(plausible_token("sb11-dev-invite-token-0001"));
        assert!(!plausible_token("short"));
        assert!(!plausible_token("has spaces in it, which tokens never do"));
        assert!(upgrade_url("abc").ends_with("/upgrade/abc"));
    }
}
