//! Accepting a plan invite, built like the signup's provisioning.
//!
//! `accept` checks the Manager, the consents and the payment method, freezes
//! the terms and moves the invite to `accepting` with its consent records;
//! `run` then makes the Stripe subscription and switches the company to it,
//! resuming after whatever step a crash stopped it at. Every Stripe POST
//! carries an idempotency key built from the invite id, every status change
//! is guarded, and `run` holds the company's billing lock throughout, so two
//! Managers accepting at once (or the maintenance job) never make two
//! subscriptions.

use super::emails::{self, Summary};
use super::{
    CompanyPlan, Invite, InviteStatus, MIN_DAYS_BEFORE_FIRST_CHARGE, has_ended,
    legacy_subscription, load, load_catalog, local_day, not_found, owes_for_move,
};
use crate::auth::session::{SessionUser, UserPositionRow};
use crate::billing::catalog::{Catalog, PlanChoice, Quote, QuoteLine, signup_day};
use crate::billing::lock::{CompanyLock, LockError};
use crate::billing::machine::apply_invite_subscription;
use crate::billing::store::{BillingStore, MySqlBillingStore};
use crate::billing::stripe::{
    InviteCustomer, NewSubscription, SetupPurpose, StripeError, SubscriptionOrigin, stripe_id,
    subscription_snapshot,
};
use crate::billing::{
    self, MANAGER_POSITION, is_ended_subscription_status, is_paying_subscription_status, items,
    seats,
};
use crate::entitlements::AddonKey;
use crate::error::{ApiError, ApiResult};
use crate::signup::consent::{
    self, ConsentAcceptance, ConsentContext, ConsentFacts, ConsentOrigin, PaymentFacts,
    RenderedConsent,
};
use crate::signup::{RequestMeta, notify, payment};
use crate::state::AppState;
use axum::http::StatusCode;
use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, Utc};
use lambda_http::tracing;
use serde::Serialize;
use serde_json::Value;
use utoipa::ToSchema;

/// Stripe keeps idempotency keys 24 hours: a run resumed later than this
/// after the acceptance looks for the subscription before creating one.
const IDEMPOTENCY_SAFE_HOURS: i64 = 23;

fn coded(status: StatusCode, code: &'static str, message: &str) -> ApiError {
    ApiError::coded(status, code, message)
}

fn revoked() -> ApiError {
    coded(
        StatusCode::GONE,
        "invite_revoked",
        "This invite was withdrawn. Ask Granite Manager for a new one.",
    )
}

fn expired() -> ApiError {
    coded(
        StatusCode::GONE,
        "invite_expired",
        "This invite expired. Ask Granite Manager for a new one.",
    )
}

/// 503 `acceptance_retry`.
pub fn retry() -> ApiError {
    coded(
        StatusCode::SERVICE_UNAVAILABLE,
        "acceptance_retry",
        "We could not finish switching your plan. Nothing was charged. Try again in a minute.",
    )
}

const SUBSCRIPTION_ENDED: &str = "invite_subscription_ended";

/// 409 `invite_subscription_ended`.
fn subscription_ended() -> ApiError {
    coded(
        StatusCode::CONFLICT,
        SUBSCRIPTION_ENDED,
        "The subscription for this plan was cancelled in Stripe, so the switch stopped. Ask Granite Manager to withdraw this invite and send a new one.",
    )
}

/// The run stopped because the invite's subscription ended in Stripe: the
/// invite is given up, so it is neither retried nor counted as a failure.
pub fn is_subscription_ended(err: &ApiError) -> bool {
    matches!(err, ApiError::Coded(coded) if coded.code == SUBSCRIPTION_ENDED)
}

fn payment_setup_unavailable() -> ApiError {
    coded(
        StatusCode::SERVICE_UNAVAILABLE,
        "payment_setup_unavailable",
        "Adding a payment method is not available yet. Try again later.",
    )
}

/// Stripe's error code only: the message can quote the Manager's details,
/// and this text is logged and stored in `last_error`.
fn stripe_failure(err: &StripeError) -> ApiError {
    ApiError::Upstream(format!("Stripe error {}", err.code()))
}

/// Who opens a link: the signed-in user and their positions in every company.
#[derive(Clone, Copy)]
pub struct Caller<'a> {
    pub user: &'a SessionUser,
    pub positions: &'a [UserPositionRow],
}

impl Caller<'_> {
    /// An employee, admin or superuser holding the Manager position in
    /// `company_id` (the invite's company, whatever company the session acts in).
    pub fn manages(&self, company_id: i32) -> bool {
        let manager = self
            .positions
            .iter()
            .any(|row| row.company_id == company_id && row.position_id == MANAGER_POSITION);
        manager && (self.user.is_employee || self.user.is_admin || self.user.is_superuser)
    }
}

/// The invite behind `token` and its company, for a Manager of it: 404
/// `invite_not_found`, 403 `not_invited`.
async fn invite_for(
    state: &AppState,
    token: &str,
    caller: Caller<'_>,
) -> ApiResult<(Invite, CompanyPlan)> {
    let invite = super::by_token(&state.pool, token)
        .await?
        .ok_or_else(not_found)?;
    let company = super::company(&state.pool, invite.company_id)
        .await?
        .ok_or_else(not_found)?;
    if !caller.manages(invite.company_id) {
        return Err(coded(
            StatusCode::FORBIDDEN,
            "not_invited",
            &format!("Only a Manager of {} can accept this plan.", company.name),
        ));
    }
    Ok((invite, company))
}

/// When the plan's first charge happens.
struct FirstCharge {
    at: NaiveDateTime,
    on: NaiveDate,
    /// The legacy plan that ends then.
    current_plan: Option<CurrentPlan>,
    /// The legacy subscription is past due or unpaid now.
    overdue: bool,
}

/// The legacy plan a Manager leaves.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CurrentPlan {
    /// Its name, or null.
    pub name: Option<String>,
    /// What it charges per month, in cents, or null when unknown.
    pub monthly_cents: Option<i64>,
    /// The day it ends (`YYYY-MM-DD`): the new plan's first charge.
    pub ends_on: NaiveDate,
}

/// The first charge of an open invite: its date, or for a legacy invite the
/// legacy subscription's live period end (the stored date once it no longer
/// runs). `strict`: a failed Stripe read is 503 `acceptance_retry` instead of
/// falling back.
async fn first_charge(state: &AppState, invite: &Invite, strict: bool) -> ApiResult<FirstCharge> {
    let stored = FirstCharge {
        at: invite.first_charge_at,
        on: invite.first_charge_on,
        current_plan: None,
        overdue: false,
    };
    let Some(legacy_id) = invite.legacy_subscription_id.as_deref() else {
        return Ok(stored);
    };
    let record = MySqlBillingStore(&state.pool)
        .by_company(invite.company_id)
        .await?;
    let plan = |name: Option<String>, monthly: Option<i64>, ends_on| CurrentPlan {
        name: name.or_else(|| record.as_ref().and_then(|r| r.plan_name.clone())),
        monthly_cents: monthly.or_else(|| record.as_ref().and_then(|r| r.monthly_price_cents)),
        ends_on,
    };
    let live = match state.stripe.subscription(legacy_id).await {
        Ok(subscription) => Some(subscription_snapshot(&subscription)),
        Err(StripeError::Missing(_)) => None,
        Err(err) if strict => {
            tracing::warn!(
                company_id = invite.company_id,
                stripe_code = err.code(),
                "legacy subscription read failed at acceptance"
            );
            return Err(retry());
        }
        Err(_) => None,
    };
    let Some(live) = live else {
        return Ok(FirstCharge {
            current_plan: Some(plan(None, None, stored.on)),
            ..stored
        });
    };
    let monthly = live.monthly_total_cents.or_else(|| {
        live.unit_amount_cents
            .zip(live.quantity)
            .map(|(unit, quantity)| unit * quantity)
    });
    let running = is_paying_subscription_status(&live.status) && live.replaced_by.is_none();
    let overdue = matches!(live.status.as_str(), "past_due" | "unpaid");
    match live.period_end.filter(|_| running) {
        Some(at) => Ok(FirstCharge {
            at,
            on: local_day(at),
            current_plan: Some(plan(live.plan_name, monthly, local_day(at))),
            overdue,
        }),
        None => Ok(FirstCharge {
            current_plan: Some(plan(live.plan_name, monthly, stored.on)),
            overdue,
            ..stored
        }),
    }
}

/// The facts the invite's consents state: the real billable users, the
/// add-ons and the first charge.
fn facts(
    catalog: &Catalog,
    company_name: &str,
    users: i64,
    addons: &[AddonKey],
    charge_at: NaiveDateTime,
    charge_on: NaiveDate,
) -> ConsentFacts {
    let mut quote = catalog.quote(
        &PlanChoice::normalized(users, addons),
        signup_day(Utc::now()),
    );
    quote.first_charge_on = charge_on;
    ConsentFacts {
        company_name: company_name.to_string(),
        quote,
        trial_end_at: charge_at,
        deadline_at: None,
        extra_user_cents: catalog.extra_user.unit_cents,
    }
}

/// The price of an invite's plan (before sales tax).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct InviteQuote {
    /// The company's active users now (what the first charge counts).
    pub users: i64,
    /// Users the base price covers (10).
    pub included_users: i64,
    /// Users above the included ones.
    pub extra_users: i64,
    /// Base, then extra users (when any), then each add-on.
    pub lines: Vec<QuoteLine>,
    /// Base + add-ons, without extra users, in cents.
    pub base_and_addons_cents: i64,
    /// The monthly total with the extra users, in cents.
    pub monthly_cents: i64,
    /// Always 0: nothing is charged before the first charge day.
    pub due_today_cents: i64,
    /// The first charge day (`YYYY-MM-DD`).
    pub first_charge_on: NaiveDate,
}

impl From<Quote> for InviteQuote {
    fn from(quote: Quote) -> Self {
        Self {
            users: quote.users,
            included_users: quote.included_users,
            extra_users: quote.extra_users,
            lines: quote.lines,
            base_and_addons_cents: quote.base_and_addons_cents,
            monthly_cents: quote.monthly_cents,
            due_today_cents: quote.due_today_cents,
            first_charge_on: quote.first_charge_on,
        }
    }
}

/// How the payment method is collected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum InvitePaymentMode {
    /// The api attaches a test card itself on acceptance (sandbox).
    Test,
    /// No way to add a payment method yet: acceptance answers 503
    /// `payment_setup_unavailable`.
    Unavailable,
}

/// `GET /v1/plan-invites/{token}`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct InviteOffer {
    /// `open` (accept it), `accepting` (being finished: accept again to
    /// resume) or `accepted` (done: go to `redirect_url`).
    pub status: InviteStatus,
    /// The invited company.
    pub company_name: String,
    /// The plan's add-ons, in a fixed order.
    pub addons: Vec<AddonKey>,
    pub quote: InviteQuote,
    /// When the first charge happens (ISO 8601, UTC).
    pub first_charge_at: DateTime<Utc>,
    /// The legacy plan that ends when this one starts, or null.
    pub current_plan: Option<CurrentPlan>,
    /// When the link stops working (ISO 8601, UTC).
    pub expires_at: DateTime<Utc>,
    /// The checkboxes to tick, in order (terms, auto_renewal,
    /// per_user_pricing); empty unless `status` is `open`.
    pub consents: Vec<RenderedConsent>,
    pub payment_mode: InvitePaymentMode,
    /// Where to go once accepted (the main app's billing page).
    pub redirect_url: String,
}

fn redirect_url(state: &AppState) -> String {
    format!("{}{}", state.app_url, billing::BILLING_PATH)
}

/// The invite as the upgrade page shows it.
pub async fn offer(state: &AppState, token: &str, caller: Caller<'_>) -> ApiResult<InviteOffer> {
    let (invite, company) = invite_for(state, token, caller).await?;
    let company_name = company.name;
    match invite.status {
        InviteStatus::Revoked => return Err(revoked()),
        InviteStatus::Open if invite.is_expired(billing::now()) => return Err(expired()),
        _ => {}
    }
    let catalog = load_catalog(&state.stripe)
        .await
        .ok_or_else(super::catalog_unavailable)?;
    let users = seats::billable_users(&state.pool, invite.company_id).await?;
    let charge = if invite.status == InviteStatus::Open {
        first_charge(state, &invite, false).await?
    } else {
        let at = invite.trial_end_at.unwrap_or(invite.first_charge_at);
        let legacy =
            invite.status == InviteStatus::Accepting && invite.legacy_subscription_id.is_some();
        let record = if legacy {
            MySqlBillingStore(&state.pool)
                .by_company(invite.company_id)
                .await?
        } else {
            None
        };
        FirstCharge {
            at,
            on: invite.first_charge_day(),
            current_plan: record.map(|record| CurrentPlan {
                name: record.plan_name,
                monthly_cents: record.monthly_price_cents,
                ends_on: invite.first_charge_day(),
            }),
            overdue: false,
        }
    };
    let facts = facts(
        &catalog,
        &company_name,
        users,
        &invite.addons,
        charge.at,
        charge.on,
    );
    let consents = if invite.status == InviteStatus::Open {
        consent::render_for(ConsentContext::Invite, &facts, &state.app_url)
    } else {
        Vec::new()
    };
    Ok(InviteOffer {
        status: invite.status,
        company_name,
        addons: invite.addons,
        quote: facts.quote.into(),
        first_charge_at: charge.at.and_utc(),
        current_plan: charge.current_plan,
        expires_at: invite.expires_at.and_utc(),
        consents,
        payment_mode: if payment::is_test_mode(state) {
            InvitePaymentMode::Test
        } else {
            InvitePaymentMode::Unavailable
        },
        redirect_url: redirect_url(state),
    })
}

/// What an acceptance request gets: where to go next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accepted {
    pub redirect_url: String,
}

/// `POST /v1/plan-invites/{token}/acceptance`.
pub async fn accept(
    state: &AppState,
    token: &str,
    caller: Caller<'_>,
    accepted: &[ConsentAcceptance],
    meta: &RequestMeta,
) -> ApiResult<Accepted> {
    let done = Accepted {
        redirect_url: redirect_url(state),
    };
    let (mut invite, company) = invite_for(state, token, caller).await?;
    // A lost race reloads and handles the new status.
    let mut tries = 0;
    loop {
        match invite.status {
            InviteStatus::Revoked => return Err(revoked()),
            InviteStatus::Accepted => return Ok(done),
            InviteStatus::Accepting => break,
            InviteStatus::Open if invite.is_expired(billing::now()) => return Err(expired()),
            InviteStatus::Open if tries < 2 => {
                tries += 1;
                let begun = begin(state, &invite, &company, caller, accepted, meta).await;
                let current = load(&state.pool, &invite.id).await?.ok_or_else(not_found)?;
                match begun {
                    Ok(true) => {
                        invite = current;
                        break;
                    }
                    Err(err) if current.status == InviteStatus::Open => return Err(err),
                    // A lost race, or another Manager's acceptance got there first.
                    Ok(false) | Err(_) => invite = current,
                }
            }
            InviteStatus::Open => return Err(retry()),
        }
    }
    match run(state, &invite.id).await {
        Ok(RunOutcome::Accepted { .. } | RunOutcome::AlreadyAccepted) => Ok(done),
        Ok(RunOutcome::Busy) => Err(retry()),
        Err(err) if is_subscription_ended(&err) => Err(err),
        Err(err) => {
            tracing::warn!(
                company_id = invite.company_id,
                error = %err,
                "plan invite acceptance failed"
            );
            if let Err(record) = record_failure(&state.pool, &invite.id, &err).await {
                tracing::warn!(error = %record, "plan invite failure not recorded");
            }
            Err(retry())
        }
    }
}

/// Checks everything, freezes the terms and moves the invite to `accepting`
/// with its consent records. False: another request moved it first (or it
/// expired meanwhile). On failure the invite stays open (a Stripe customer
/// made on the way is kept on it for the retry).
#[allow(clippy::too_many_lines)]
async fn begin(
    state: &AppState,
    invite: &Invite,
    company: &CompanyPlan,
    caller: Caller<'_>,
    accepted: &[ConsentAcceptance],
    meta: &RequestMeta,
) -> ApiResult<bool> {
    if !payment::is_test_mode(state) {
        return Err(payment_setup_unavailable());
    }
    let catalog = load_catalog(&state.stripe).await.ok_or_else(retry)?;
    let charge = first_charge(state, invite, true).await?;
    let record = MySqlBillingStore(&state.pool)
        .by_company(invite.company_id)
        .await?;
    // A subscription that started since the invite was sent would be billed
    // alongside the new plan.
    if legacy_subscription(state, record.as_ref())
        .await
        .is_some_and(|live| {
            live.self_serve || invite.legacy_subscription_id.as_deref() != Some(live.id.as_str())
        })
    {
        return Err(coded(
            StatusCode::CONFLICT,
            "invite_needs_new_date",
            "The company's billing changed since this invite was sent. Ask Granite Manager for a new invite.",
        ));
    }
    // A self-serve company here has no live subscription (checked above).
    let ended = company.plan_source == "subscription";
    if charge.overdue
        || record
            .as_ref()
            .is_some_and(|record| owes_for_move(record, ended))
    {
        return Err(coded(
            StatusCode::CONFLICT,
            "legacy_overdue",
            "This company has an overdue payment. Ask Granite Manager to settle it first.",
        ));
    }
    let now = billing::now();
    if !far_enough(charge.at, now) {
        return Err(coded(
            StatusCode::CONFLICT,
            "invite_needs_new_date",
            &format!(
                "Your first charge would be less than {MIN_DAYS_BEFORE_FIRST_CHARGE} days away. Ask Granite Manager for a new invite."
            ),
        ));
    }
    let users = seats::billable_users(&state.pool, invite.company_id).await?;
    let facts = facts(
        &catalog,
        &company.name,
        users,
        &invite.addons,
        charge.at,
        charge.on,
    );
    let rendered = consent::render_for(ConsentContext::Invite, &facts, &state.app_url);
    if !consent::all_accepted(&rendered, accepted) {
        return Err(coded(
            StatusCode::CONFLICT,
            "consents_changed",
            "The terms changed. Read them again.",
        ));
    }
    let known = record.and_then(|record| record.stripe_customer_id);
    let customer = customer_id(state, invite, &company.name, known).await?;
    let payment = payment_method(state, invite, &customer).await?;
    let price_ids = serde_json::to_string(&catalog.subscription_price_ids(&invite.addons))
        .map_err(|err| ApiError::Internal(err.to_string()))?;
    let mut tx = state.pool.begin().await?;
    let moved = sqlx::query(
        "UPDATE plan_invites
            SET status = 'accepting', accepted_by = ?, accepting_at = ?, trial_end_at = ?,
                price_ids = CAST(? AS JSON), stripe_customer_id = ?, payment_method_id = ?,
                last_error = NULL, updated_at = ?
          WHERE id = ? AND status = 'open' AND expires_at > ?",
    )
    .bind(caller.user.id)
    .bind(now)
    .bind(charge.at)
    .bind(&price_ids)
    .bind(&customer)
    .bind(&payment.payment_method_id)
    .bind(now)
    .bind(&invite.id)
    .bind(billing::now())
    .execute(&mut *tx)
    .await?;
    if moved.rows_affected() != 1 {
        tx.rollback().await?;
        return Ok(false);
    }
    consent::insert_records(
        &mut tx,
        ConsentOrigin::Invite {
            invite_id: &invite.id,
            company_id: invite.company_id,
            user_id: caller.user.id,
            invited_by: invite.created_by,
            legacy_subscription_replaced: invite.legacy_subscription_id.is_some(),
        },
        &rendered,
        &facts,
        &payment,
        meta,
        now,
    )
    .await?;
    tx.commit().await?;
    Ok(true)
}

/// The first charge falls at least [`MIN_DAYS_BEFORE_FIRST_CHARGE`] calendar
/// days ([`SIGNUP_TZ`](crate::billing::catalog::SIGNUP_TZ)) after `now`, so
/// the reminder 7 days before it can still be sent.
fn far_enough(charge_at: NaiveDateTime, now: NaiveDateTime) -> bool {
    local_day(charge_at) >= local_day(now) + Duration::days(MIN_DAYS_BEFORE_FIRST_CHARGE)
}

/// The company's Stripe customer: the one billing knows, else the invite's,
/// else a new one under `plan-invite:<id>:customer` (stored on the invite at
/// once). Its email is the company's first Manager's, whoever accepts, so
/// another Manager's retry sends the same request under the same key.
async fn customer_id(
    state: &AppState,
    invite: &Invite,
    company_name: &str,
    known: Option<String>,
) -> ApiResult<String> {
    if let Some(id) = known.or_else(|| invite.stripe_customer_id.clone()) {
        return Ok(id);
    }
    let first_manager = super::managers(&state.pool, invite.company_id)
        .await?
        .into_iter()
        .next();
    let created = state
        .stripe
        .create_invite_customer(&InviteCustomer {
            invite_id: &invite.id,
            company_id: invite.company_id,
            company_name,
            email: first_manager.as_ref().map(|manager| manager.email.as_str()),
        })
        .await
        .map_err(|err| {
            tracing::warn!(
                company_id = invite.company_id,
                stripe_code = err.code(),
                "plan invite customer not created"
            );
            retry()
        })?;
    let id = stripe_id(&created["id"]).ok_or_else(retry)?;
    sqlx::query(
        "UPDATE plan_invites SET stripe_customer_id = ?, updated_at = ?
          WHERE id = ? AND stripe_customer_id IS NULL",
    )
    .bind(&id)
    .bind(billing::now())
    .bind(&invite.id)
    .execute(&state.pool)
    .await?;
    Ok(id)
}

/// The payment method the plan is charged to.
///
/// Until step 13 only the test seam exists: a test card confirmed
/// server-side (`plan-invite:<id>:si`). Step 13 plugs the SetupIntent the
/// Payment Element confirms in here.
async fn payment_method(
    state: &AppState,
    invite: &Invite,
    customer: &str,
) -> ApiResult<PaymentFacts> {
    let Some(test_method) = state
        .signup
        .test_payment_method
        .as_deref()
        .filter(|_| payment::is_test_mode(state))
    else {
        return Err(payment_setup_unavailable());
    };
    let failed = |err: &StripeError| {
        tracing::warn!(
            company_id = invite.company_id,
            stripe_code = err.code(),
            "plan invite payment method not set up"
        );
        retry()
    };
    let setup = state
        .stripe
        .create_confirmed_test_setup_intent(
            customer,
            SetupPurpose::PlanInvite {
                invite_id: &invite.id,
            },
            test_method,
        )
        .await
        .map_err(|err| failed(&err))?;
    if setup["status"].as_str() != Some("succeeded") {
        return Err(retry());
    }
    let method_id = stripe_id(&setup["payment_method"]).ok_or_else(retry)?;
    // The card's type and last 4 for the records; best effort.
    let expanded = match setup["id"].as_str() {
        Some(id) => state.stripe.setup_intent(id).await.ok(),
        None => None,
    };
    let method = expanded.as_ref().map(|setup| &setup["payment_method"]);
    let method_type = method
        .and_then(|method| method["type"].as_str())
        .map(str::to_string);
    let last4 = method
        .zip(method_type.as_deref())
        .and_then(|(method, kind)| method[kind]["last4"].as_str())
        .map(str::to_string);
    Ok(PaymentFacts {
        stripe_customer_id: Some(customer.to_string()),
        payment_method_id: Some(method_id),
        method_type,
        last4,
    })
}

/// Counts a failed run (`provision_attempts`, `last_error`: codes only) and
/// restarts the job's 10-minute wait (`updated_at`).
pub async fn record_failure(
    pool: &sqlx::MySqlPool,
    invite_id: &str,
    err: &ApiError,
) -> sqlx::Result<()> {
    let message: String = err.to_string().chars().take(255).collect();
    sqlx::query(
        "UPDATE plan_invites
            SET provision_attempts = LEAST(provision_attempts + 1, 255), last_error = ?, updated_at = ?
          WHERE id = ? AND status = 'accepting'",
    )
    .bind(message)
    .bind(billing::now())
    .bind(invite_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// What a run reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    /// This run switched the company (it sent the notices).
    Accepted { company_id: i32, user_id: i32 },
    /// Someone else finished it first.
    AlreadyAccepted,
    /// Another run holds the company's billing lock: try later.
    Busy,
}

/// Finishes an `accepting` invite from wherever it stopped, holding the
/// company's billing lock throughout.
pub async fn run(state: &AppState, invite_id: &str) -> ApiResult<RunOutcome> {
    let invite = load(&state.pool, invite_id).await?.ok_or_else(not_found)?;
    let lock = match CompanyLock::acquire(&state.pool, invite.company_id).await {
        Ok(lock) => lock,
        Err(LockError::Busy) => return Ok(RunOutcome::Busy),
        Err(LockError::Database(err)) => return Err(err.into()),
    };
    let outcome = run_locked(state, &invite.id).await;
    lock.release().await;
    let outcome = outcome?;
    if let RunOutcome::Accepted { company_id, .. } = outcome {
        // The seat push takes the lock itself.
        if let Err(err) = seats::sync_seats(state, company_id).await {
            tracing::warn!(company_id, error = %err.reason(), "plan move seat sync failed");
        }
        after_acceptance(state, &invite.id).await;
    }
    Ok(outcome)
}

async fn run_locked(state: &AppState, invite_id: &str) -> ApiResult<RunOutcome> {
    let pool = &state.pool;
    let invite = load(pool, invite_id).await?.ok_or_else(not_found)?;
    match invite.status {
        InviteStatus::Accepted => return Ok(RunOutcome::AlreadyAccepted),
        InviteStatus::Accepting => {}
        _ => {
            return Err(ApiError::Internal(
                "This invite is not being accepted.".into(),
            ));
        }
    }
    let (Some(customer), Some(payment_method), Some(user_id)) = (
        invite.stripe_customer_id.as_deref(),
        invite.payment_method_id.as_deref(),
        invite.accepted_by,
    ) else {
        return Err(ApiError::Internal(
            "The invite's terms were not frozen.".into(),
        ));
    };
    let company_id = invite.company_id;
    state
        .stripe
        .set_invite_default_payment_method(customer, payment_method, &invite.id)
        .await
        .map_err(|err| stripe_failure(&err))?;
    let subscription = subscription(state, &invite, customer, payment_method).await?;
    let snapshot = subscription_snapshot(&subscription);
    if has_ended(&snapshot) {
        return Err(stop_ended(pool, &invite).await?);
    }
    if let Some(legacy_id) = invite.legacy_subscription_id.as_deref() {
        replace_legacy(state, &invite, legacy_id, &snapshot.id).await?;
    }
    let now = billing::now();
    apply_invite_subscription(&MySqlBillingStore(pool), &snapshot, now).await?;
    let mut tx = pool.begin().await?;
    items::sync_items_in(&mut tx, company_id, &snapshot, now).await?;
    // As the superuser switch: a newly required company starts its window now.
    // Billed per user like a self-serve company, so no user limit.
    sqlx::query(
        "UPDATE company
            SET billing_required_at = IF(billing_required = 0 OR billing_required_at IS NULL, ?,
                                         billing_required_at),
                billing_required = 1, plan_source = 'subscription', max_users = NULL
          WHERE id = ?",
    )
    .bind(now)
    .bind(company_id)
    .execute(&mut *tx)
    .await?;
    let accepted = sqlx::query(
        "UPDATE plan_invites SET status = 'accepted', accepted_at = ?, last_error = NULL, updated_at = ?
          WHERE id = ? AND status = 'accepting'",
    )
    .bind(now)
    .bind(now)
    .bind(&invite.id)
    .execute(&mut *tx)
    .await?;
    if accepted.rows_affected() != 1 {
        tx.rollback().await?;
        return Ok(RunOutcome::AlreadyAccepted);
    }
    tx.commit().await?;
    Ok(RunOutcome::Accepted {
        company_id,
        user_id,
    })
}

/// The invite's subscription ended in Stripe (cancelled by hand): nothing
/// is switched, and the invite is given up for staff to withdraw.
async fn stop_ended(pool: &sqlx::MySqlPool, invite: &Invite) -> ApiResult<ApiError> {
    let now = billing::now();
    sqlx::query(
        "UPDATE plan_invites
            SET gave_up_at = COALESCE(gave_up_at, ?), last_error = ?, updated_at = ?
          WHERE id = ? AND status = 'accepting'",
    )
    .bind(now)
    .bind(SUBSCRIPTION_ENDED)
    .bind(now)
    .bind(&invite.id)
    .execute(pool)
    .await?;
    tracing::warn!(
        company_id = invite.company_id,
        "plan invite subscription ended in Stripe; acceptance given up"
    );
    Ok(subscription_ended())
}

/// The invite's subscription: the stored one, or a new one under
/// `plan-invite:<id>:subscription` with the frozen prices and trial end.
async fn subscription(
    state: &AppState,
    invite: &Invite,
    customer: &str,
    payment_method: &str,
) -> ApiResult<Value> {
    if let Some(id) = invite.stripe_subscription_id.as_deref() {
        return state
            .stripe
            .subscription(id)
            .await
            .map_err(|err| stripe_failure(&err));
    }
    let (Some(price_ids), Some(trial_end_at)) = (
        invite.price_ids.as_deref().filter(|ids| !ids.is_empty()),
        invite.trial_end_at,
    ) else {
        return Err(ApiError::Internal(
            "The invite's terms were not frozen.".into(),
        ));
    };
    let created = match adopt(state, invite, customer).await? {
        Some(existing) => existing,
        None => state
            .stripe
            .create_self_serve_subscription(&NewSubscription {
                origin: SubscriptionOrigin::PlanInvite(&invite.id),
                company_id: invite.company_id,
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
    sqlx::query(
        "UPDATE plan_invites SET stripe_subscription_id = ?, updated_at = ?
          WHERE id = ? AND stripe_subscription_id IS NULL",
    )
    .bind(id)
    .bind(billing::now())
    .bind(&invite.id)
    .execute(&state.pool)
    .await?;
    Ok(created)
}

/// Stripe forgets idempotency keys after 24 hours: a late resume first looks
/// for the subscription an earlier run created (`metadata[plan_invite_id]`)
/// that has not ended.
async fn adopt(state: &AppState, invite: &Invite, customer: &str) -> ApiResult<Option<Value>> {
    let late = invite
        .accepting_at
        .is_none_or(|at| billing::now() - at >= Duration::hours(IDEMPOTENCY_SAFE_HOURS));
    if !late {
        return Ok(None);
    }
    let existing = state
        .stripe
        .list_subscriptions(customer)
        .await
        .map_err(|err| stripe_failure(&err))?;
    Ok(existing.into_iter().find(|subscription| {
        subscription["metadata"]["plan_invite_id"].as_str() == Some(invite.id.as_str())
            && !is_ended_subscription_status(subscription["status"].as_str().unwrap_or_default())
    }))
}

/// The legacy subscription ends at its period end, marked as replaced by
/// the new one (already done, ended or gone: nothing to do).
async fn replace_legacy(
    state: &AppState,
    invite: &Invite,
    legacy_id: &str,
    new_id: &str,
) -> ApiResult<()> {
    let legacy = match state.stripe.subscription(legacy_id).await {
        Ok(legacy) => legacy,
        Err(StripeError::Missing(_)) => return Ok(()),
        Err(err) => return Err(stripe_failure(&err)),
    };
    let ended = is_ended_subscription_status(legacy["status"].as_str().unwrap_or_default());
    let replaced = legacy["metadata"]["replaced_by"].as_str() == Some(new_id)
        && legacy["cancel_at_period_end"] == true;
    if ended || replaced {
        return Ok(());
    }
    state
        .stripe
        .replace_legacy_subscription(legacy_id, new_id, &invite.id)
        .await
        .map_err(|err| stripe_failure(&err))?;
    Ok(())
}

/// The "plan active" email to the accepting Manager and the staff notice.
/// Best effort: failures are logged with the company id only.
async fn after_acceptance(state: &AppState, invite_id: &str) {
    let loaded = async {
        let invite = load(&state.pool, invite_id).await?;
        let Some(invite) = invite else {
            return Ok::<_, sqlx::Error>(None);
        };
        let company = super::company(&state.pool, invite.company_id).await?;
        let manager: Option<(String, String)> =
            match invite.accepted_by {
                Some(user_id) => sqlx::query_as(
                    "SELECT COALESCE(name, ''), TRIM(COALESCE(email, '')) FROM users WHERE id = ?",
                )
                .bind(user_id)
                .fetch_optional(&state.pool)
                .await?,
                None => None,
            };
        let users = seats::billable_users(&state.pool, invite.company_id).await?;
        Ok(Some((invite, company, manager, users)))
    }
    .await;
    let (invite, company, manager, users) = match loaded {
        Ok(Some(loaded)) => loaded,
        Ok(None) => return,
        Err(err) => {
            tracing::warn!(error = %err, "plan move notices not sent");
            return;
        }
    };
    let company_name = company.map(|company| company.name).unwrap_or_default();
    let first_charge_on = invite.first_charge_day();
    if let (Some((name, email)), Some(catalog)) = (manager, load_catalog(&state.stripe).await)
        && !email.is_empty()
    {
        let summary = Summary::of(&catalog, users, &invite.addons, first_charge_on);
        let body = emails::accepted_html(&name, &company_name, &summary);
        if let Err(kind) = emails::send(state, &email, emails::ACCEPTED_SUBJECT, body).await {
            tracing::warn!(
                company_id = invite.company_id,
                kind,
                "plan active email failed"
            );
        }
    }
    notify::after_plan_accepted(
        state,
        &company_name,
        invite.company_id,
        &invite.addons,
        first_charge_on,
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entitlements::Entitlements;

    fn user(employee: bool, admin: bool, superuser: bool) -> SessionUser {
        SessionUser {
            id: 5,
            email: "jane@acme.test".into(),
            name: Some("Jane".into()),
            phone_number: None,
            is_employee: employee,
            is_admin: admin,
            is_superuser: superuser,
            company_id: 1,
            pined_bar: false,
            cloudtalk_agent_id: None,
            cloudtalk_phone_number: None,
            ringcentral_extension_id: None,
            ringcentral_phone_number: None,
            is_signature_available: false,
            plan: Entitlements::default(),
        }
    }

    #[test]
    fn the_first_charge_is_eight_calendar_days_after_acceptance() {
        let at = |text: &str| NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M").unwrap();
        // 23:00 on Oct 24 in Indianapolis (EDT); first charges at 09:00 EST.
        let now = at("2026-10-25 03:00");
        assert!(far_enough(at("2026-11-01 14:00"), now));
        assert!(!far_enough(at("2026-10-31 13:00"), now));
        assert!(!far_enough(at("2026-10-25 03:30"), now));
    }

    #[test]
    fn only_a_manager_of_the_invited_company_accepts() {
        let manager_of_2 = [UserPositionRow {
            position_id: MANAGER_POSITION,
            company_id: 2,
        }];
        let rep_of_2 = [UserPositionRow {
            position_id: 1,
            company_id: 2,
        }];
        let manages = |user: &SessionUser, positions: &[UserPositionRow], company_id| {
            Caller { user, positions }.manages(company_id)
        };
        let employee = user(true, false, false);
        // The session acts in company 1; the position in company 2 counts.
        assert!(manages(&employee, &manager_of_2, 2));
        assert!(!manages(&employee, &manager_of_2, 1));
        assert!(!manages(&employee, &rep_of_2, 2));
        assert!(manages(&user(false, true, false), &manager_of_2, 2));
        assert!(manages(&user(false, false, true), &manager_of_2, 2));
        assert!(!manages(&user(false, false, false), &manager_of_2, 2));
    }
}
