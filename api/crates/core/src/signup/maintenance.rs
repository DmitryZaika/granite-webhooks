//! `POST /v1/jobs/subscription-maintenance`.
//!
//! Resumes stuck provisioning and plan invite acceptances, expires abandoned
//! signups (deleting their Stripe customers) and, once a day, reconciles the
//! extra-user quantity of every subscription company. A signup waiting on
//! its bank account (`awaiting_bank_verification`) is never resumed nor
//! expired here: its company exists, and Stripe's verification resumes it.

use super::funnel;
use super::notify;
use super::provision::{self, RunOutcome, record_failure};
use crate::billing::invites::accept as invite_accept;
use crate::billing::stripe::StripeError;
use crate::billing::{self, seats};
use crate::job_runs;
use crate::openapi::ToSchema;
use crate::state::AppState;
use chrono::{Duration, NaiveDateTime};
use lambda_http::tracing;
use serde::Serialize;
use std::time::Instant;

/// No new work starts after this long; calls in flight end inside the API
/// Gateway's 30 s.
const TIME_BUDGET: std::time::Duration = std::time::Duration::from_secs(20);
/// The first wait before a stuck signup is resumed; it doubles with every
/// failed run, up to [`MAX_RESUME_WAIT_MINUTES`].
const RESUME_AFTER_MINUTES: i64 = 10;
const MAX_RESUME_WAIT_MINUTES: i64 = 120;
/// Resumes go on this long after the acceptance, then stop and alert staff.
///
/// Longer than the 24 hours Stripe replays a saved answer (a 500 too) under
/// the same idempotency key, and than the 23 hours after which provisioning
/// looks for an earlier subscription before creating one.
pub const GIVE_UP_AFTER_HOURS: i64 = 26;
const RESUME_BATCH: i64 = 5;
const EXPIRE_BATCH: i64 = 50;
const SEATS_JOB_KEY: &str = "subscription-maintenance:seats";
const SEATS_EVERY_HOURS: i64 = 23;

/// What one run did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
pub struct MaintenanceRun {
    /// `resumed` + `invites_resumed` + `expired` (what the scheduler logs).
    pub processed: usize,
    /// Stuck signups whose provisioning was run again.
    pub resumed: usize,
    /// Resumed signups this run completed.
    pub completed: usize,
    /// Stuck signups given up on (staff were alerted).
    pub given_up: usize,
    /// Abandoned signups expired.
    pub expired: usize,
    /// Stripe customers of expired signups deleted.
    pub customers_deleted: usize,
    /// Subscription companies whose seats were reconciled (0 except on the
    /// daily reconcile).
    pub seat_companies: usize,
    /// Seat reconciles that failed.
    pub seat_errors: usize,
    /// Resumes that failed and Stripe customers that could not be deleted
    /// (retried on a later run).
    pub errors: usize,
    /// Plan invites stuck in acceptance whose switch was run again.
    pub invites_resumed: usize,
    /// Resumed plan invites this run finished (the company is on its plan).
    pub invites_accepted: usize,
    /// Plan invite acceptances given up on (staff were alerted).
    pub invites_given_up: usize,
}

/// One run of the job.
pub async fn run(state: &AppState) -> sqlx::Result<MaintenanceRun> {
    let started = Instant::now();
    let mut outcome = MaintenanceRun::default();
    give_up(state, &mut outcome).await?;
    resume(state, started, &mut outcome).await?;
    give_up_invites(state, &mut outcome).await?;
    resume_invites(state, started, &mut outcome).await?;
    expire(state, started, &mut outcome).await?;
    delete_customers(state, started, &mut outcome).await?;
    reconcile_seats(state, started, &mut outcome).await?;
    outcome.processed = outcome.resumed + outcome.invites_resumed + outcome.expired;
    Ok(outcome)
}

fn out_of_time(started: Instant) -> bool {
    started.elapsed() >= TIME_BUDGET
}

/// How long a stuck signup waits after its last run: 10 minutes, doubling
/// with each failed run, at most 2 hours.
pub fn resume_wait(attempts: u8) -> Duration {
    let doubled = RESUME_AFTER_MINUTES << u32::from(attempts.min(4));
    Duration::minutes(doubled.min(MAX_RESUME_WAIT_MINUTES))
}

/// Provisioning signups accepted more than [`GIVE_UP_AFTER_HOURS`] ago (a
/// bank account's: counted from its verification deadline, as it may have
/// been verified days after the acceptance) stop being resumed: staff get
/// one alert. Without a company the signup expires
/// (its Stripe customer is deleted next); with one, staff finish it by hand.
/// The password hash goes either way.
async fn give_up(state: &AppState, outcome: &mut MaintenanceRun) -> sqlx::Result<()> {
    let now = billing::now();
    let stuck: Vec<(String, Option<i32>, Option<String>)> = sqlx::query_as(
        "SELECT id, company_id, last_error FROM signup_intents
          WHERE status = 'provisioning' AND gave_up_at IS NULL
            AND GREATEST(accepted_at, COALESCE(bank_verification_until, accepted_at)) <= ?
          ORDER BY accepted_at, id LIMIT ?",
    )
    .bind(now - Duration::hours(GIVE_UP_AFTER_HOURS))
    .bind(RESUME_BATCH)
    .fetch_all(&state.pool)
    .await?;
    for (id, company_id, last_error) in stuck {
        let marked = sqlx::query(
            "UPDATE signup_intents
                SET gave_up_at = ?, password_hash = '', updated_at = ?,
                    status = IF(company_id IS NULL, 'expired', status)
              WHERE id = ? AND status = 'provisioning' AND gave_up_at IS NULL",
        )
        .bind(now)
        .bind(now)
        .bind(&id)
        .execute(&state.pool)
        .await?;
        if marked.rows_affected() != 1 {
            continue;
        }
        outcome.given_up += 1;
        let signup = funnel::distinct_id(&id);
        tracing::error!(?company_id, %signup, "signup provisioning given up; staff alerted");
        notify::provisioning_given_up(state, &signup, company_id, last_error.as_deref()).await;
    }
    Ok(())
}

async fn resume(
    state: &AppState,
    started: Instant,
    outcome: &mut MaintenanceRun,
) -> sqlx::Result<()> {
    let now = billing::now();
    let candidates: Vec<(String, u8, Option<i32>, NaiveDateTime)> = sqlx::query_as(
        "SELECT id, provision_attempts, company_id, updated_at FROM signup_intents
          WHERE status = 'provisioning' AND gave_up_at IS NULL AND updated_at <= ?
          ORDER BY updated_at, id LIMIT ?",
    )
    .bind(now - Duration::minutes(RESUME_AFTER_MINUTES))
    .bind(RESUME_BATCH * 4)
    .fetch_all(&state.pool)
    .await?;
    let due = candidates
        .into_iter()
        .filter(|(_, attempts, _, updated_at)| *updated_at + resume_wait(*attempts) <= now)
        .take(usize::try_from(RESUME_BATCH).unwrap_or(5));
    for (id, _, company_id, _) in due {
        if out_of_time(started) {
            break;
        }
        outcome.resumed += 1;
        match provision::run(state, &id).await {
            Ok(RunOutcome::Completed {
                awaiting_bank: false,
                ..
            }) => outcome.completed += 1,
            // Awaiting: the company exists, the bank account still verifies.
            Ok(
                RunOutcome::Completed { .. } | RunOutcome::AlreadyCompleted | RunOutcome::Refused,
            ) => {}
            Err(err) => {
                outcome.errors += 1;
                let signup = funnel::distinct_id(&id);
                tracing::warn!(?company_id, %signup, error = %err, "signup provisioning resume failed");
                record_failure(&state.pool, &id, &err).await?;
            }
        }
    }
    Ok(())
}

/// Plan invites accepted more than [`GIVE_UP_AFTER_HOURS`] ago and still not
/// finished stop being resumed (`gave_up_at`): staff get one alert and finish
/// the switch by hand. The company keeps its legacy plan meanwhile.
async fn give_up_invites(state: &AppState, outcome: &mut MaintenanceRun) -> sqlx::Result<()> {
    let now = billing::now();
    let stuck: Vec<(String, i32, Option<String>)> = sqlx::query_as(
        "SELECT id, company_id, last_error FROM plan_invites
          WHERE status = 'accepting' AND gave_up_at IS NULL AND accepting_at <= ?
          ORDER BY accepting_at, id LIMIT ?",
    )
    .bind(now - Duration::hours(GIVE_UP_AFTER_HOURS))
    .bind(RESUME_BATCH)
    .fetch_all(&state.pool)
    .await?;
    for (id, company_id, last_error) in stuck {
        let marked = sqlx::query(
            "UPDATE plan_invites SET gave_up_at = ?, updated_at = ?
              WHERE id = ? AND status = 'accepting' AND gave_up_at IS NULL",
        )
        .bind(now)
        .bind(now)
        .bind(&id)
        .execute(&state.pool)
        .await?;
        if marked.rows_affected() != 1 {
            continue;
        }
        outcome.invites_given_up += 1;
        tracing::error!(company_id, "plan invite acceptance given up; staff alerted");
        notify::plan_acceptance_given_up(state, company_id, last_error.as_deref()).await;
    }
    Ok(())
}

/// Plan invites stuck in `accepting` (the Manager left, or the switch
/// failed) run again with the signups' wait: 10 minutes, doubling per
/// failed run.
async fn resume_invites(
    state: &AppState,
    started: Instant,
    outcome: &mut MaintenanceRun,
) -> sqlx::Result<()> {
    let now = billing::now();
    let candidates: Vec<(String, u8, i32, NaiveDateTime)> = sqlx::query_as(
        "SELECT id, provision_attempts, company_id, updated_at FROM plan_invites
          WHERE status = 'accepting' AND gave_up_at IS NULL AND updated_at <= ?
          ORDER BY updated_at, id LIMIT ?",
    )
    .bind(now - Duration::minutes(RESUME_AFTER_MINUTES))
    .bind(RESUME_BATCH * 4)
    .fetch_all(&state.pool)
    .await?;
    let due = candidates
        .into_iter()
        .filter(|(_, attempts, _, updated_at)| *updated_at + resume_wait(*attempts) <= now)
        .take(usize::try_from(RESUME_BATCH).unwrap_or(5));
    for (id, _, company_id, _) in due {
        if out_of_time(started) {
            break;
        }
        outcome.invites_resumed += 1;
        match invite_accept::run(state, &id).await {
            Ok(invite_accept::RunOutcome::Accepted { .. }) => outcome.invites_accepted += 1,
            Ok(invite_accept::RunOutcome::AlreadyAccepted | invite_accept::RunOutcome::Busy) => {}
            // The run gave the invite up: no retry, not a failure.
            Err(err) if invite_accept::is_subscription_ended(&err) => {
                tracing::info!(company_id, "plan invite resume stopped: subscription ended");
            }
            Err(err) => {
                outcome.errors += 1;
                tracing::warn!(company_id, error = %err, "plan invite acceptance resume failed");
                invite_accept::record_failure(&state.pool, &id, &err).await?;
            }
        }
    }
    Ok(())
}

/// Abandoned signups expire (password hash and code dropped); their Stripe
/// customers are deleted by [`delete_customers`]. Signups waiting on their
/// bank account are not abandoned.
async fn expire(
    state: &AppState,
    started: Instant,
    outcome: &mut MaintenanceRun,
) -> sqlx::Result<()> {
    let now = billing::now();
    let abandoned: Vec<(String, String)> = sqlx::query_as(
        "SELECT id, CAST(status AS CHAR) AS status FROM signup_intents
          WHERE status IN ('started', 'verified', 'payment_ready') AND expires_at <= ?
          ORDER BY created_at, id LIMIT ?",
    )
    .bind(now)
    .bind(EXPIRE_BATCH)
    .fetch_all(&state.pool)
    .await?;
    for (id, status) in abandoned {
        if out_of_time(started) {
            break;
        }
        // Expired first: a signup being deleted can no longer be accepted.
        let expired = sqlx::query(
            "UPDATE signup_intents
                SET status = 'expired', password_hash = '', code_hash = NULL, updated_at = ?
              WHERE id = ? AND status = ?",
        )
        .bind(billing::now())
        .bind(&id)
        .bind(&status)
        .execute(&state.pool)
        .await?;
        if expired.rows_affected() == 1 {
            outcome.expired += 1;
        }
    }
    Ok(())
}

/// Deletes the Stripe customer of every expired signup without a company
/// that still has one, retrying the ones a refusal or an earlier run could
/// not delete.
async fn delete_customers(
    state: &AppState,
    started: Instant,
    outcome: &mut MaintenanceRun,
) -> sqlx::Result<()> {
    let leftovers: Vec<(String, String)> = sqlx::query_as(
        "SELECT id, stripe_customer_id FROM signup_intents
          WHERE status = 'expired' AND company_id IS NULL AND stripe_customer_id IS NOT NULL
          ORDER BY updated_at, id LIMIT ?",
    )
    .bind(EXPIRE_BATCH)
    .fetch_all(&state.pool)
    .await?;
    for (id, customer) in leftovers {
        if out_of_time(started) {
            break;
        }
        if delete_signup_customer(state, &id, &customer).await? {
            outcome.customers_deleted += 1;
        } else {
            outcome.errors += 1;
        }
    }
    Ok(())
}

/// Deletes an expired signup's Stripe customer and forgets it; false when
/// Stripe failed (the next run retries).
pub(super) async fn delete_signup_customer(
    state: &AppState,
    signup_id: &str,
    customer: &str,
) -> sqlx::Result<bool> {
    match state.stripe.delete_customer(customer).await {
        Ok(_) | Err(StripeError::Missing(_)) => {}
        Err(err) => {
            tracing::warn!(
                signup = %funnel::distinct_id(signup_id),
                stripe_code = err.code(),
                "expired signup's Stripe customer not deleted"
            );
            return Ok(false);
        }
    }
    sqlx::query(
        "UPDATE signup_intents SET stripe_customer_id = NULL
          WHERE id = ? AND status = 'expired' AND company_id IS NULL AND stripe_customer_id = ?",
    )
    .bind(signup_id)
    .bind(customer)
    .execute(&state.pool)
    .await?;
    Ok(true)
}

/// Once per [`SEATS_EVERY_HOURS`] (the `job_runs` row): every subscription
/// company's extra-user quantity, pushed when it differs.
async fn reconcile_seats(
    state: &AppState,
    started: Instant,
    outcome: &mut MaintenanceRun,
) -> sqlx::Result<()> {
    let Some(claim) = job_runs::claim(
        &state.pool,
        SEATS_JOB_KEY,
        Duration::hours(SEATS_EVERY_HOURS),
        billing::now(),
    )
    .await?
    else {
        return Ok(());
    };
    let companies: Vec<i32> = sqlx::query_scalar(
        "SELECT c.id FROM company c
           JOIN company_billing cb ON cb.company_id = c.id
          WHERE c.plan_source = 'subscription'
            AND cb.stripe_subscription_id IS NOT NULL AND cb.stripe_subscription_id <> ''
          ORDER BY c.id",
    )
    .fetch_all(&state.pool)
    .await?;
    for company_id in companies {
        if out_of_time(started) {
            tracing::warn!(
                company_id,
                "seat reconcile ran out of time; the rest waits a day"
            );
            break;
        }
        match seats::sync_seats(state, company_id).await {
            Ok(_) => outcome.seat_companies += 1,
            Err(err) => {
                outcome.seat_errors += 1;
                tracing::warn!(company_id, error = %err.reason(), "seat reconcile failed");
            }
        }
    }
    claim.finish(&state.pool, billing::now()).await
}
