//! `POST /v1/jobs/subscription-maintenance`.
//!
//! Resumes stuck provisioning, expires abandoned signups (deleting their
//! Stripe customers) and, once a day, reconciles the extra-user quantity of
//! every subscription company.

use super::funnel;
use super::intent::BANK_VERIFICATION_DAYS;
use super::provision::{self, RunOutcome, record_failure};
use crate::billing::stripe::StripeError;
use crate::billing::{self, seats};
use crate::job_runs;
use crate::state::AppState;
use chrono::Duration;
use lambda_http::tracing;
use serde::Serialize;
use std::time::Instant;
use utoipa::ToSchema;

/// No new work starts after this long.
const TIME_BUDGET: std::time::Duration = std::time::Duration::from_secs(40);
/// A `provisioning` signup untouched this long is resumed.
const RESUME_AFTER_MINUTES: i64 = 10;
/// Resumes stop after this many failed runs.
pub const MAX_PROVISION_ATTEMPTS: u8 = 10;
const RESUME_BATCH: i64 = 5;
const EXPIRE_BATCH: i64 = 50;
const SEATS_JOB_KEY: &str = "subscription-maintenance:seats";
const SEATS_EVERY_HOURS: i64 = 23;

/// What one run did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
pub struct MaintenanceRun {
    /// `resumed` + `expired` (what the scheduler logs).
    pub processed: usize,
    /// Stuck signups whose provisioning was run again.
    pub resumed: usize,
    /// Resumed signups this run completed.
    pub completed: usize,
    /// Abandoned signups expired.
    pub expired: usize,
    /// Subscription companies whose seats were reconciled (0 except on the
    /// daily reconcile).
    pub seat_companies: usize,
    /// Seat reconciles that failed.
    pub seat_errors: usize,
    /// Resumes that failed and abandoned signups whose Stripe customer could
    /// not be deleted (retried on a later run).
    pub errors: usize,
}

/// One run of the job.
pub async fn run(state: &AppState) -> sqlx::Result<MaintenanceRun> {
    let started = Instant::now();
    let mut outcome = MaintenanceRun::default();
    resume(state, started, &mut outcome).await?;
    expire(state, started, &mut outcome).await?;
    reconcile_seats(state, started, &mut outcome).await?;
    outcome.processed = outcome.resumed + outcome.expired;
    Ok(outcome)
}

fn out_of_time(started: Instant) -> bool {
    started.elapsed() >= TIME_BUDGET
}

async fn resume(
    state: &AppState,
    started: Instant,
    outcome: &mut MaintenanceRun,
) -> sqlx::Result<()> {
    let now = billing::now();
    let stuck: Vec<(String, u8, Option<i32>)> = sqlx::query_as(
        "SELECT id, provision_attempts, company_id FROM signup_intents
          WHERE status = 'provisioning' AND updated_at <= ? AND provision_attempts < ?
          ORDER BY updated_at, id LIMIT ?",
    )
    .bind(now - Duration::minutes(RESUME_AFTER_MINUTES))
    .bind(MAX_PROVISION_ATTEMPTS)
    .bind(RESUME_BATCH)
    .fetch_all(&state.pool)
    .await?;
    for (id, attempts, company_id) in stuck {
        if out_of_time(started) {
            break;
        }
        outcome.resumed += 1;
        match provision::run(state, &id).await {
            Ok(RunOutcome::Completed { .. }) => outcome.completed += 1,
            Ok(RunOutcome::AlreadyCompleted | RunOutcome::Refused) => {}
            Err(err) => {
                outcome.errors += 1;
                let signup = funnel::distinct_id(&id);
                tracing::warn!(?company_id, %signup, error = %err, "signup provisioning resume failed");
                record_failure(&state.pool, &id, &err).await?;
                if attempts.saturating_add(1) >= MAX_PROVISION_ATTEMPTS {
                    tracing::warn!(
                        ?company_id,
                        %signup,
                        "signup provisioning gave up after {MAX_PROVISION_ATTEMPTS} attempts; finish it by hand"
                    );
                }
            }
        }
    }
    Ok(())
}

async fn expire(
    state: &AppState,
    started: Instant,
    outcome: &mut MaintenanceRun,
) -> sqlx::Result<()> {
    let now = billing::now();
    let abandoned: Vec<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT id, CAST(status AS CHAR) AS status, stripe_customer_id FROM signup_intents
          WHERE (status IN ('started', 'verified', 'payment_ready') AND expires_at <= ?)
             OR (status = 'awaiting_bank_verification' AND updated_at <= ?)
          ORDER BY created_at, id LIMIT ?",
    )
    .bind(now)
    .bind(now - Duration::days(BANK_VERIFICATION_DAYS))
    .bind(EXPIRE_BATCH)
    .fetch_all(&state.pool)
    .await?;
    for (id, status, customer) in abandoned {
        if out_of_time(started) {
            break;
        }
        if let Some(customer) = customer.as_deref().filter(|customer| !customer.is_empty()) {
            match state.stripe.delete_customer(customer).await {
                Ok(_) | Err(StripeError::Missing(_)) => {}
                Err(err) => {
                    outcome.errors += 1;
                    tracing::warn!(
                        signup = %funnel::distinct_id(&id),
                        error = %err,
                        "abandoned signup's Stripe customer not deleted"
                    );
                    continue;
                }
            }
        }
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
                tracing::warn!(company_id, error = %err, "seat reconcile failed");
            }
        }
    }
    claim.finish(&state.pool, billing::now()).await
}
