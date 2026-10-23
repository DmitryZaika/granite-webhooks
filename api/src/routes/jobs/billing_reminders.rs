//! `POST /v1/jobs/billing-reminders`: the billing upkeep that runs on every
//! one-minute tick, then the reminder emails.
//!
//! In order, each step failing on its own: a daily prune of old Stripe
//! webhook event ids, a 6-hourly reconciliation of Stripe-billed companies
//! with Stripe, and the reminder emails to managers (payment failed, asked to
//! pay, first charge coming). Port of the CRM's
//! `api.subscription-reminders.process.ts`.

use super::JobCaller;
use crate::billing::CompanyBilling;
use crate::billing::account::{default_card, reconcile_with_stripe};
use crate::billing::gate::settle_company_locked;
use crate::billing::lock::{CompanyLock, LockError};
use crate::billing::reminder_store::MySqlReminderStore;
use crate::billing::reminders::{
    DunningKind, FirstChargeQuote, ReminderEffects, ReminderEmail, ReminderError, ReminderLimits,
    ReminderSendResult, ReminderStep, ReminderTickResult, ReportContext, classify_send_failure,
    describe_card, first_charge_quote_from, process_billing_reminders,
};
use crate::billing::stripe::StripeError;
use crate::error::{ApiResult, ErrorBody};
use crate::job_runs::{self, Claim};
use crate::state::AppState;
use crate::webhooks_client::OutgoingEmail;
use axum::Json;
use axum::extract::State;
use chrono::{Duration, NaiveDateTime};
use lambda_http::tracing;
use serde::Serialize;
use std::sync::LazyLock;
use std::time::Instant;
use utoipa::ToSchema;

static POSTHOG_HTTP: LazyLock<reqwest::Client> = LazyLock::new(reqwest::Client::new);

const PRUNE_KEY: &str = "billing-prune-webhook-events";
const RECONCILE_KEY: &str = "billing-reconcile";
/// Stripe retries a webhook for 3 days; ids older than this are safe to drop.
const WEBHOOK_EVENT_RETENTION_DAYS: u32 = 90;
/// Well under the 10 s other billing writers wait for the company lock, which
/// is held while the email is sent.
const SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const DEFAULT_FROM_ADDRESS: &str = "noreply@granite-manager.com";

/// What one reconciliation with Stripe did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
pub struct ReconcileRun {
    /// Stripe-billed companies re-read from Stripe successfully.
    pub checked: usize,
    /// Companies Stripe could not be read for, or whose billing was busy.
    pub failed: usize,
}

/// What one run did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, ToSchema)]
pub struct BillingRemindersRun {
    /// Reminders claimed this run (each then sent, failed or unknown); what
    /// the scheduler logs.
    pub processed: usize,
    /// Reminder emails delivered to the mail service.
    pub sent: usize,
    /// Reminder emails the mail service refused; retried on a later run, up
    /// to 3 attempts.
    pub failed: usize,
    /// Reminder emails whose outcome is unknown (timeout, 5xx); never resent.
    pub unknown: usize,
    /// Claims found still sending after 15 minutes, now marked unknown.
    pub stuck: usize,
    /// Companies skipped after an error (busy billing lock, database, Stripe);
    /// the next run picks them up again.
    pub company_errors: usize,
    /// The Stripe reconciliation, or null when it was not due this run (it
    /// runs every 6 hours) or Stripe is not configured.
    pub reconciled: Option<ReconcileRun>,
}

const fn run_of(tick: ReminderTickResult, reconciled: Option<ReconcileRun>) -> BillingRemindersRun {
    BillingRemindersRun {
        processed: tick.processed,
        sent: tick.sent,
        failed: tick.failed,
        unknown: tick.unknown,
        stuck: tick.stuck,
        company_errors: tick.company_errors,
        reconciled,
    }
}

/// The lock, Stripe, mail and reporting the reminders use.
struct LiveReminders<'a> {
    state: &'a AppState,
    started: Instant,
    from_address: String,
}

impl ReminderEffects for LiveReminders<'_> {
    type Lock = CompanyLock;

    async fn lock(&self, company_id: i32) -> Result<CompanyLock, ReminderError> {
        CompanyLock::acquire(&self.state.pool, company_id)
            .await
            .map_err(|err| match err {
                LockError::Busy => ReminderError::Busy,
                LockError::Database(err) => ReminderError::Database(err),
            })
    }

    async fn unlock(&self, lock: CompanyLock) {
        lock.release().await;
    }

    async fn settle(
        &self,
        company_id: i32,
        required_since: Option<NaiveDateTime>,
        now: NaiveDateTime,
    ) -> Result<Option<CompanyBilling>, ReminderError> {
        Ok(settle_company_locked(
            &self.state.stripe,
            &self.state.pool,
            company_id,
            required_since,
            now,
        )
        .await?)
    }

    async fn first_charge_quote(
        &self,
        record: &CompanyBilling,
    ) -> Result<Option<FirstChargeQuote>, ReminderError> {
        let stripe = &self.state.stripe;
        let Some(id) = record.stripe_subscription_id.as_deref() else {
            return Ok(None);
        };
        let subscription = match stripe.subscription(id).await {
            Ok(subscription) => subscription,
            Err(StripeError::NotConfigured) => return Ok(None),
            Err(StripeError::Api(_) | StripeError::Missing(_)) => {
                return Err(ReminderError::Stripe("Stripe refused the request".into()));
            }
            Err(StripeError::Request(_) | StripeError::Server(_)) => {
                return Err(ReminderError::Stripe("Stripe request failed".into()));
            }
        };
        let card = default_card(stripe, Some(record)).await;
        Ok(first_charge_quote_from(
            &subscription,
            card.map(|card| describe_card(&card)),
        ))
    }

    async fn send(&self, email: &ReminderEmail) -> ReminderSendResult {
        let message = OutgoingEmail {
            from_name: Some("Granite Manager".into()),
            from_address: self.from_address.clone(),
            to: vec![email.to.clone()],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: email.subject.clone(),
            body: email.html.clone(),
            signature: None,
            reply_to: Vec::new(),
            in_reply_to: None,
            references: Vec::new(),
            attachments: Vec::new(),
            transactional: true,
        };
        match self
            .state
            .webhooks
            .send_notice(&message, &email.text, SEND_TIMEOUT)
            .await
        {
            Ok(message_id) => ReminderSendResult::Sent {
                message_id: Some(message_id).filter(|id| !id.is_empty()),
            },
            Err(failure) => classify_send_failure(&failure),
        }
    }

    async fn report(&self, problem: &str, context: &ReportContext) {
        let fields = (
            context.company_id,
            context.reminder_id,
            context.user_id,
            context.kind.map(DunningKind::as_str),
            context.step.map(ReminderStep::as_str),
            context.error_code.as_deref(),
        );
        if context.error_code.as_deref() == Some("busy") {
            tracing::warn!(?fields, "billing reminder skipped, company busy");
            return;
        }
        tracing::error!(problem, ?fields, "billing reminder problem");
        report_exception("BillingReminderError", problem, context).await;
    }

    fn elapsed(&self) -> std::time::Duration {
        self.started.elapsed()
    }

    fn app_url(&self) -> &str {
        &self.state.app_url
    }
}

/// An `$exception` for Error Tracking: ids, kind, step and error code only.
async fn report_exception(kind: &str, problem: &str, context: &ReportContext) {
    let properties = serde_json::json!({
        "$exception_list": [{
            "type": kind,
            "value": problem,
            "mechanism": { "handled": true, "synthetic": false },
        }],
        "$process_person_profile": false,
        "error_source": "api",
        "job": "billing_reminders",
        "company_id": context.company_id,
        "reminder_id": context.reminder_id,
        "user_id": context.user_id,
        "cycle_kind": context.kind.map(DunningKind::as_str),
        "step": context.step.map(ReminderStep::as_str),
        "error_code": context.error_code,
    });
    crate::posthog::capture(&POSTHOG_HTTP, "$exception", "granite-api", properties).await;
}

/// A step failed: log it and tell Error Tracking, then carry on.
async fn report_step(step: &str, err: &str) {
    tracing::error!(step, error = %err, "billing job step failed");
    let context = ReportContext {
        error_code: Some(step.to_string()),
        ..ReportContext::default()
    };
    report_exception(
        "BillingJobError",
        &format!("{step} failed: {err}"),
        &context,
    )
    .await;
}

/// Ends a claimed run: stamped finished, or given back so the next tick
/// retries.
async fn settle_claim(state: &AppState, claim: Claim, failed: bool) {
    let result = if failed {
        claim.undo(&state.pool).await
    } else {
        claim.finish(&state.pool, crate::billing::now()).await
    };
    if let Err(err) = result {
        tracing::warn!(error = %err, "job run not recorded");
    }
}

/// Drops processed Stripe event ids older than the retention, in a bounded
/// batch. Daily.
async fn prune(state: &AppState, now: NaiveDateTime) {
    let claim = match job_runs::claim(&state.pool, PRUNE_KEY, Duration::days(1), now).await {
        Ok(Some(claim)) => claim,
        Ok(None) => return,
        Err(err) => return report_step("prune_webhook_events", &err.to_string()).await,
    };
    let pruned = sqlx::query(sqlx::AssertSqlSafe(format!(
        "DELETE FROM stripe_webhook_events
          WHERE processed_at < UTC_TIMESTAMP() - INTERVAL {WEBHOOK_EVENT_RETENTION_DAYS} DAY
          LIMIT 5000"
    )))
    .execute(&state.pool)
    .await;
    if let Err(err) = &pruned {
        report_step("prune_webhook_events", &err.to_string()).await;
    }
    settle_claim(state, claim, pruned.is_err()).await;
}

/// Re-reads Stripe-billed companies from Stripe. Every 6 hours.
async fn reconcile(state: &AppState, now: NaiveDateTime) -> Option<ReconcileRun> {
    let claim = match job_runs::claim(&state.pool, RECONCILE_KEY, Duration::hours(6), now).await {
        Ok(Some(claim)) => claim,
        Ok(None) => return None,
        Err(err) => {
            report_step("reconcile", &err.to_string()).await;
            return None;
        }
    };
    match reconcile_with_stripe(&state.stripe, &state.pool).await {
        Ok(done) => {
            settle_claim(state, claim, false).await;
            done.map(|done| ReconcileRun {
                checked: done.checked,
                failed: done.failed,
            })
        }
        Err(err) => {
            report_step("reconcile", &err.to_string()).await;
            settle_claim(state, claim, true).await;
            None
        }
    }
}

/// Run the billing upkeep and reminder emails
///
/// Called by the `time-triggered` Lambda every minute with
/// `Authorization: <LAMBDA_KEY>`. Three steps run in order and each fails on
/// its own: it prunes Stripe webhook event ids older than 90 days (at most
/// once a day), reconciles Stripe-billed companies with Stripe (at most every
/// 6 hours), then sends the billing reminder emails to each company's
/// Managers. Reminders are a failed renewal or a first payment asked for
/// (day 0, three days left, paused tomorrow, paused, settled) and one notice
/// 7 days before the first charge of a self-serve subscription. Each
/// recipient gets each reminder once; an email whose outcome is unknown is
/// never resent. A run sends at most 20 emails and starts no new company
/// after 40 seconds. Reminders are skipped while the mail service is not
/// configured.
#[utoipa::path(
    post,
    path = "/v1/jobs/billing-reminders",
    operation_id = "run_billing_reminders",
    tag = "jobs",
    responses(
        (status = 200, description = "What the run did", body = BillingRemindersRun),
        (status = 401, description = "Missing or wrong job key", body = ErrorBody),
    ),
    security(("job_key" = []))
)]
pub async fn run(
    State(state): State<AppState>,
    _caller: JobCaller,
) -> ApiResult<Json<BillingRemindersRun>> {
    let started = Instant::now();
    let now = crate::billing::now();
    prune(&state, now).await;
    let reconciled = reconcile(&state, now).await;
    if !state.webhooks.is_enabled() {
        // A send could never succeed and would turn every claim into unknown.
        tracing::warn!("billing reminders skipped: the mail service is not configured");
        return Ok(Json(run_of(ReminderTickResult::default(), reconciled)));
    }
    let effects = LiveReminders {
        state: &state,
        started,
        from_address: std::env::var("BILLING_EMAIL_FROM_ADDRESS")
            .ok()
            .map(|address| address.trim().to_string())
            .filter(|address| !address.is_empty())
            .unwrap_or_else(|| DEFAULT_FROM_ADDRESS.to_string()),
    };
    let tick = process_billing_reminders(
        &MySqlReminderStore(&state.pool),
        &effects,
        now,
        ReminderLimits::default(),
    )
    .await;
    Ok(Json(run_of(tick, reconciled)))
}
