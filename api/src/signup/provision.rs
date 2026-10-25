//! Completion and provisioning (02-architecture.md section 10).
//!
//! `complete` accepts the consents (the SetupIntent succeeded, the hashes
//! match), freezes the facts and moves the signup to `provisioning`; `run`
//! then makes the company, owner, billing row and subscription, resuming
//! after whatever step a crash stopped it at: every Stripe POST carries an
//! idempotency key built from the signup id, every status change is guarded.

use super::consent::{self, ConsentAcceptance, PaymentFacts};
use super::intent::{self, CompanyDetails, Intent, SignupStatus};
use super::{RequestMeta, email_lock_name, funnel, notify, unavailable};
use crate::billing::lock::{CompanyLock, LockError, NamedLock};
use crate::billing::machine::apply_subscription_change;
use crate::billing::store::MySqlBillingStore;
use crate::billing::stripe::{NewSubscription, StripeError, stripe_id, subscription_snapshot};
use crate::billing::{self, items, seats};
use crate::company_create::{
    CompanyInit, OwnerInit, PlanSource, create_company_tx, create_owner_tx,
};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use axum::http::StatusCode;
use chrono::{Duration, NaiveDateTime, SubsecRound, Utc};
use lambda_http::tracing;
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
    /// Sign this user in: this request finished the signup. `None` when it
    /// was already completed before (sign in with the password instead).
    pub sign_in: Option<i32>,
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
    let outcome = |sign_in| CompletionOutcome {
        redirect_url: format!("{}/employee?welcome=1", state.app_url),
        sign_in,
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
            SignupStatus::Completed => return Ok(outcome(None)),
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
            SignupStatus::AwaitingBankVerification => {
                return Err(invalid_state("Your bank account is still being verified."));
            }
            _ => return Err(invalid_state("Add a payment method first.")),
        }
    }
    let signup_id = intent.id.as_str();
    match run(state, signup_id).await {
        Ok(RunOutcome::Completed { user_id, .. }) => Ok(outcome(Some(user_id))),
        Ok(RunOutcome::AlreadyCompleted) => Ok(outcome(None)),
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
            return Err(unavailable());
        }
    };
    if setup["status"].as_str() != Some("succeeded") {
        return Err(payment_not_ready());
    }
    let method = &setup["payment_method"];
    let payment_method_id = stripe_id(method).ok_or_else(payment_not_ready)?;
    let method_type = method["type"].as_str().map(str::to_string);
    let last4 = method_type
        .as_deref()
        .and_then(|kind| method[kind]["last4"].as_str())
        .map(str::to_string);
    let catalog = super::signup_catalog(&state.stripe).await?;
    let now = Utc::now().trunc_subsecs(0);
    let facts = consent::facts(&catalog, intent, now);
    let rendered = consent::render(&facts, &state.app_url);
    if !consent::all_accepted(&rendered, accepted) {
        return Err(ApiError::coded(
            StatusCode::CONFLICT,
            "consents_changed",
            "The terms changed. Read them again.",
        ));
    }
    let accepted_at = now.naive_utc();
    let price_ids = catalog.subscription_price_ids(&intent.plan.addons);
    let price_ids =
        serde_json::to_string(&price_ids).map_err(|err| ApiError::Internal(err.to_string()))?;
    let plan =
        serde_json::to_string(&intent.plan).map_err(|err| ApiError::Internal(err.to_string()))?;
    let mut tx = state.pool.begin().await?;
    let moved = sqlx::query(
        "UPDATE signup_intents
            SET status = 'provisioning', payment_method_id = ?, accepted_at = ?, trial_end_at = ?,
                fee_cents = ?, deadline_at = ?, price_ids = CAST(? AS JSON), last_error = NULL,
                updated_at = ?
          WHERE id = ? AND status = 'payment_ready' AND expires_at > ?
            AND plan = CAST(? AS JSON)",
    )
    .bind(&payment_method_id)
    .bind(accepted_at)
    .bind(facts.trial_end_at.trunc_subsecs(0))
    .bind(facts.quote.fee_cents)
    .bind(facts.deadline_at.trunc_subsecs(0))
    .bind(&price_ids)
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
    let payment = PaymentFacts {
        stripe_customer_id: intent.stripe_customer_id.clone(),
        payment_method_id: Some(payment_method_id),
        method_type,
        last4,
    };
    consent::insert_records(
        &mut tx,
        &intent.id,
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
    /// This run completed the signup (it sends the notifications).
    Completed { company_id: i32, user_id: i32 },
    /// Someone else completed it first.
    AlreadyCompleted,
    /// Refused for good (e.g. the email got taken): the signup expired.
    Refused,
}

/// Provisions a `provisioning` signup from wherever it stopped.
pub async fn run(state: &AppState, signup_id: &str) -> ApiResult<RunOutcome> {
    let pool = &state.pool;
    let intent = intent::load(pool, signup_id).await?.ok_or_else(not_found)?;
    match intent.status {
        SignupStatus::Completed => return Ok(RunOutcome::AlreadyCompleted),
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
    state
        .stripe
        .link_signup_customer(customer, signup_id, company_id, payment_method)
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
    notify::after_signup(state, &intent, company_id, user_id).await;
    let addons: Vec<&str> = intent.plan.addons.iter().map(|key| key.as_str()).collect();
    funnel::capture(
        state,
        "signup_completed",
        signup_id,
        json!({ "company_id": company_id, "users": intent.plan.users, "addons": addons }),
    )
    .await;
    funnel::alias(state, signup_id, user_id).await;
    Ok(RunOutcome::Completed {
        company_id,
        user_id,
    })
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
    let (Some(trial_end_at), Some(fee_cents), Some(deadline_at)) =
        (intent.trial_end_at, intent.fee_cents, intent.deadline_at)
    else {
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
    .bind(&intent.payment_method_id)
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
    create_onboarding_commitment(&mut tx, company_id, fee_cents, deadline_at).await?;
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
    let created = match adopt(state, intent, customer).await? {
        Some(existing) => existing,
        None => state
            .stripe
            .create_self_serve_subscription(&NewSubscription {
                signup_id: &intent.id,
                company_id,
                customer,
                payment_method,
                price_ids,
                trial_end: trial_end_at.and_utc().timestamp(),
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
            state
                .stripe
                .subscription(&stored)
                .await
                .map_err(|err| stripe_failure(&err))
        }
        _ => Ok(created),
    }
}

/// Stripe forgets idempotency keys after 24 hours: a late resume first looks
/// for the subscription an earlier run created (`metadata[signup_id]`).
async fn adopt(state: &AppState, intent: &Intent, customer: &str) -> ApiResult<Option<Value>> {
    let late = intent
        .accepted_at
        .is_none_or(|at| billing::now() - at >= Duration::hours(IDEMPOTENCY_SAFE_HOURS));
    if !late {
        return Ok(None);
    }
    let existing = state
        .stripe
        .list_subscriptions(customer)
        .await
        .map_err(|err| stripe_failure(&err))?;
    Ok(existing
        .into_iter()
        .find(|subscription| subscription["metadata"]["signup_id"].as_str() == Some(&intent.id)))
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

/// The company's onboarding commitment (`fee_cents` until `deadline_at`),
/// inside the provisioning transaction.
pub async fn create_onboarding_commitment(
    conn: &mut MySqlConnection,
    company_id: i32,
    fee_cents: i64,
    deadline_at: NaiveDateTime,
) -> sqlx::Result<()> {
    let now = billing::now();
    let outcome =
        crate::onboarding_call::commitment::new_commitment(company_id, now, deadline_at, fee_cents);
    crate::onboarding_call::store::insert(conn, &outcome, now).await?;
    Ok(())
}
