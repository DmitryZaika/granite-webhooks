//! `signup_intents`: the store of a signup in progress. Status changes are
//! guarded (`UPDATE ... WHERE id = ? AND status = ?`), so two requests can
//! never both move a signup on.

use crate::billing::catalog::PlanChoice;
use crate::openapi::ToSchema;
use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use sqlx::MySqlPool;

/// Signups expire this long after they start (unless provisioning).
pub const EXPIRES_HOURS: i64 = 24;
/// How long a bank account waiting for micro-deposits has to be verified
/// (Stripe's micro-deposit timeout): `bank_verification_until` is the
/// acceptance plus this.
pub const BANK_VERIFICATION_DAYS: i64 = 10;

/// Where a signup is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SignupStatus {
    /// Created; the email code was sent.
    Started,
    /// The email code was entered.
    Verified,
    /// A payment method setup exists (SetupIntent).
    PaymentReady,
    /// The company exists and the owner is signed in; the subscription
    /// waits for Stripe to verify the bank account (micro-deposits).
    AwaitingBankVerification,
    /// Consents accepted; the company and subscription are being made.
    Provisioning,
    /// The company exists and the owner can sign in.
    Completed,
    /// Abandoned (24 hours) or refused; start again.
    Expired,
}

impl SignupStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Verified => "verified",
            Self::PaymentReady => "payment_ready",
            Self::AwaitingBankVerification => "awaiting_bank_verification",
            Self::Provisioning => "provisioning",
            Self::Completed => "completed",
            Self::Expired => "expired",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        [
            Self::Started,
            Self::Verified,
            Self::PaymentReady,
            Self::AwaitingBankVerification,
            Self::Provisioning,
            Self::Completed,
            Self::Expired,
        ]
        .into_iter()
        .find(|status| status.as_str() == value)
    }
}

/// The company in step 2.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CompanyDetails {
    /// Company name.
    pub name: String,
    /// Phone, E.164 (`+13175550100`).
    pub phone: String,
    /// Website, or null.
    pub website: Option<String>,
    /// Street address.
    pub street: String,
    /// City.
    pub city: String,
    /// Two-letter US state code.
    pub state: String,
    /// ZIP code.
    pub zip: String,
}

/// A `signup_intents` row. No `Debug`: it holds the email and password hash.
#[derive(Clone)]
pub struct Intent {
    pub id: String,
    pub status: SignupStatus,
    /// Lowercase.
    pub email: String,
    pub code_hash: Option<String>,
    pub code_sent_at: Option<NaiveDateTime>,
    pub code_expires_at: Option<NaiveDateTime>,
    pub code_attempts: u8,
    pub codes_sent: u8,
    pub verified_at: Option<NaiveDateTime>,
    pub company: CompanyDetails,
    pub owner_name: String,
    /// bcrypt; blank once expired.
    pub password_hash: String,
    pub plan: PlanChoice,
    pub stripe_customer_id: Option<String>,
    pub setup_intent_id: Option<String>,
    pub setup_attempts: u8,
    pub payment_method_id: Option<String>,
    /// Frozen when the consents were accepted.
    pub accepted_at: Option<NaiveDateTime>,
    pub trial_end_at: Option<NaiveDateTime>,
    pub fee_cents: Option<i64>,
    pub deadline_at: Option<NaiveDateTime>,
    pub price_ids: Option<Vec<String>>,
    pub stripe_subscription_id: Option<String>,
    pub provision_attempts: u8,
    pub last_error: Option<String>,
    pub company_id: Option<i32>,
    pub user_id: Option<i32>,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
    pub expires_at: NaiveDateTime,
    pub completed_at: Option<NaiveDateTime>,
    /// Set when completion accepted a bank account still waiting for
    /// micro-deposits (acceptance + [`BANK_VERIFICATION_DAYS`]); stays set
    /// once verified.
    pub bank_verification_until: Option<NaiveDateTime>,
}

impl Intent {
    /// Past its time and not being provisioned or done. A signup waiting on
    /// its bank account never expires: its company exists.
    pub fn is_expired(&self, now: NaiveDateTime) -> bool {
        match self.status {
            SignupStatus::Expired => true,
            SignupStatus::Provisioning
            | SignupStatus::Completed
            | SignupStatus::AwaitingBankVerification => false,
            _ => now >= self.expires_at,
        }
    }
}

/// The columns of [`Intent`], JSON ones as text.
const COLUMNS: &str = "id, CAST(status AS CHAR) AS status, email, code_hash, code_sent_at,
    code_expires_at, code_attempts, codes_sent, verified_at, CAST(company AS CHAR) AS company,
    owner_name, password_hash, CAST(plan AS CHAR) AS plan, stripe_customer_id, setup_intent_id,
    setup_attempts, payment_method_id, accepted_at, trial_end_at, fee_cents, deadline_at,
    CAST(price_ids AS CHAR) AS price_ids, stripe_subscription_id, provision_attempts,
    last_error, company_id, user_id, ip, user_agent, created_at, updated_at, expires_at,
    completed_at, bank_verification_until";

#[derive(sqlx::FromRow)]
struct Row {
    id: String,
    status: String,
    email: String,
    code_hash: Option<String>,
    code_sent_at: Option<NaiveDateTime>,
    code_expires_at: Option<NaiveDateTime>,
    code_attempts: u8,
    codes_sent: u8,
    verified_at: Option<NaiveDateTime>,
    company: String,
    owner_name: String,
    password_hash: String,
    plan: String,
    stripe_customer_id: Option<String>,
    setup_intent_id: Option<String>,
    setup_attempts: u8,
    payment_method_id: Option<String>,
    accepted_at: Option<NaiveDateTime>,
    trial_end_at: Option<NaiveDateTime>,
    fee_cents: Option<i32>,
    deadline_at: Option<NaiveDateTime>,
    price_ids: Option<String>,
    stripe_subscription_id: Option<String>,
    provision_attempts: u8,
    last_error: Option<String>,
    company_id: Option<i32>,
    user_id: Option<i32>,
    ip: Option<String>,
    user_agent: Option<String>,
    created_at: NaiveDateTime,
    updated_at: NaiveDateTime,
    expires_at: NaiveDateTime,
    completed_at: Option<NaiveDateTime>,
    bank_verification_until: Option<NaiveDateTime>,
}

fn corrupt(column: &str) -> sqlx::Error {
    sqlx::Error::ColumnDecode {
        index: column.to_string(),
        source: format!("signup_intents.{column} is not valid").into(),
    }
}

impl TryFrom<Row> for Intent {
    type Error = sqlx::Error;

    fn try_from(row: Row) -> sqlx::Result<Self> {
        let status = SignupStatus::parse(&row.status).ok_or_else(|| corrupt("status"))?;
        let company = serde_json::from_str(&row.company).map_err(|_| corrupt("company"))?;
        let plan = serde_json::from_str(&row.plan).map_err(|_| corrupt("plan"))?;
        let price_ids = row
            .price_ids
            .map(|json| serde_json::from_str(&json).map_err(|_| corrupt("price_ids")))
            .transpose()?;
        Ok(Self {
            id: row.id,
            status,
            email: row.email,
            code_hash: row.code_hash,
            code_sent_at: row.code_sent_at,
            code_expires_at: row.code_expires_at,
            code_attempts: row.code_attempts,
            codes_sent: row.codes_sent,
            verified_at: row.verified_at,
            company,
            owner_name: row.owner_name,
            password_hash: row.password_hash,
            plan,
            stripe_customer_id: row.stripe_customer_id,
            setup_intent_id: row.setup_intent_id,
            setup_attempts: row.setup_attempts,
            payment_method_id: row.payment_method_id,
            accepted_at: row.accepted_at,
            trial_end_at: row.trial_end_at,
            fee_cents: row.fee_cents.map(i64::from),
            deadline_at: row.deadline_at,
            price_ids,
            stripe_subscription_id: row.stripe_subscription_id,
            provision_attempts: row.provision_attempts,
            last_error: row.last_error,
            company_id: row.company_id,
            user_id: row.user_id,
            ip: row.ip,
            user_agent: row.user_agent,
            created_at: row.created_at,
            updated_at: row.updated_at,
            expires_at: row.expires_at,
            completed_at: row.completed_at,
            bank_verification_until: row.bank_verification_until,
        })
    }
}

/// The signup by id; `None` when unknown (or not a UUID).
pub async fn load(pool: &MySqlPool, id: &str) -> sqlx::Result<Option<Intent>> {
    if uuid::Uuid::parse_str(id).is_err() {
        return Ok(None);
    }
    let row: Option<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM signup_intents WHERE id = ?"
    )))
    .bind(id.to_ascii_lowercase())
    .fetch_optional(pool)
    .await?;
    row.map(Intent::try_from).transpose()
}

/// Everything a new `started` signup stores.
pub struct NewIntent<'a> {
    pub id: &'a str,
    pub email: &'a str,
    pub code_hash: &'a str,
    pub company: &'a CompanyDetails,
    pub owner_name: &'a str,
    pub password_hash: &'a str,
    pub plan: &'a PlanChoice,
    pub ip: Option<&'a str>,
    pub user_agent: Option<&'a str>,
    pub now: NaiveDateTime,
}

/// How long an email code works.
pub const CODE_MINUTES: i64 = 15;

/// Inserts a `started` signup whose first code was sent just now.
pub async fn insert(pool: &MySqlPool, new: &NewIntent<'_>) -> sqlx::Result<()> {
    let company =
        serde_json::to_string(new.company).map_err(|err| sqlx::Error::Encode(err.into()))?;
    let plan = serde_json::to_string(new.plan).map_err(|err| sqlx::Error::Encode(err.into()))?;
    sqlx::query(
        "INSERT INTO signup_intents
           (id, status, email, code_hash, code_sent_at, code_expires_at, codes_sent,
            company, owner_name, password_hash, plan, ip, user_agent,
            created_at, updated_at, expires_at)
         VALUES (?, 'started', ?, ?, ?, ?, 1, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(new.id)
    .bind(new.email)
    .bind(new.code_hash)
    .bind(new.now)
    .bind(new.now + chrono::Duration::minutes(CODE_MINUTES))
    .bind(company)
    .bind(new.owner_name)
    .bind(new.password_hash)
    .bind(plan)
    .bind(new.ip)
    .bind(new.user_agent)
    .bind(new.now)
    .bind(new.now)
    .bind(new.now + chrono::Duration::hours(EXPIRES_HOURS))
    .execute(pool)
    .await?;
    Ok(())
}

/// `started`/`verified`/`payment_ready` to `expired`, dropping the password
/// hash and code. `false` when the signup was not in one of them.
pub async fn expire(pool: &MySqlPool, id: &str, now: NaiveDateTime) -> sqlx::Result<bool> {
    let done = sqlx::query(
        "UPDATE signup_intents SET status = 'expired', password_hash = '', code_hash = NULL,
                updated_at = ?
         WHERE id = ? AND status IN ('started', 'verified', 'payment_ready')",
    )
    .bind(now)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Replaces the plan while the signup is still before provisioning.
pub async fn set_plan(
    pool: &MySqlPool,
    id: &str,
    plan: &PlanChoice,
    now: NaiveDateTime,
) -> sqlx::Result<bool> {
    let json = serde_json::to_string(plan).map_err(|err| sqlx::Error::Encode(err.into()))?;
    let done = sqlx::query(
        "UPDATE signup_intents SET plan = ?, updated_at = ?
         WHERE id = ? AND status IN ('started', 'verified', 'payment_ready')",
    )
    .bind(json)
    .bind(now)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// `started` to `verified`. `false` when the signup was not `started`.
pub async fn mark_verified(pool: &MySqlPool, id: &str, now: NaiveDateTime) -> sqlx::Result<bool> {
    let done = sqlx::query(
        "UPDATE signup_intents SET status = 'verified', verified_at = ?, code_hash = NULL,
                updated_at = ?
         WHERE id = ? AND status = 'started'",
    )
    .bind(now)
    .bind(now)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}
