//! Completion and provisioning (02-architecture.md section 10).
//!
//! `complete` accepts the consents (the SetupIntent succeeded, or its bank
//! account waits for micro-deposits; the hashes match), freezes the facts and
//! moves the signup to `provisioning`; `run` then makes the company, owner,
//! billing row and subscription, resuming after whatever step a crash
//! stopped it at: every Stripe POST carries an idempotency key built from the
//! signup id, every status change is guarded.
//!
//! A bank account waiting for micro-deposits (`bank_verification_until`
//! set) stops `run` after the company: the signup goes
//! `awaiting_bank_verification`, the owner is signed in and welcomed, and
//! [`resume_verified`] makes the subscription once Stripe verifies the
//! account (or another method replaces it).

use super::consent::{self, ConsentAcceptance, ConsentOrigin};
use super::intent::{self, BANK_VERIFICATION_DAYS, CompanyDetails, Intent, SignupStatus};
use super::payment::{self, saved_method};
use super::{RequestMeta, email_lock_name, funnel, notify, payments_unavailable};
use crate::billing::lock::{CompanyLock, LockError, NamedLock};
use crate::billing::machine::apply_subscription_change;
use crate::billing::store::MySqlBillingStore;
use crate::billing::stripe::{
    NewSubscription, StripeError, SubscriptionOrigin, stripe_id, subscription_snapshot,
};
use crate::billing::{self, items, seats};
use crate::company_create::{
    CompanyInit, OwnerInit, PlanSource, create_company_tx, create_owner_tx,
};
use crate::error::{ApiError, ApiResult};
use crate::openapi::ToSchema;
use crate::state::AppState;
use axum::http::StatusCode;
use chrono::{DateTime, Duration, NaiveDateTime, SubsecRound, Utc};
use lambda_http::tracing;
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::{MySqlConnection, MySqlPool};

/// How long the email lock waits for another provisioning of the same email.
const EMAIL_LOCK_SECS: u32 = 10;
/// Stripe keeps idempotency keys 24 hours: a run resumed later than this
/// after the acceptance looks for the subscription before creating one.
const IDEMPOTENCY_SAFE_HOURS: i64 = 23;

/// What a completion request gets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionOutcome {
    /// Where the browser goes next (`<APP_URL>/employee?welcome=1`).
    pub redirect_url: String,
    /// Sign this user in: this request finished the signup (or made its
    /// company while the bank account verifies). `None` when that happened
    /// before (sign in with the password instead).
    pub sign_in: Option<i32>,
    /// The company exists but its bank account still waits for
    /// micro-deposits; `None` once the signup is completed.
    pub bank_verification: Option<BankVerification>,
}

/// A bank account the signup waits for (micro-deposits).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct BankVerification {
    /// Stripe's page where the code of the test deposit is entered; null
    /// when Stripe cannot be read right now.
    pub verify_url: Option<String>,
    /// Verify by then to keep access (ISO 8601, UTC).
    pub verify_by: DateTime<Utc>,
    /// The bank's name, or null.
    pub bank_name: Option<String>,
    /// The last 4 digits of the account number, or null.
    pub last4: Option<String>,
}

/// What a verified (or re-pending) payment method did to a signup waiting
/// on its bank.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeOutcome {
    /// The subscription exists and the signup is completed.
    Completed,
    /// Still waiting: the SetupIntent is not verified yet (a new one that is
    /// pending replaced the stored one), or it failed and changed nothing.
    StillVerifying,
    /// The signup does not wait on its bank (unknown, completed, other
    /// status): nothing to do.
    NotAwaiting,
}

fn not_found() -> ApiError {
    ApiError::coded(StatusCode::NOT_FOUND, "signup_expired", "Signup not found.")
}

fn expired() -> ApiError {
    ApiError::coded(
        StatusCode::GONE,
        "signup_expired",
        "This signup expired. Start again.",
    )
}

fn invalid_state(message: &str) -> ApiError {
    ApiError::coded(StatusCode::CONFLICT, "invalid_state", message)
}

fn setup_intent_mismatch() -> ApiError {
    ApiError::coded(
        StatusCode::CONFLICT,
        "setup_intent_mismatch",
        "This payment method belongs to another account.",
    )
}

fn payment_not_ready() -> ApiError {
    ApiError::coded(
        StatusCode::CONFLICT,
        "payment_not_ready",
        "Your payment method is not confirmed yet.",
    )
}

/// Stripe's error code only: the message can quote the buyer's details, and
/// this text is logged and stored in `last_error`.
fn stripe_failure(err: &StripeError) -> ApiError {
    ApiError::Upstream(format!("Stripe error {}", err.code()))
}

fn lock_failure(err: LockError) -> ApiError {
    match err {
        LockError::Busy => ApiError::Unavailable("Signup is busy right now.".into()),
        LockError::Database(err) => ApiError::Database(err),
    }
}

/// `POST /v1/public/signups/{id}/completion`.
pub async fn complete(
    state: &AppState,
    signup_id: &str,
    accepted: &[ConsentAcceptance],
    meta: &RequestMeta,
) -> ApiResult<CompletionOutcome> {
    let outcome = |sign_in, bank_verification| CompletionOutcome {
        redirect_url: format!("{}/employee?welcome=1", state.app_url),
        sign_in,
        bank_verification,
    };
    let mut intent = intent::load(&state.pool, signup_id)
        .await?
        .ok_or_else(not_found)?;
    // A lost race on the guarded update reloads once and handles the new status.
    let mut tries = 0;
    loop {
        if intent.is_expired(billing::now()) {
            return Err(expired());
        }
        match intent.status {
            SignupStatus::Completed => return Ok(outcome(None, None)),
            SignupStatus::AwaitingBankVerification => {
                return Ok(outcome(None, bank_verification(state, &intent).await));
            }
            SignupStatus::Provisioning => break,
            SignupStatus::PaymentReady if tries < 2 => {
                if accept(state, &intent, accepted, meta).await? {
                    break;
                }
                tries += 1;
                intent = intent::load(&state.pool, signup_id)
                    .await?
                    .ok_or_else(not_found)?;
            }
            SignupStatus::PaymentReady => return Err(invalid_state("Try again in a minute.")),
            _ => return Err(invalid_state("Add a payment method first.")),
        }
    }
    let signup_id = intent.id.as_str();
    let ran = run(state, signup_id).await;
    let awaiting = || async {
        match intent::load(&state.pool, signup_id).await {
            Ok(Some(current)) => bank_verification(state, &current).await,
            _ => None,
        }
    };
    match ran {
        Ok(RunOutcome::Completed {
            user_id,
            awaiting_bank,
            ..
        }) => {
            let bank = if awaiting_bank {
                awaiting().await
            } else {
                None
            };
            Ok(outcome(Some(user_id), bank))
        }
        Ok(RunOutcome::AlreadyCompleted) => Ok(outcome(None, awaiting().await)),
        Ok(RunOutcome::Refused) => Err(ApiError::coded(
            StatusCode::CONFLICT,
            "email_taken",
            "An account already uses this email.",
        )),
        Err(err) => {
            tracing::warn!(
                signup = %funnel::distinct_id(signup_id),
                error = %err,
                "signup provisioning failed"
            );
            if let Err(record) = record_failure(&state.pool, signup_id, &err).await {
                tracing::warn!(error = %record, "signup provisioning failure not recorded");
            }
            Err(ApiError::coded(
                StatusCode::SERVICE_UNAVAILABLE,
                "provisioning_retry",
                "We could not finish setting up your account. Try again in a minute.",
            ))
        }
    }
}

/// Checks the SetupIntent and the consents, freezes the facts and moves the
/// signup to `provisioning` with its consent records. False: another request
/// moved it first (or it expired meanwhile).
async fn accept(
    state: &AppState,
    intent: &Intent,
    accepted: &[ConsentAcceptance],
    meta: &RequestMeta,
) -> ApiResult<bool> {
    let Some(setup_intent_id) = intent.setup_intent_id.as_deref() else {
        return Err(invalid_state("Add a payment method first."));
    };
    let setup = match state.stripe.setup_intent(setup_intent_id).await {
        Ok(setup) => setup,
        Err(StripeError::Missing(_) | StripeError::Api(_)) => return Err(payment_not_ready()),
        Err(err) => {
            tracing::warn!(stripe_code = err.code(), "signup SetupIntent read failed");
            return Err(payments_unavailable());
        }
    };
    // D1: a bank account waiting for micro-deposits is accepted too; the
    // subscription waits for it.
    let verifying = payment::is_verifying(&setup);
    if setup["status"].as_str() != Some("succeeded") && !verifying {
        return Err(payment_not_ready());
    }
    let payment = payment::payment_facts(intent.stripe_customer_id.clone(), &setup)
        .ok_or_else(payment_not_ready)?;
    let (Some(payment_method_id), Some(method_type)) =
        (payment.payment_method_id.clone(), payment.method_type)
    else {
        return Err(payment_not_ready());
    };
    let catalog = super::signup_catalog(&state.stripe).await?;
    let now = Utc::now().trunc_subsecs(0);
    let facts = consent::facts(&catalog, intent, now);
    let rendered = consent::render(&facts, &state.app_url);
    if !consent::all_accepted(&rendered, accepted, method_type) {
        return Err(ApiError::coded(
            StatusCode::CONFLICT,
            "consents_changed",
            "The terms changed. Read them again.",
        ));
    }
    let accepted_at = now.naive_utc();
    // The date step 3 showed: the SetupIntent's creation + 10 days.
    let bank_verification_until = verifying.then(|| {
        payment::bank_verification_due(&setup).map_or_else(
            || accepted_at + Duration::days(BANK_VERIFICATION_DAYS),
            |due| due.naive_utc(),
        )
    });
    let price_ids = catalog.subscription_price_ids(&intent.plan.addons);
    let price_ids =
        serde_json::to_string(&price_ids).map_err(|err| ApiError::Internal(err.to_string()))?;
    let plan =
        serde_json::to_string(&intent.plan).map_err(|err| ApiError::Internal(err.to_string()))?;
    let mut tx = state.pool.begin().await?;
    let moved = sqlx::query(
        "UPDATE signup_intents
            SET status = 'provisioning', payment_method_id = ?, accepted_at = ?, trial_end_at = ?,
                advance_cents = ?, deadline_at = ?, price_ids = CAST(? AS JSON), last_error = NULL,
                bank_verification_until = ?, updated_at = ?
          WHERE id = ? AND status = 'payment_ready' AND expires_at > ?
            AND plan = CAST(? AS JSON)",
    )
    .bind(&payment_method_id)
    .bind(accepted_at)
    .bind(facts.trial_end_at.trunc_subsecs(0))
    .bind(facts.quote.advance_cents)
    .bind(facts.deadline_at.map(|at| at.trunc_subsecs(0)))
    .bind(&price_ids)
    .bind(bank_verification_until)
    .bind(accepted_at)
    .bind(&intent.id)
    // Fresh: the job deletes the customer of a signup past `expires_at`.
    .bind(Utc::now().naive_utc())
    // The plan the consents were rendered for: a change in between reloads,
    // and the new texts no longer match.
    .bind(&plan)
    .execute(&mut *tx)
    .await?;
    if moved.rows_affected() != 1 {
        tx.rollback().await?;
        return Ok(false);
    }
    consent::insert_records(
        &mut tx,
        ConsentOrigin::Signup {
            signup_id: &intent.id,
        },
        &rendered,
        &facts,
        &payment,
        meta,
        accepted_at,
    )
    .await?;
    tx.commit().await?;
    Ok(true)
}

/// Counts a failed provisioning run (`provision_attempts`, `last_error`) and
/// restarts the job's 10-minute wait (`updated_at`).
pub(super) async fn record_failure(
    pool: &MySqlPool,
    signup_id: &str,
    err: &ApiError,
) -> sqlx::Result<()> {
    let message: String = err.to_string().chars().take(255).collect();
    sqlx::query(
        "UPDATE signup_intents
            SET provision_attempts = LEAST(provision_attempts + 1, 255), last_error = ?, updated_at = ?
          WHERE id = ? AND status = 'provisioning'",
    )
    .bind(message)
    .bind(billing::now())
    .bind(signup_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// What a provisioning run reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    /// This run completed the signup, or made its company while the bank
    /// account verifies (`awaiting_bank`): sign the owner in.
    Completed {
        company_id: i32,
        user_id: i32,
        awaiting_bank: bool,
    },
    /// Someone else completed it (or moved it to wait for the bank) first.
    AlreadyCompleted,
    /// Refused for good (e.g. the email got taken): the signup expired.
    Refused,
}

/// The welcome already went out when the signup first waited on its bank:
/// that move drops the password hash (the owner exists by then).
const fn welcomed(intent: &Intent) -> bool {
    intent.bank_verification_until.is_some() && intent.password_hash.is_empty()
}

/// Provisions a `provisioning` signup from wherever it stopped.
pub async fn run(state: &AppState, signup_id: &str) -> ApiResult<RunOutcome> {
    let pool = &state.pool;
    let intent = intent::load(pool, signup_id).await?.ok_or_else(not_found)?;
    match intent.status {
        SignupStatus::Completed | SignupStatus::AwaitingBankVerification => {
            return Ok(RunOutcome::AlreadyCompleted);
        }
        SignupStatus::Provisioning => {}
        _ => return Err(invalid_state("This signup is not being set up.")),
    }
    let intent = if intent.company_id.is_some() && intent.user_id.is_some() {
        intent
    } else {
        match create_company(state, &intent).await? {
            Created::Ready(intent) => *intent,
            Created::Done(outcome) => return Ok(outcome),
        }
    };
    // The stored (lowercase) id: it builds the Stripe idempotency keys.
    let signup_id = intent.id.as_str();
    let (Some(company_id), Some(user_id)) = (intent.company_id, intent.user_id) else {
        return Err(ApiError::Internal("The signup has no company.".into()));
    };
    let (Some(customer), Some(payment_method)) = (
        intent.stripe_customer_id.as_deref(),
        intent.payment_method_id.as_deref(),
    ) else {
        return Err(ApiError::Internal(
            "The signup has no payment method.".into(),
        ));
    };
    if intent.bank_verification_until.is_some() && !bank_verified(state, &intent).await? {
        return await_bank(state, &intent, company_id, user_id, customer).await;
    }
    state
        .stripe
        .link_signup_customer(customer, signup_id, company_id, Some(payment_method))
        .await
        .map_err(|err| stripe_failure(&err))?;
    let subscription = subscription(state, &intent, company_id, customer, payment_method).await?;
    apply(pool, company_id, &subscription).await?;
    if let Err(err) = seats::sync_seats(state, company_id).await {
        tracing::warn!(company_id, error = %err.reason(), "signup seat sync failed");
    }
    let now = billing::now();
    let completed = sqlx::query(
        "UPDATE signup_intents SET status = 'completed', completed_at = ?, password_hash = '', updated_at = ?
          WHERE id = ? AND status = 'provisioning'",
    )
    .bind(now)
    .bind(now)
    .bind(signup_id)
    .execute(pool)
    .await?;
    if completed.rows_affected() != 1 {
        return Ok(RunOutcome::AlreadyCompleted);
    }
    if welcomed(&intent) {
        funnel::capture(
            state,
            "signup_bank_verified",
            signup_id,
            json!({ "company_id": company_id }),
        )
        .await;
    } else {
        welcome(state, &intent, company_id, user_id, false).await;
    }
    Ok(RunOutcome::Completed {
        company_id,
        user_id,
        awaiting_bank: false,
    })
}

/// The welcome, staff notice and `signup_completed`, once per signup.
async fn welcome(
    state: &AppState,
    intent: &Intent,
    company_id: i32,
    user_id: i32,
    bank_verification: bool,
) {
    notify::after_signup(state, intent, company_id, user_id).await;
    let addons: Vec<&str> = intent.plan.addons.iter().map(|key| key.as_str()).collect();
    funnel::capture(
        state,
        "signup_completed",
        &intent.id,
        json!({
            "company_id": company_id, "users": intent.plan.users, "addons": addons,
            "bank_verification": bank_verification,
        }),
    )
    .await;
    funnel::alias(state, &intent.id, user_id).await;
}

/// The stored SetupIntent has succeeded (a Stripe failure is an error: the
/// run is retried).
async fn bank_verified(state: &AppState, intent: &Intent) -> ApiResult<bool> {
    let Some(id) = intent.setup_intent_id.as_deref() else {
        return Err(ApiError::Internal("The signup has no SetupIntent.".into()));
    };
    let setup = state
        .stripe
        .setup_intent(id)
        .await
        .map_err(|err| stripe_failure(&err))?;
    Ok(setup["status"] == "succeeded")
}

/// The company exists but its bank account still waits for micro-deposits:
/// the customer is linked to the company without a default method, the
/// signup waits (`provisioning` to `awaiting_bank_verification`, dropping the
/// password hash) and the owner is welcomed once.
async fn await_bank(
    state: &AppState,
    intent: &Intent,
    company_id: i32,
    user_id: i32,
    customer: &str,
) -> ApiResult<RunOutcome> {
    state
        .stripe
        .link_signup_customer(customer, &intent.id, company_id, None)
        .await
        .map_err(|err| stripe_failure(&err))?;
    let moved = sqlx::query(
        "UPDATE signup_intents
            SET status = 'awaiting_bank_verification', password_hash = '', last_error = NULL,
                updated_at = ?
          WHERE id = ? AND status = 'provisioning'",
    )
    .bind(billing::now())
    .bind(&intent.id)
    .execute(&state.pool)
    .await?;
    if moved.rows_affected() != 1 {
        return Ok(RunOutcome::AlreadyCompleted);
    }
    if !welcomed(intent) {
        welcome(state, intent, company_id, user_id, true).await;
    }
    Ok(RunOutcome::Completed {
        company_id,
        user_id,
        awaiting_bank: true,
    })
}

/// What the done page shows for a signup waiting on its bank account: the
/// stored SetupIntent read live (verify link, bank, last 4; null fields when
/// Stripe cannot be read). `None` unless the signup waits.
async fn bank_verification(state: &AppState, intent: &Intent) -> Option<BankVerification> {
    if intent.status != SignupStatus::AwaitingBankVerification {
        return None;
    }
    let verify_by = intent.bank_verification_until?.and_utc();
    let saved = match intent.setup_intent_id.as_deref() {
        Some(id) => match state.stripe.setup_intent(id).await {
            Ok(setup) => saved_method(&setup),
            Err(err) => {
                tracing::warn!(stripe_code = err.code(), "signup SetupIntent read failed");
                None
            }
        },
        None => None,
    };
    Some(BankVerification {
        verify_url: saved.as_ref().and_then(|saved| saved.verify_url.clone()),
        verify_by,
        bank_name: saved.as_ref().and_then(|saved| saved.brand.clone()),
        last4: saved.and_then(|saved| saved.last4),
    })
}

/// The company's signup waiting on its bank account, if any.
pub async fn awaiting_signup(pool: &MySqlPool, company_id: i32) -> sqlx::Result<Option<Intent>> {
    let id: Option<String> = sqlx::query_scalar(
        "SELECT id FROM signup_intents
          WHERE company_id = ? AND status = 'awaiting_bank_verification'
          ORDER BY created_at DESC LIMIT 1",
    )
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    match id {
        Some(id) => intent::load(pool, &id).await,
        None => Ok(None),
    }
}

/// Resumes a signup waiting on its bank account with `setup_intent`.
///
/// `setup_intent` is as `Stripe::setup_intent` returns it (payment method
/// expanded), from the `setup_intent.succeeded` webhook or a method added on
/// the account page.
/// The SetupIntent must belong to the signup's customer (409
/// `setup_intent_mismatch` otherwise). A different one that succeeded or
/// waits for micro-deposits replaces the stored one (the old one is
/// cancelled, best effort); a failed one changes nothing. Once it
/// succeeded the signup goes `provisioning` and [`run`] makes the
/// subscription (no second welcome). A run that fails is left to the
/// maintenance job. Must be called WITHOUT the company lock held.
pub async fn resume_verified(
    state: &AppState,
    signup_id: &str,
    setup_intent: &Value,
) -> ApiResult<ResumeOutcome> {
    let Some(intent) = intent::load(&state.pool, signup_id).await? else {
        return Ok(ResumeOutcome::NotAwaiting);
    };
    let signup_id = intent.id.as_str();
    if crate::billing::cancellation::signup_cancelled(&state.pool, signup_id).await? {
        // Cancellation was requested while the bank account waited: no
        // subscription is ever made.
        return Ok(ResumeOutcome::NotAwaiting);
    }
    match intent.status {
        SignupStatus::AwaitingBankVerification => {}
        // A resume that stopped after the move: finish it.
        SignupStatus::Provisioning if intent.bank_verification_until.is_some() => {
            return resumed_run(state, signup_id).await;
        }
        _ => return Ok(ResumeOutcome::NotAwaiting),
    }
    let customer = stripe_id(&setup_intent["customer"]);
    let (Some(new_id), true) = (
        stripe_id(setup_intent),
        customer.is_some() && customer == intent.stripe_customer_id,
    ) else {
        tracing::warn!(
            signup = %funnel::distinct_id(signup_id),
            "a SetupIntent of another customer was offered to a signup"
        );
        return Err(setup_intent_mismatch());
    };
    let succeeded = setup_intent["status"] == "succeeded";
    if !succeeded && !payment::is_verifying(setup_intent) {
        return Ok(ResumeOutcome::StillVerifying);
    }
    let Some(payment_method) =
        payment::payment_facts(None, setup_intent).and_then(|facts| facts.payment_method_id)
    else {
        return Ok(ResumeOutcome::StillVerifying);
    };
    let old = intent.setup_intent_id.clone().filter(|old| *old != new_id);
    if !succeeded && old.is_none() {
        return Ok(ResumeOutcome::StillVerifying);
    }
    let next = if succeeded {
        "provisioning"
    } else {
        "awaiting_bank_verification"
    };
    let moved = sqlx::query(
        "UPDATE signup_intents
            SET status = ?, setup_intent_id = ?, payment_method_id = ?, provision_attempts = 0,
                last_error = NULL, updated_at = ?
          WHERE id = ? AND status = 'awaiting_bank_verification' AND setup_intent_id <=> ?",
    )
    .bind(next)
    .bind(&new_id)
    .bind(&payment_method)
    .bind(billing::now())
    .bind(signup_id)
    .bind(&intent.setup_intent_id)
    .execute(&state.pool)
    .await?;
    if moved.rows_affected() != 1 {
        // Another resume moved it first: handle what it left.
        let current = intent::load(&state.pool, signup_id).await?;
        return match current.map(|current| current.status) {
            Some(SignupStatus::Provisioning) => resumed_run(state, signup_id).await,
            Some(SignupStatus::AwaitingBankVerification) => Ok(ResumeOutcome::StillVerifying),
            _ => Ok(ResumeOutcome::NotAwaiting),
        };
    }
    if let Some(old) = old
        && let Err(err) = state
            .stripe
            .cancel_setup_intent(&old, &format!("signup:{signup_id}:si-cancel:{old}"))
            .await
    {
        tracing::warn!(
            signup = %funnel::distinct_id(signup_id),
            stripe_code = err.code(),
            "replaced signup SetupIntent not cancelled"
        );
    }
    if !succeeded {
        return Ok(ResumeOutcome::StillVerifying);
    }
    resumed_run(state, signup_id).await
}

/// [`run`] for a signup whose bank account (or replacement) was verified; a
/// failure is recorded for the maintenance job.
async fn resumed_run(state: &AppState, signup_id: &str) -> ApiResult<ResumeOutcome> {
    match run(state, signup_id).await {
        Ok(RunOutcome::Completed {
            awaiting_bank: false,
            ..
        }) => Ok(ResumeOutcome::Completed),
        Ok(RunOutcome::Completed { .. }) => Ok(ResumeOutcome::StillVerifying),
        Ok(RunOutcome::AlreadyCompleted | RunOutcome::Refused) => {
            let current = intent::load(&state.pool, signup_id).await?;
            Ok(match current.map(|current| current.status) {
                Some(SignupStatus::Completed) => ResumeOutcome::Completed,
                Some(SignupStatus::AwaitingBankVerification) => ResumeOutcome::StillVerifying,
                _ => ResumeOutcome::NotAwaiting,
            })
        }
        Err(err) => {
            tracing::warn!(
                signup = %funnel::distinct_id(signup_id),
                error = %err,
                "verified signup provisioning failed"
            );
            if let Err(record) = record_failure(&state.pool, signup_id, &err).await {
                tracing::warn!(error = %record, "signup provisioning failure not recorded");
            }
            Err(err)
        }
    }
}

enum Created {
    /// The company exists: the signup, reloaded.
    Ready(Box<Intent>),
    /// Nothing more to do.
    Done(RunOutcome),
}

/// The company, owner and billing row, once per signup: under the email's
/// lock, so neither a concurrent run nor a signup of the same email races it.
async fn create_company(state: &AppState, intent: &Intent) -> ApiResult<Created> {
    let lock = NamedLock::acquire(
        &state.pool,
        &email_lock_name(&intent.email),
        EMAIL_LOCK_SECS,
    )
    .await
    .map_err(lock_failure)?;
    let created = create_company_locked(state, &intent.id).await;
    lock.release().await;
    created
}

async fn create_company_locked(state: &AppState, signup_id: &str) -> ApiResult<Created> {
    let pool = &state.pool;
    let intent = intent::load(pool, signup_id).await?.ok_or_else(not_found)?;
    match intent.status {
        SignupStatus::Completed => return Ok(Created::Done(RunOutcome::AlreadyCompleted)),
        SignupStatus::Expired => return Ok(Created::Done(RunOutcome::Refused)),
        SignupStatus::Provisioning => {}
        _ => return Err(invalid_state("This signup is not being set up.")),
    }
    if intent.company_id.is_some() {
        return Ok(Created::Ready(Box::new(intent)));
    }
    if email_taken(pool, &intent.email).await? {
        refuse(state, &intent).await?;
        return Ok(Created::Done(RunOutcome::Refused));
    }
    let (Some(trial_end_at), Some(advance_cents), Some(accepted_at)) = (
        intent.trial_end_at,
        intent.advance_cents,
        intent.accepted_at,
    ) else {
        return Err(ApiError::Internal(
            "The signup's terms were not frozen.".into(),
        ));
    };
    let mut tx = pool.begin().await?;
    let company_id = create_company_tx(&mut tx, &company_init(&intent.company)).await?;
    let user_id = create_owner_tx(
        &mut tx,
        company_id,
        &OwnerInit {
            name: &intent.owner_name,
            email: &intent.email,
            password_hash: &intent.password_hash,
            phone: None,
        },
    )
    .await?;
    sqlx::query(
        "INSERT INTO company_billing
            (company_id, stripe_customer_id, stripe_payment_method_id, subscription_status,
             payment_status, trial_end_at)
         VALUES (?, ?, ?, 'incomplete', 'none', ?)",
    )
    .bind(company_id)
    .bind(&intent.stripe_customer_id)
    // A bank account still verifying is no default yet: the subscription
    // brings it once verified.
    .bind(
        intent
            .payment_method_id
            .as_deref()
            .filter(|_| intent.bank_verification_until.is_none()),
    )
    .bind(trial_end_at)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE consent_records SET company_id = ?, user_id = ? WHERE signup_intent_id = ?",
    )
    .bind(company_id)
    .bind(user_id)
    .bind(signup_id)
    .execute(&mut *tx)
    .await?;
    create_onboarding_commitment(&mut tx, company_id, advance_cents, accepted_at).await?;
    let linked = sqlx::query(
        "UPDATE signup_intents SET company_id = ?, user_id = ?, updated_at = ?
          WHERE id = ? AND status = 'provisioning' AND company_id IS NULL",
    )
    .bind(company_id)
    .bind(user_id)
    .bind(billing::now())
    .bind(signup_id)
    .execute(&mut *tx)
    .await?;
    if linked.rows_affected() != 1 {
        // A concurrent run won: keep its company.
        tx.rollback().await?;
        let winner = intent::load(pool, signup_id).await?.ok_or_else(not_found)?;
        return match winner.status {
            SignupStatus::Completed => Ok(Created::Done(RunOutcome::AlreadyCompleted)),
            SignupStatus::Provisioning if winner.company_id.is_some() => {
                Ok(Created::Ready(Box::new(winner)))
            }
            _ => Err(invalid_state("This signup is not being set up.")),
        };
    }
    tx.commit().await?;
    let intent = intent::load(pool, signup_id).await?.ok_or_else(not_found)?;
    Ok(Created::Ready(Box::new(intent)))
}

/// A self-serve company: no subdomain (it lives on the apex).
fn company_init(details: &CompanyDetails) -> CompanyInit {
    CompanyInit {
        name: details.name.clone(),
        subdomain: None,
        address: Some(format!(
            "{}, {}, {} {}",
            details.street, details.city, details.state, details.zip
        )),
        state_taxes: None,
        domain: None,
        cloudtalk_access_key: None,
        cloudtalk_access_secret: None,
        ringcentral_client_id: None,
        ringcentral_client_secret: None,
        ringcentral_jwt: None,
        ringcentral_server_url: None,
        plan_source: PlanSource::Subscription,
    }
}

/// Any user (deleted ones too) already has this email.
async fn email_taken(pool: &MySqlPool, email: &str) -> sqlx::Result<bool> {
    let taken: i64 =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE LOWER(TRIM(email)) = ?)")
            .bind(email.trim().to_lowercase())
            .fetch_one(pool)
            .await?;
    Ok(taken != 0)
}

/// The email got taken after the acceptance: the signup expires and its
/// Stripe customer goes (best effort).
async fn refuse(state: &AppState, intent: &Intent) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE signup_intents
            SET status = 'expired', last_error = 'email_taken', password_hash = '', updated_at = ?
          WHERE id = ? AND status = 'provisioning' AND company_id IS NULL",
    )
    .bind(billing::now())
    .bind(&intent.id)
    .execute(&state.pool)
    .await?;
    // A failed delete is retried by the maintenance job.
    if let Some(customer) = intent.stripe_customer_id.as_deref() {
        super::maintenance::delete_signup_customer(state, &intent.id, customer).await?;
    }
    tracing::info!(signup = %funnel::distinct_id(&intent.id), "signup refused: email taken");
    Ok(())
}

/// The signup's subscription: the stored one, or a new one under
/// `signup:<id>:subscription` with the frozen prices and trial end.
async fn subscription(
    state: &AppState,
    intent: &Intent,
    company_id: i32,
    customer: &str,
    payment_method: &str,
) -> ApiResult<Value> {
    if let Some(id) = intent.stripe_subscription_id.as_deref() {
        cancel_strays(state, intent, company_id, customer, id).await?;
        return state
            .stripe
            .subscription(id)
            .await
            .map_err(|err| stripe_failure(&err));
    }
    let (Some(price_ids), Some(trial_end_at)) = (
        intent.price_ids.as_deref().filter(|ids| !ids.is_empty()),
        intent.trial_end_at,
    ) else {
        return Err(ApiError::Internal(
            "The signup's terms were not frozen.".into(),
        ));
    };
    // Billed at once (the free period passed): sent under another key than
    // an earlier attempt's dated one, so first look for what that one made.
    let bills_now = trial_end_at.and_utc().timestamp() <= Utc::now().timestamp();
    let created = match adopt(state, intent, customer, bills_now).await? {
        Some(existing) => existing,
        None => state
            .stripe
            .create_self_serve_subscription(&NewSubscription {
                origin: SubscriptionOrigin::Signup(&intent.id),
                company_id,
                customer,
                payment_method,
                price_ids,
                trial_end: trial_end(trial_end_at, Utc::now()),
                automatic_tax: state.signup.automatic_tax,
            })
            .await
            .map_err(|err| stripe_failure(&err))?,
    };
    let Some(id) = created["id"].as_str().filter(|id| !id.is_empty()) else {
        return Err(ApiError::Upstream(
            "Stripe answered without a subscription id.".into(),
        ));
    };
    let stored = sqlx::query(
        "UPDATE signup_intents SET stripe_subscription_id = ?, updated_at = ?
          WHERE id = ? AND stripe_subscription_id IS NULL",
    )
    .bind(id)
    .bind(billing::now())
    .bind(&intent.id)
    .execute(&state.pool)
    .await?;
    if stored.rows_affected() == 1 {
        return Ok(created);
    }
    let current = intent::load(&state.pool, &intent.id)
        .await?
        .and_then(|intent| intent.stripe_subscription_id);
    match current {
        Some(stored) if stored != id => {
            tracing::error!(
                company_id,
                "signup has two subscriptions; keeping the one stored first"
            );
            cancel_stray(state, company_id, id).await?;
            state
                .stripe
                .subscription(&stored)
                .await
                .map_err(|err| stripe_failure(&err))
        }
        _ => Ok(created),
    }
}

/// Cancels a subscription of the signup that is not the stored one (two
/// runs raced), so only one ever bills; already gone is fine.
async fn cancel_stray(state: &AppState, company_id: i32, id: &str) -> ApiResult<()> {
    match state.stripe.cancel_subscription(id).await {
        Ok(_) | Err(StripeError::Missing(_)) => Ok(()),
        Err(err) => {
            tracing::error!(
                company_id,
                stripe_code = err.code(),
                "second signup subscription not cancelled"
            );
            Err(stripe_failure(&err))
        }
    }
}

/// The signup's subscriptions (`metadata[signup_id]`) other than the stored
/// one that have not ended, cancelled: a resume after a race finishes what
/// the racing run could not.
async fn cancel_strays(
    state: &AppState,
    intent: &Intent,
    company_id: i32,
    customer: &str,
    stored: &str,
) -> ApiResult<()> {
    let existing = state
        .stripe
        .list_subscriptions(customer)
        .await
        .map_err(|err| stripe_failure(&err))?;
    for subscription in existing {
        let Some(id) = subscription["id"].as_str() else {
            continue;
        };
        if id != stored
            && subscription["metadata"]["signup_id"].as_str() == Some(&intent.id)
            && !billing::is_ended_subscription_status(
                subscription["status"].as_str().unwrap_or_default(),
            )
        {
            cancel_stray(state, company_id, id).await?;
        }
    }
    Ok(())
}

/// D21: the frozen end of the free period, or `now` once it has passed (a
/// bank account verified after the first charge day: Stripe bills at once).
pub fn trial_end(frozen: NaiveDateTime, now: DateTime<Utc>) -> i64 {
    frozen.and_utc().timestamp().max(now.timestamp())
}

/// Stripe forgets idempotency keys after 24 hours: a late resume, or one
/// billing at once (`always`, another key), first looks for the subscription
/// an earlier run created (`metadata[signup_id]`).
async fn adopt(
    state: &AppState,
    intent: &Intent,
    customer: &str,
    always: bool,
) -> ApiResult<Option<Value>> {
    let late = intent
        .accepted_at
        .is_none_or(|at| billing::now() - at >= Duration::hours(IDEMPOTENCY_SAFE_HOURS));
    if !late && !always {
        return Ok(None);
    }
    let existing = state
        .stripe
        .list_subscriptions(customer)
        .await
        .map_err(|err| stripe_failure(&err))?;
    Ok(existing.into_iter().find(|subscription| {
        subscription["metadata"]["signup_id"].as_str() == Some(&intent.id)
            && !billing::is_ended_subscription_status(
                subscription["status"].as_str().unwrap_or_default(),
            )
    }))
}

/// The subscription into `company_billing` and `company_addons`, under the
/// company's billing lock.
async fn apply(pool: &MySqlPool, company_id: i32, subscription: &Value) -> ApiResult<()> {
    let snapshot = subscription_snapshot(subscription);
    let lock = CompanyLock::acquire(pool, company_id)
        .await
        .map_err(lock_failure)?;
    let now = billing::now();
    let applied = async {
        apply_subscription_change(&MySqlBillingStore(pool), &snapshot, now).await?;
        items::sync_items(pool, company_id, &snapshot, now).await
    }
    .await;
    lock.release().await;
    applied.map_err(ApiError::from)
}

/// The company's onboarding commitment, inside the provisioning
/// transaction.
///
/// Unless onboarding is done by the day the consent accepted at
/// `accepted_at` named (until its end in `SIGNUP_TZ`), a month is paid in
/// advance; `advance_cents` is the amount the consent stated.
pub async fn create_onboarding_commitment(
    conn: &mut MySqlConnection,
    company_id: i32,
    advance_cents: i64,
    accepted_at: NaiveDateTime,
) -> sqlx::Result<()> {
    use crate::onboarding_call::commitment::{new_commitment, signup_deadline_at};
    let now = billing::now();
    let outcome = new_commitment(
        company_id,
        now,
        signup_deadline_at(accepted_at),
        advance_cents,
    );
    crate::onboarding_call::store::insert(conn, &outcome, now).await?;
    Ok(())
}
